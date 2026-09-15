//! Codespaces inside the control plane.
//!
//! Two halves of one contract:
//!
//! * [`CodespacesLink`] — what a link's finish does inside the GitHub
//!   account beyond what the OAuth callback already proved: the private
//!   `flyco-sessions` repository every codespace is created on, and the
//!   devcontainer inside it that boots the session image.
//! * [`bootstrap`] — the public endpoint a *running* codespace calls for
//!   its daemon configuration.
//!
//! # Why bootstrap exists
//!
//! A codespace is given no per-machine secret and no config file: GitHub's
//! repository secrets are shared by every codespace on the repository, so
//! one session's daemon configuration in a secret would be read by the next
//! session that starts. The machine instead fetches its `flycod`
//! configuration here on `postStart`, and the pair GitHub injects into
//! every codespace — `CODESPACE_NAME` and a `GITHUB_TOKEN` scoped to the
//! environment repository — is the whole credential: the name names the
//! machine row, and the token is proved by asking GitHub whether it can
//! read the private environment repository that machine's account
//! provisions on. What it is handed back is the sealed TOML the provision
//! stored on `machines.bootstrap_enc`.

use core::future::Future;

use flyco_core::{
    ClientEvent, CodespacesBootstrap, CodespacesBootstrapRequest, InterruptedReason, MachineState,
    ProviderCredentials, RepoSlug, SessionState,
};
use flyco_provider::codespaces::EnvRepo;
use flyco_provider::{LiveTransport, ProviderError};
use skyzen::routing::{CreateRouteNode as _, Route, RouteNode, Routes as _};
use skyzen::utils::{Json, State};
use skyzen_services::Db;

use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::extract::Headers;
use crate::github::{GithubCall, GithubClient, GithubError, GithubOauth as _, GithubToken};
use crate::problem::Outcome;
use crate::rooms::Rooms;
use crate::{machines, provisioning, sessions};

/// The GitHub-side work a Codespaces link's finish performs.
///
/// Behind a trait for the reason [`GithubOauth`](crate::github::GithubOauth)
/// is: the finish handler is otherwise untestable, because it cannot run
/// without a GitHub account to create a repository in.
pub trait CodespacesLink: Send + Sync + Clone + 'static {
    /// Creates or verifies the account's private environment repository
    /// and writes the devcontainer that boots sessions into it.
    ///
    /// `owner` is the login the sign-in resolved to, and `devcontainer_json`
    /// the rendered document — both supplied because they are the control
    /// plane's facts rather than the driver's.
    ///
    /// The returned future is deliberately not `Send`, for the reason
    /// [`CloudLink::prepare`](crate::clouds::CloudLink::prepare)'s is not.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the repository cannot be read,
    /// created or written, or an existing repository of this name is
    /// public.
    fn ensure_environment(
        &self,
        token: &GithubToken,
        owner: &str,
        devcontainer_json: &str,
    ) -> impl Future<Output = Result<EnvRepo, ProviderError>>;
}

/// The production implementation, which talks to `api.github.com`.
#[derive(Debug, Clone, Copy, Default)]
pub struct LiveCodespaces;

impl LiveCodespaces {
    /// Creates it.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl CodespacesLink for LiveCodespaces {
    async fn ensure_environment(
        &self,
        token: &GithubToken,
        owner: &str,
        devcontainer_json: &str,
    ) -> Result<EnvRepo, ProviderError> {
        flyco_provider::codespaces::ensure_environment(
            &LiveTransport::new(),
            &token.access_token,
            owner,
            devcontainer_json,
        )
        .await
    }
}

/// The Codespaces link seam the router carries.
///
/// An enum rather than a type parameter, for the reason every other vendor
/// client here is one: `#[skyzen::openapi]` cannot annotate a generic
/// handler.
#[derive(Debug, Clone)]
pub enum Codespaces {
    /// Talks to `api.github.com`.
    Live(LiveCodespaces),
    /// Answers without a network, for tests.
    #[cfg(test)]
    Fake(crate::testing::TestCodespaces),
}

impl Default for Codespaces {
    fn default() -> Self {
        Self::Live(LiveCodespaces::new())
    }
}

impl CodespacesLink for Codespaces {
    async fn ensure_environment(
        &self,
        token: &GithubToken,
        owner: &str,
        devcontainer_json: &str,
    ) -> Result<EnvRepo, ProviderError> {
        match self {
            Self::Live(live) => {
                live.ensure_environment(token, owner, devcontainer_json)
                    .await
            }
            #[cfg(test)]
            Self::Fake(fake) => {
                fake.ensure_environment(token, owner, devcontainer_json)
                    .await
            }
        }
    }
}

impl Codespaces {
    /// What GitHub reports for the codespace a machine's native id names,
    /// as a lifecycle state — `None` when GitHub no longer holds it at all.
    ///
    /// The reconcile's whole read. The state is mapped through the driver's
    /// own table, so a codespace that `Failed` reads destroyed here the
    /// same way it does everywhere else.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the account is not a Codespaces
    /// account or GitHub cannot be asked.
    pub async fn codespace_state(
        &self,
        account: &provisioning::LinkedAccount,
        native_id: &str,
    ) -> Result<Option<MachineState>, ProviderError> {
        match self {
            Self::Live(_) => provisioning::codespace_state(account, native_id).await,
            #[cfg(test)]
            Self::Fake(fake) => Ok(fake
                .reported_state
                .map(flyco_provider::codespaces::machine_state)),
        }
    }

    /// Deletes a codespace GitHub still holds in a dead state.
    ///
    /// What the reconcile does with a `Failed` codespace before it releases
    /// the row: `Failed` still bills storage until it is deleted, and the
    /// delete is flyco's to issue because nothing else will.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the account is not a Codespaces
    /// account or the delete is refused.
    pub async fn destroy_codespace(
        &self,
        account: &provisioning::LinkedAccount,
        machine: &flyco_provider::Machine,
    ) -> Result<(), ProviderError> {
        match self {
            Self::Live(_) => provisioning::destroy_codespace(account, machine).await,
            #[cfg(test)]
            Self::Fake(fake) => {
                fake.destroyed
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
        }
    }
}

/// `POST /v1/providers/codespaces/bootstrap` — a running codespace asks for
/// its session's daemon configuration.
///
/// Public because it cannot be anything else: the caller is a machine
/// GitHub just built, which holds no flyco credential. What it presents is
/// the `GITHUB_TOKEN` injected into it, and that is checked against the
/// environment repository rather than against a flyco store — a token that
/// can read `owner/flyco-sessions` is one GitHub minted for a codespace on
/// it.
#[skyzen::openapi]
pub async fn bootstrap(
    State(config): State<ApiConfig>,
    State(github): State<GithubClient>,
    headers: Headers,
    Json(request): Json<CodespacesBootstrapRequest>,
    db: Db,
) -> Outcome<Json<CodespacesBootstrap>> {
    serve(
        &db,
        &config,
        &github,
        headers.bearer(),
        &request.codespace_name,
    )
    .await
    .map(Json)
    .into()
}

/// Answers the bootstrap call.
///
/// The order of the checks is the order the facts arrive: the machine row
/// is needed before there is an account to check the token against, and
/// the token is checked before the payload is opened — so a denial never
/// touches the sealed configuration, and a machine row that does not exist
/// yet is the `404` the codespace's own retry is written for.
async fn serve(
    db: &Db,
    config: &ApiConfig,
    github: &GithubClient,
    bearer: Option<&str>,
    codespace_name: &str,
) -> Result<CodespacesBootstrap, ApiError> {
    let access_token = bearer.ok_or(ApiError::MissingCredential)?;

    let Some(row) = machines::codespace(db, codespace_name).await? else {
        return Err(ApiError::CodespacesMachineUnknown);
    };
    // The account the machine provisions through, still linked — an
    // unlinked one is `ProviderAccountNotFound`, and its codespace has no
    // business fetching a configuration that account paid for.
    let account = provisioning::account(db, config, row.user, row.account)
        .await
        .map_err(|error| match error {
            ApiError::ProviderAccountNotFound => ApiError::CodespacesBootstrapDenied,
            other => other,
        })?;
    let ProviderCredentials::Codespaces { env_repo, .. } = account.credentials() else {
        return Err(ApiError::CorruptRecord(
            "a codespaces machine is provisioned through a non-codespaces account",
        ));
    };
    let slug: RepoSlug = env_repo.parse().map_err(|_| {
        ApiError::CorruptRecord("this account's environment repository is not owner/name")
    })?;

    // The authentication of this endpoint, in full: GitHub answers whether
    // the presented token can read the private environment repository this
    // account provisions on. A codespace's injected `GITHUB_TOKEN` is
    // scoped to exactly that repository; anything else's is not, and a
    // 404/401/403 from the lookup is a denial rather than GitHub's outage.
    github
        .get_repo(
            &GithubToken {
                access_token: access_token.to_owned(),
            },
            &slug,
        )
        .await
        .map_err(|error| match error {
            GithubError::Status {
                call: GithubCall::Repository,
                status: 401 | 403 | 404,
                ..
            } => ApiError::CodespacesBootstrapDenied,
            other => ApiError::from(other),
        })?;

    let Some(sealed) = row.bootstrap_enc else {
        return Err(ApiError::MachineNotReady);
    };
    let config_toml = config.token_cipher().open(&sealed)?;
    tracing::info!(
        codespace = codespace_name,
        "served a codespace its configuration"
    );
    Ok(CodespacesBootstrap { config_toml })
}

/// The bootstrap route, mounted with the other public paths.
///
/// Public because the caller holds no flyco credential — see [`bootstrap`].
/// It authenticates with what GitHub injected into the machine instead.
pub fn public_routes() -> Vec<RouteNode> {
    Route::new(("/v1/providers/codespaces/bootstrap".post(bootstrap),)).into_route_nodes()
}

/// Reconciles every codespace flyco believes it holds against what GitHub
/// reports, once per scheduled sweep.
///
/// A codespace's state changes underneath flyco in ways nothing reports:
/// GitHub stops it when its own idle timeout expires, a user deletes it
/// from github.com, and the only truth about either is asking. The states
/// *flyco* moves a machine through are written when they happen, so the
/// sweep only ever moves a row *down* a state — a `running` row whose
/// codespace reads `Available` is its own write standing, not news.
///
/// A row that cannot be asked about is warned and skipped rather than
/// failing the sweep: a GitHub outage or an account unlinked mid-sweep is
/// retried when this runs next, and the machines behind it still get
/// reconciled.
///
/// # Errors
///
/// Returns [`ApiError`] if the held set cannot be read.
pub async fn reconcile(
    db: &Db,
    config: &ApiConfig,
    rooms: &Rooms,
    codespaces: &Codespaces,
) -> Result<(), ApiError> {
    for held in machines::held_codespaces(db).await? {
        if let Err(error) = reconcile_one(db, config, rooms, codespaces, &held).await {
            tracing::warn!(machine = %held.machine, %error, "a codespace could not be reconciled");
        }
    }
    Ok(())
}

/// Reconciles one held codespace's row — and the session on it — against
/// what GitHub reports for it.
async fn reconcile_one(
    db: &Db,
    config: &ApiConfig,
    rooms: &Rooms,
    codespaces: &Codespaces,
    held: &machines::HeldCodespace,
) -> Result<(), ApiError> {
    let account = provisioning::account(db, config, held.user_id, held.account).await?;
    let reported = codespaces
        .codespace_state(&account, &held.native_id)
        .await
        .map_err(|error| ApiError::Provisioning(error.to_string()))?;

    match reported {
        // GitHub holds nothing under this name. Nothing is left to bill
        // against, and the session's machine is never coming back — the
        // next thing it is asked provisions a fresh one.
        None => {
            machines::mark_lost(db, held.machine).await?;
            machine_lost(db, rooms, held).await
        }
        // GitHub holds it dead: `Failed` past starting, or `Deleted`,
        // `Archived` or `Moved` without flyco issuing it. It still bills
        // storage until it is deleted, and the delete is flyco's to issue
        // because nothing else will — `Gone` means somebody beat the sweep
        // to it, which is the same outcome.
        Some(MachineState::Destroyed) => {
            match codespaces
                .destroy_codespace(&account, &held.as_provider_machine())
                .await
            {
                Ok(()) | Err(ProviderError::Gone(_)) => {}
                Err(error) => return Err(ApiError::Provisioning(error.to_string())),
            }
            machines::mark_lost(db, held.machine).await?;
            machine_lost(db, rooms, held).await
        }
        // GitHub suspended it on its own idle clock. The disk is kept and
        // keeps billing; the compute meter ends where the stop landed. An
        // active session on it is interrupted as suspended — the daemon is
        // not coming back on its own — while a paused or already
        // interrupted one keeps its own reason.
        Some(MachineState::Deallocated) => {
            machines::suspended(db, held.machine).await?;
            if held.session_state == SessionState::Active {
                sessions::interrupted(db, held.session, InterruptedReason::Suspended).await?;
                rooms
                    .broadcast(
                        db,
                        held.session,
                        &ClientEvent::SessionStateChanged {
                            state: SessionState::Interrupted,
                        },
                    )
                    .await?;
            }
            Ok(())
        }
        // Somebody started it — the codespace is one click on github.com —
        // or a start flyco issued landed between this row's read and the
        // answer. The compute meter restarts; the session's move back is
        // the daemon's attach to make, because only that proves the machine
        // is serving again.
        Some(MachineState::Running) => {
            machines::note_running(db, held.machine).await?;
            Ok(())
        }
        // Still moving between states — the next sweep sees where it
        // settled.
        Some(MachineState::Provisioning) => Ok(()),
    }
}

/// Tells a session its machine ceased to exist, and its watchers.
///
/// The two loss answers of the reconcile share this write: the row is
/// already released by the time it runs, and a paused session is the one
/// state [`sessions::machine_lost`] deliberately leaves alone — its wake
/// reads the row and provisions around the gap itself.
async fn machine_lost(
    db: &Db,
    rooms: &Rooms,
    held: &machines::HeldCodespace,
) -> Result<(), ApiError> {
    sessions::machine_lost(db, held.session).await?;
    if held.session_state != SessionState::Paused {
        rooms
            .broadcast(
                db,
                held.session,
                &ClientEvent::SessionStateChanged {
                    state: SessionState::Interrupted,
                },
            )
            .await?;
    }
    Ok(())
}
