//! Router assembly and the handlers that are not part of the OAuth flow.

use flyco_core::workdir::{
    DirectoryListing, FileContent, WorkdirDiff, WorkdirReply, WorkdirRequest,
};
use flyco_core::{
    AgentMachineView, ApiKeyId, ApiKeySummary, ApprovalDecision, ApprovalId, ApprovalState,
    ApprovalView, BranchName, BudgetConfig, BudgetView, ClientEvent, ControlToDaemon, CreateApiKey,
    CreateSession, CreatedApiKey, CurrentUser, DaemonToken, DecideApproval, DesktopInputRequest,
    DesktopTakeoverRequest, EnvDocument, HarnessFeature, HarnessObservation, HarnessSessionView,
    HarnessTui, InterruptedReason, MAX_SESSION_TITLE_CHARS, MachineCatalogEntry, MachineOrigin,
    MachineSpec, MessageOrigin, ModelChoice, ProvisioningStage, RepoAddedBy, RepoSelection,
    RepoSlug, RepoStatus, ReportModels, ReportProvisioningStage, ReportSpotNotice,
    ReportStartupFailure, ReportStopping, ReportUsage, ResizeMachine, RunShell, SendMessage,
    SessionActivity, SessionDetail, SessionId, SessionRepo, SessionState, SessionSummary,
    TerminalInput, TerminalSize, TurnPage, UpdateEnv, UpdateMe, UpdateSession, UsageLimitHit,
    UserId,
    wire::{ApprovalPayload, DaemonAttach, DaemonAttached, DaemonFrames},
};
use flyco_provider::host::{HostAttach, HostFrames};
use serde::{Deserialize, Serialize};
use skyzen::extract::Query;
use skyzen::middleware::ErrorHandlingMiddleware;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Router, Routes as _};
use skyzen::static_files::EmbeddedStaticDir;
use skyzen::utils::{Bytes, Json, State};
use skyzen::{HttpError as _, Response};
use skyzen_services::{Db, Kv, Queue, Storage};

use crate::authenticator::FlycoAuthenticator;
use crate::clouds::Clouds;
use crate::codespaces::Codespaces;
use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::extract::{Headers, path_id, path_segment};
use crate::github::{GithubClient, GithubOauth};
use crate::host_room::HostAttachResponse;
use crate::middleware::{DaemonSession, RequireAuth, RequireDaemon};
use crate::problem::Outcome;
use crate::provisioning_queue::{self, ProvisioningJob};
use crate::respond::{Accepted, Created, NoContent};
use crate::rooms::{HostRooms, Rooms, UserStreams};
use crate::vendors::Vendors;
use crate::{
    agents_md, api_keys, approvals, claude_oauth, cli, codespaces, codex_oauth, daemon_tokens, env,
    handoffs, harness_accounts, hosts, idempotency, machines, mcp, memory, oauth, observations,
    problem, provider_accounts, provider_oauth, provisioning, push, relay, releases, repos,
    responses, session_repos, sessions, skills, transcripts, turns, usage_limits, users, webhooks,
    workdirs,
};
use flyco_core::wire::EventPage;

/// Health probe response.
#[derive(Debug, Serialize, skyzen::ToSchema)]
struct Health {
    /// Wire protocol version this control plane speaks to daemons.
    wire_protocol_version: u32,
}

/// Reports that the control plane is up, and which wire protocol version it
/// speaks to session daemons.
#[skyzen::openapi]
async fn healthz() -> Json<Health> {
    Json(Health {
        wire_protocol_version: flyco_core::WIRE_PROTOCOL_VERSION,
    })
}

/// Serves one allowlisted execution-plane binary or checksum from R2.
async fn get_release_artifact(params: Params, storage: Storage) -> Outcome<Response> {
    read_release_artifact(&params, &storage).await.into()
}

async fn read_release_artifact(params: &Params, storage: &Storage) -> Result<Response, ApiError> {
    let name = path_segment(params, "artifact")?;
    let artifact = releases::get(storage, &name)
        .await?
        .ok_or(ApiError::ReleaseArtifactNotFound)?;
    let mut response = Response::new(skyzen::Body::from(artifact.body));
    response.headers_mut().insert(
        skyzen::header::CONTENT_TYPE,
        skyzen::header::HeaderValue::from_static(artifact.content_type),
    );
    response.headers_mut().insert(
        skyzen::header::CACHE_CONTROL,
        skyzen::header::HeaderValue::from_static("public, max-age=60"),
    );
    Ok(response)
}

/// Describes the account behind the presented credential.
#[skyzen::openapi]
async fn me(State(user): State<CurrentUser>) -> Json<CurrentUser> {
    Json(user)
}

/// Updates the caller's account settings.
#[skyzen::openapi]
async fn update_me(
    State(user): State<CurrentUser>,
    Json(update): Json<UpdateMe>,
    db: Db,
) -> Outcome<Json<CurrentUser>> {
    apply_update_me(&user, update, &db).await.into()
}

async fn apply_update_me(
    user: &CurrentUser,
    update: UpdateMe,
    db: &Db,
) -> Result<Json<CurrentUser>, ApiError> {
    if let Some(cap) = update.session_cap {
        users::set_session_cap(db, user.id, cap).await?;
    }

    users::find(db, user.id)
        .await?
        .map(Json)
        .ok_or(ApiError::CorruptRecord(
            "the row for an authenticated user disappeared mid-request",
        ))
}

/// Mints an API key, returning the plaintext key exactly once.
#[skyzen::openapi]
async fn create_api_key(
    State(user): State<CurrentUser>,
    Json(request): Json<CreateApiKey>,
    db: Db,
) -> Outcome<Json<CreatedApiKey>> {
    api_keys::create(&db, user.id, request.label)
        .await
        .inspect(|key| tracing::info!(label = %key.label, "minted an API key"))
        .map(Json)
        .into()
}

/// Lists the caller's API keys, without their secrets.
#[skyzen::openapi]
async fn list_api_keys(
    State(user): State<CurrentUser>,
    db: Db,
) -> Outcome<Json<Vec<ApiKeySummary>>> {
    api_keys::list(&db, user.id).await.map(Json).into()
}

/// Revokes one of the caller's API keys.
#[skyzen::openapi]
async fn revoke_api_key(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Outcome<NoContent> {
    revoke(&user, &params, &db).await.into()
}

async fn revoke(user: &CurrentUser, params: &Params, db: &Db) -> Result<NoContent, ApiError> {
    api_keys::revoke(db, user.id, path_id::<ApiKeyId>(params, "id")?).await?;
    Ok(NoContent)
}

/// Starts a session: reserves its budget and its machine, and queues the
/// machine to be built. No machine exists yet.
///
/// The choice of machine is validated against the named account's own
/// catalog *before* anything is written, so a machine the account cannot
/// deploy is refused here rather than accepted and then failed minutes later
/// by a queue consumer the caller is no longer watching.
#[skyzen::openapi]
#[expect(
    clippy::too_many_arguments,
    reason = "one session creation reaches every service the control plane has"
)]
async fn create_session(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(github): State<GithubClient>,
    headers: Headers,
    Json(request): Json<CreateSession>,
    rooms: Rooms,
    queue: Queue,
    db: Db,
    kv: Kv,
) -> Outcome<Created<Json<SessionDetail>>> {
    start_session(
        &user,
        request,
        headers.get("idempotency-key"),
        &config,
        &github,
        &rooms,
        &queue,
        &db,
        &kv,
    )
    .await
    .into()
}

#[expect(
    clippy::too_many_arguments,
    reason = "one session creation reaches every service the control plane has"
)]
async fn start_session(
    user: &CurrentUser,
    request: CreateSession,
    idempotency_key: Option<&str>,
    config: &ApiConfig,
    github: &GithubClient,
    rooms: &Rooms,
    queue: &Queue,
    db: &Db,
    kv: &Kv,
) -> Result<Created<Json<SessionDetail>>, ApiError> {
    // Pulled off before the request is consumed: a handoff source changes
    // what creation commits the session to, so it travels alongside the
    // resolution rather than through it.
    let source = request.source.clone();
    let resolved = resolve_request(user, request, config, github, db, kv, queue).await?;

    // Claimed only once everything that could refuse the request has
    // refused it: a key bound to a request that was never going to be
    // accepted would poison a corrected retry under the same key.
    let claim = match idempotency_key {
        Some(key) => match idempotency::claim(db, user.id, key).await? {
            idempotency::Claim::Committed(session) => {
                return sessions::find(db, user.id, session)
                    .await
                    .map(|detail| Created(Json(detail)));
            }
            idempotency::Claim::InFlight => return Err(ApiError::IdempotencyInFlight),
            idempotency::Claim::Fresh(claim) => Some(claim),
        },
        None => None,
    };

    let session = match sessions::create(
        db,
        user.session_cap,
        sessions::Opening {
            user: user.id,
            title: &flyco_core::excerpt(&resolved.prompt, MAX_SESSION_TITLE_CHARS),
            harness: resolved.harness,
            repos: &resolved.repos,
            machine_origin: resolved.machine_origin,
            budget: resolved.budget,
            model: &resolved.model,
            permission_mode: resolved.permission_mode,
            computer_use: resolved.computer_use,
        },
    )
    .await
    {
        Ok(session) => session,
        Err(error) => {
            if let Some(claim) = claim {
                claim.release(db).await?;
            }
            return Err(error);
        }
    };
    if let Some(claim) = claim {
        claim.record(db, session.summary.id).await?;
    }
    let id = session.summary.id;
    // The pending-handoff row lands before anything else that could fail:
    // a session created with a source but no row would sit in `provisioning`
    // with no route able to complete it.
    if let Some(flyco_core::SessionSource::LocalHandoff(handoff)) = &source {
        fail_session_on(
            db,
            rooms,
            id,
            "the session's handoff could not be recorded",
            handoffs::create_pending(db, id, handoff).await,
        )
        .await?;
    }
    let machine = machines::reserve(db, id, resolved.account.id, &resolved.spec).await?;

    // The prompt is posted to the session's room *before* the machine is
    // queued, so the agent's first instruction is durable before anything
    // asynchronous can go wrong. No daemon exists yet — the room holds it
    // in its mailbox and hands it over on the daemon's first attach.
    //
    // Both steps fail the session rather than return early: a session row
    // whose prompt never reached its room, or whose job never reached the
    // queue, would sit in `provisioning` waiting for something that is never
    // going to happen.
    fail_session_on(
        db,
        rooms,
        id,
        "the session's room would not take its first prompt",
        rooms
            .command(
                db,
                id,
                &ControlToDaemon::UserMessage {
                    text: resolved.prompt,
                    origin: MessageOrigin::User,
                },
            )
            .await,
    )
    .await?;
    // A handoff stays out of the queue until `handoff/complete` has
    // verified its patch and transcript in storage; enqueueing here would
    // let the daemon boot a machine against objects still in flight.
    if source.is_none() {
        fail_session_on(
            db,
            rooms,
            id,
            "the provisioning queue would not accept this session's job",
            provisioning_queue::enqueue(queue, ProvisioningJob::first(id, machine)).await,
        )
        .await?;
    }

    tracing::info!(
        repos = ?resolved.repos.iter().map(|repo| repo.slug.as_str()).collect::<Vec<_>>(),
        harness = ?resolved.harness,
        model = %resolved.model.model,
        effort = ?resolved.model.effort,
        machine_type = %resolved.spec.machine_type,
        region = %resolved.spec.region,
        spot = resolved.spec.spot,
        "opened a session and queued its machine"
    );
    Ok(Created(Json(session)))
}

/// Everything `start_session` settles before a row is written: the request's
/// claims each checked against what the control plane can verify, resolved to
/// the values the session will actually carry.
struct ResolvedSession {
    /// The trimmed first prompt.
    prompt: String,
    /// The repositories the session works in, in the order the caller
    /// chose them, each branch already resolved.
    repos: Vec<sessions::RepoOpening>,
    /// The harness the session runs.
    harness: flyco_core::HarnessKind,
    /// How the harness treats its own confirmations — `None` stores no
    /// opinion and the product default applies.
    permission_mode: Option<flyco_core::PermissionMode>,
    /// Who chose the machine: the user, or the automatic picker.
    machine_origin: MachineOrigin,
    /// The budget the session enforces.
    budget: BudgetConfig,
    /// The model the session opens on, off the account's own list.
    model: ModelChoice,
    /// Whether the session gets a screen.
    computer_use: bool,
    /// The provider account the machine bills to.
    account: provisioning::LinkedAccount,
    /// What the machine is.
    spec: MachineSpec,
}

/// The resolution half of `start_session`: every check a create request can
/// fail, done before anything is written.
///
/// Consumes the request: `machine` and `model` are the caller's *claims*,
/// replaced by what the resolution produced — a machine picked when none
/// was named, a model defaulted off the account's list.
async fn resolve_request(
    user: &CurrentUser,
    request: CreateSession,
    config: &ApiConfig,
    github: &GithubClient,
    db: &Db,
    kv: &Kv,
    queue: &Queue,
) -> Result<ResolvedSession, ApiError> {
    let prompt = request.prompt.trim();
    if prompt.is_empty() {
        return Err(ApiError::EmptyMessage);
    }
    let repos = resolve_repos(github, config, db, user, &request.repos).await?;

    let machine_origin = if request.machine.is_some() {
        MachineOrigin::User
    } else {
        MachineOrigin::Auto
    };
    let choice = match request.machine {
        Some(choice) => choice,
        None => {
            machines::automatic(db, config, github, kv, queue, user.id, request.spot, None)
                .await?
                .choice
        }
    };
    // Resolved against the account's own model list before anything is
    // written, and for the same reason the machine choice is: a model the
    // harness does not offer is refused here rather than accepted and then
    // discovered by a daemon the caller is no longer watching. A request
    // that named none opens on the list's default, so the session row
    // always states what it runs.
    let models = harness_accounts::models(db, user.id, request.harness).await?;
    let model = match request.model {
        Some(model) => {
            model.validate(&models).map_err(ApiError::InvalidModel)?;
            model
        }
        None => ModelChoice::default_of(&models),
    };
    // A harness whose ids carry the effort — Devin's `swe-2-max` — has no
    // "unset" level: the choice must name one before a daemon can build
    // the id, so an unstated effort takes the row's own default here.
    let model = if request.harness.effort_is_fused() {
        model.with_default_effort(&models)
    } else {
        model
    };
    let account =
        provisioning::account(db, config, github, user.id, choice.provider_account).await?;
    // Checked against the cached catalog, which is what the picker showed;
    // the queue re-asks the provider when it actually builds the machine.
    let entry = machines::deployable(db, config, github, kv, user.id, &choice).await?;
    let spec = MachineSpec {
        provider: account.kind(),
        machine_type: choice.machine_type,
        // Taken from the entry rather than from the request, which
        // `deployable` has just proved agree: the catalog is the authority
        // on what a type is, and the request is a claim about it.
        runtime: entry.runtime,
        region: choice.region,
        // Spot only where the catalog quoted it; a type the spot pool cannot
        // fund is held on demand rather than refused at provisioning.
        spot: choice.spot && entry.pricing.offers_spot(),
        disk_gib: choice.disk_gib,
    };
    Ok(ResolvedSession {
        prompt: prompt.to_owned(),
        repos,
        harness: request.harness,
        permission_mode: request.permission_mode,
        machine_origin,
        budget: BudgetConfig::new(request.budget_limit).map_err(|_| ApiError::InvalidBudget)?,
        model,
        computer_use: request.computer_use,
        account,
        spec,
    })
}

/// Settles which repositories a session works in, and which branch each
/// checks out, before any row is written.
///
/// A session must always know its branches — the machine has to clone
/// *something*, and the header renders `repo · branch` (docs/ux.md §9.1) —
/// so a selection that names none has the repository's default read from
/// GitHub here rather than left for the provisioning queue to guess at
/// minutes later.
///
/// The same call establishes that the caller's stored GitHub authorization
/// can actually reach the repositories. Checking it here as well as in the
/// queue is deliberate: this is where the user is watching, and being told
/// to sign in again in the moment they pressed send is worth a great deal
/// more than the same sentence attached to a session that failed while they
/// were elsewhere.
async fn resolve_repos(
    github: &GithubClient,
    config: &ApiConfig,
    db: &Db,
    user: &CurrentUser,
    selections: &[flyco_core::RepoSelection],
) -> Result<Vec<sessions::RepoOpening>, ApiError> {
    use crate::github::{GithubOauth as _, REPO_SCOPE};

    if selections.is_empty() {
        return Err(ApiError::NoRepositories);
    }
    if selections.len() > flyco_core::MAX_SESSION_REPOS {
        return Err(ApiError::SessionRepoCapReached {
            cap: flyco_core::MAX_SESSION_REPOS,
        });
    }
    let mut slugs = Vec::with_capacity(selections.len());
    for selection in selections {
        let slug = selection
            .repo
            .parse::<RepoSlug>()
            .map_err(|_| ApiError::InvalidRepo(selection.repo.clone()))?;
        // A request that names one repository twice is refused rather than
        // doubled: the checkout would carry it once anyway, and silently
        // dropping a selection the user made would leave them thinking a
        // second worktree exists.
        if slugs.contains(&slug) {
            return Err(ApiError::RepoAlreadyAttached { repo: slug });
        }
        slugs.push(slug);
    }
    let token = users::github_token(db, config, github, user.id).await?;
    if !github.current_user(&token).await?.grants_repo_scope() {
        return Err(ApiError::GithubTokenInsufficient {
            scope: REPO_SCOPE,
            repo: slugs[0].clone(),
        });
    }
    let mut repos = Vec::with_capacity(slugs.len());
    for (slug, selection) in slugs.into_iter().zip(selections) {
        let branch = resolve_branch(github, &token, &slug, selection.branch.as_deref()).await?;
        repos.push(sessions::RepoOpening { slug, branch });
    }
    Ok(repos)
}

/// Settles which branch one checkout works on.
///
/// The token's scope was established by the caller, once for the whole
/// selection; what is left per repository is the name itself — the
/// caller's, parsed, or the repository's default read from GitHub.
async fn resolve_branch(
    github: &GithubClient,
    token: &crate::github::GithubToken,
    repo: &RepoSlug,
    requested: Option<&str>,
) -> Result<BranchName, ApiError> {
    use crate::github::GithubOauth as _;

    match requested {
        Some(name) => name
            .trim()
            .parse()
            .map_err(
                |error: flyco_core::BranchNameError| ApiError::InvalidBranch {
                    name: name.to_owned(),
                    reason: error.to_string(),
                },
            ),
        None => Ok(github.get_repo(token, repo).await?.default_branch),
    }
}

/// Marks a freshly opened session failed when one of its hand-off steps
/// refused, and passes that refusal on.
async fn fail_session_on<T>(
    db: &Db,
    rooms: &Rooms,
    id: SessionId,
    reason: &str,
    step: Result<T, ApiError>,
) -> Result<T, ApiError> {
    match step {
        Ok(value) => Ok(value),
        Err(error) => {
            sessions::fail(db, rooms, id, reason).await?;
            Err(error)
        }
    }
}

/// The verified per-harness feature matrix.
#[skyzen::openapi]
async fn list_harness_features() -> Outcome<Json<Vec<HarnessFeature>>> {
    Ok(Json(flyco_core::matrix())).into()
}

/// Lists the caller's sessions, newest first.
#[skyzen::openapi]
async fn list_sessions(
    State(user): State<CurrentUser>,
    db: Db,
) -> Outcome<Json<Vec<SessionSummary>>> {
    sessions::list(&db, user.id).await.map(Json).into()
}

/// Describes one of the caller's sessions, including its budget.
#[skyzen::openapi]
async fn get_session(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Outcome<Json<SessionDetail>> {
    read_session(&user, &params, &db).await.into()
}

async fn read_session(
    user: &CurrentUser,
    params: &Params,
    db: &Db,
) -> Result<Json<SessionDetail>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    sessions::find(db, user.id, id).await.map(Json)
}

/// Changes what one of the caller's sessions is called, what it may spend,
/// what it runs on, or any combination of the three.
///
/// The title opens as the excerpt of the prompt the session was created
/// with; this is how it becomes something the user chose. The budget limit
/// is the one thing that releases a session paused on an exhausted budget,
/// and raising it past the spend both puts the session back to
/// [`SessionState::Active`] and tells its daemon to carry on — the daemon
/// stopped accepting work when the pause reached it and nothing in the
/// database can lift that. The model is recorded and then sent to the
/// session's room, which echoes it into the transcript and hands it to the
/// harness mid-conversation. The permission mode is recorded and announced
/// on the same path: Claude applies it to the live query and Codex applies
/// it from the next turn.
#[skyzen::openapi]
async fn update_session(
    State(user): State<CurrentUser>,
    params: Params,
    Json(update): Json<UpdateSession>,
    rooms: Rooms,
    db: Db,
) -> Outcome<Json<SessionDetail>> {
    apply_session_update(&user, &params, &update, &rooms, &db)
        .await
        .into()
}

async fn apply_session_update(
    user: &CurrentUser,
    params: &Params,
    update: &UpdateSession,
    rooms: &Rooms,
    db: &Db,
) -> Result<Json<SessionDetail>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;

    let renamed = match &update.title {
        Some(title) => Some(sessions::rename(db, user.id, id, title).await?),
        None => None,
    };

    let remodelled = match &update.model {
        Some(choice) => {
            // Validated against the harness the *session* runs, read off
            // the session rather than taken from the caller: a body naming
            // a Codex model for a Claude session is a stale picker, and it
            // is refused with the model it named rather than accepted.
            let session = sessions::find(db, user.id, id).await?;
            let models = harness_accounts::models(db, user.id, session.summary.harness).await?;
            choice.validate(&models).map_err(ApiError::InvalidModel)?;
            // On a harness whose ids carry the effort an unstated level
            // names the row's default — the daemon fuses it back into the
            // id it sends.
            let mut choice = choice.clone();
            if session.summary.harness.effort_is_fused() {
                choice = choice.with_default_effort(&models);
            }
            let session = sessions::set_model(db, user.id, id, &choice).await?;
            // Recorded first, announced second: the room's echo is what the
            // transcript shows, and an echo the database had not yet agreed
            // with would be a line about a change that could still fail.
            rooms
                .command(
                    db,
                    id,
                    &ControlToDaemon::SetModel {
                        model: choice.clone(),
                    },
                )
                .await?;
            tracing::info!(session = %id, model = %choice.model, effort = ?choice.effort, "a session was put on another model");
            Some(session)
        }
        None => None,
    };

    let remoded = match update.permission_mode {
        Some(mode) => {
            let session = sessions::set_mode(db, user.id, id, mode).await?;
            // Recorded first, announced second, on the same terms as the
            // model: the room's echo is what the transcript shows, and an
            // echo the database had not yet agreed with would be a line
            // about a change that could still fail. Held for a daemon that
            // is away — `survives_a_disconnect` — because it is what the
            // next machine comes up under.
            rooms
                .command(db, id, &ControlToDaemon::SetPermissionMode { mode })
                .await?;
            tracing::info!(session = %id, ?mode, "a session was put under another permission mode");
            Some(session)
        }
        None => None,
    };

    let screened = match update.computer_use {
        Some(enabled) => {
            let session = sessions::set_computer_use(db, user.id, id, enabled).await?;
            // Recorded first, announced second — the same terms as the
            // mode, and the same reason a daemon that is away is held the
            // command: it is what the next machine comes up with.
            rooms
                .command(db, id, &ControlToDaemon::SetComputerUse { enabled })
                .await?;
            tracing::info!(session = %id, enabled, "a session's screen was turned {}", if enabled { "on" } else { "off" });
            Some(session)
        }
        None => None,
    };

    let rebudgeted = match update.budget_limit {
        Some(limit) => {
            let raise = sessions::set_budget_limit(db, user.id, id, limit).await?;
            if raise.resumed {
                rooms
                    .command(db, id, &ControlToDaemon::BudgetRaised { limit })
                    .await?;
                tracing::info!(session = %id, %limit, "a raised budget released a paused session");
            }
            Some(raise.session)
        }
        None => None,
    };

    // The last answer wins where more than one was asked for: each read
    // follows the write before it, so the latest is the one carrying every
    // change made above it. None at all means a body that named nothing to
    // do, which is the caller's bug rather than a session that happens to
    // be unchanged.
    rebudgeted
        .or(screened)
        .or(remoded)
        .or(remodelled)
        .or(renamed)
        .map(Json)
        .ok_or(ApiError::EmptyUpdate)
}

/// Narrows a manual archive.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
struct ArchiveQuery {
    /// Required when the working tree is dirty: the caller has seen the
    /// warning and still wants the disk released without keeping the
    /// uncommitted work.
    #[serde(default)]
    discard_uncommitted: bool,
}

/// Archives a session, releasing its execution environment for good.
#[skyzen::openapi]
#[expect(
    clippy::too_many_arguments,
    reason = "an archive reaches the session, the rooms, and the providers \
              its machine may need"
)]
async fn archive_session(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(github): State<GithubClient>,
    Query(query): Query<ArchiveQuery>,
    params: Params,
    rooms: Rooms,
    hosts: HostRooms,
    db: Db,
) -> Outcome<Json<SessionDetail>> {
    end_session(
        user.id,
        &config,
        &github,
        &params,
        &rooms,
        &hosts,
        &db,
        ArchiveKind::Manual {
            discard_uncommitted: query.discard_uncommitted,
        },
    )
    .await
    .into()
}

/// How a session is being archived.
pub(crate) enum ArchiveKind {
    /// The user asked. A dirty tree requires confirmation and is discarded.
    Manual { discard_uncommitted: bool },
    /// The session sat idle. Uncommitted work is snapshotted first.
    Automatic,
}

#[expect(
    clippy::too_many_arguments,
    reason = "an archive reaches the session, the rooms, and the providers \
              its machine may need"
)]
async fn end_session(
    user: UserId,
    config: &ApiConfig,
    github: &GithubClient,
    params: &Params,
    rooms: &Rooms,
    hosts: &HostRooms,
    db: &Db,
    kind: ArchiveKind,
) -> Result<Json<SessionDetail>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    archive(db, config, github, rooms, hosts, user, id, kind)
        .await
        .map(Json)
}

/// Archives one session.
///
/// # Errors
///
/// Returns [`ApiError::DirtyArchive`] when a manual archive would discard
/// uncommitted work without confirmation, [`ApiError::RepoStatusUnknown`]
/// when an active session has never reported its tree, or
/// [`ApiError::InvalidTransition`] when the lifecycle forbids the move.
#[expect(
    clippy::too_many_arguments,
    reason = "an archive reaches the session, the rooms, and the providers \
              its machine may need"
)]
pub(crate) async fn archive(
    db: &Db,
    config: &ApiConfig,
    github: &impl GithubOauth,
    rooms: &Rooms,
    hosts: &HostRooms,
    user: UserId,
    id: SessionId,
    kind: ArchiveKind,
) -> Result<SessionDetail, ApiError> {
    let state = sessions::state_of(db, user, id).await?;
    state
        .transition(SessionState::Archived)
        .map_err(|error| ApiError::InvalidTransition {
            from: error.from,
            to: error.to,
        })?;

    let preserve_workdir = match kind {
        ArchiveKind::Automatic => true,
        ArchiveKind::Manual {
            discard_uncommitted,
        } => {
            confirm_manual_archive(rooms, id, state, discard_uncommitted).await?;
            false
        }
    };

    rooms
        .command(db, id, &ControlToDaemon::Archive { preserve_workdir })
        .await?;
    machines::destroy_for_archive(db, config, github, hosts, user, id).await?;
    sessions::transition(db, user, id, SessionState::Archived).await
}

async fn confirm_manual_archive(
    rooms: &Rooms,
    id: SessionId,
    state: SessionState,
    discard_uncommitted: bool,
) -> Result<(), ApiError> {
    if !matches!(
        state,
        SessionState::Active | SessionState::Paused | SessionState::Interrupted
    ) {
        return Ok(());
    }
    let status = rooms.repo_status(id).await?;
    if status.dirty() && !discard_uncommitted {
        return Err(ApiError::DirtyArchive {
            summary: status.dirty_summary(),
        });
    }
    Ok(())
}

/// Archives every session that has sat idle for a week.
///
/// # Errors
///
/// Returns [`ApiError`] if listing or archiving a session fails.
pub async fn archive_idle(
    db: &Db,
    config: &ApiConfig,
    github: &impl GithubOauth,
    rooms: &Rooms,
    hosts: &HostRooms,
    at_unix: u64,
) -> Result<(), ApiError> {
    for idle in sessions::idle_since(db, at_unix).await? {
        archive(
            db,
            config,
            github,
            rooms,
            hosts,
            idle.user_id,
            idle.id,
            ArchiveKind::Automatic,
        )
        .await?;
    }
    Ok(())
}

/// Suspends the machine of every session idle past
/// [`SUSPEND_AFTER_IDLE_SECS`].
///
/// A codespace gets this from GitHub's own idle clock; every other machine
/// runs — and bills, or holds a user's host resources — until flyco stops
/// it, so this sweep is that stop. The interruption is the same
/// [`InterruptedReason::Suspended`] a reconcile writes: the disk is kept,
/// the next message or resume starts the machine again by name, and the
/// daemon's own attach returns the session to `active`. One failure does
/// not stop the rest — a session whose stop was refused is left active and
/// billing until the next pass, which is the honest answer to a provider
/// that could not be asked.
///
/// # Errors
///
/// Returns [`ApiError`] if the list itself fails.
pub async fn suspend_idle(
    db: &Db,
    config: &ApiConfig,
    github: &impl GithubOauth,
    rooms: &Rooms,
    hosts: &HostRooms,
    at_unix: u64,
) -> Result<(), ApiError> {
    for idle in sessions::suspendable(db, at_unix).await? {
        if let Err(error) = suspend(db, config, github, rooms, hosts, &idle).await {
            tracing::warn!(
                session = %idle.id,
                %error,
                "an idle session's machine was not suspended"
            );
        }
    }
    Ok(())
}

/// Stops one idle session's machine and marks the session suspended.
///
/// The provider call comes first: a refusal leaves everything as it was —
/// still active, still running — and the next pass asks again. A machine
/// already deallocated is the gap a pass cut short leaves, or a Stop
/// button the user pressed themselves: [`machines::stop_for_flyco`]
/// answers `false` for it and the session write is still owed, so this is
/// where the interruption is recorded either way.
async fn suspend(
    db: &Db,
    config: &ApiConfig,
    github: &impl GithubOauth,
    rooms: &Rooms,
    hosts: &HostRooms,
    idle: &sessions::IdleSession,
) -> Result<(), ApiError> {
    machines::stop_for_flyco(db, config, github, hosts, idle.user_id, idle.id).await?;
    sessions::interrupted(db, idle.id, InterruptedReason::Suspended).await?;
    rooms
        .broadcast(
            db,
            idle.id,
            &ClientEvent::SessionStateChanged {
                state: SessionState::Interrupted,
            },
        )
        .await
}

/// Reports a session's budget, recomputed from its spend ledger.
#[skyzen::openapi]
async fn get_session_budget(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Outcome<Json<BudgetView>> {
    read_budget(&user, &params, &db).await.into()
}

async fn read_budget(
    user: &CurrentUser,
    params: &Params,
    db: &Db,
) -> Result<Json<BudgetView>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    Ok(Json(sessions::find(db, user.id, id).await?.budget))
}

/// Narrows a listing of approvals.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
struct ApprovalFilter {
    /// Only approvals raised by this session.
    session: Option<SessionId>,
    /// Only approvals in this state — `pending` is the useful one.
    state: Option<ApprovalState>,
}

/// Lists approvals raised against the caller's sessions, newest first.
#[skyzen::openapi]
async fn list_approvals(
    State(user): State<CurrentUser>,
    Query(filter): Query<ApprovalFilter>,
    db: Db,
) -> Outcome<Json<Vec<ApprovalView>>> {
    approvals::list(&db, user.id, filter.session, filter.state)
        .await
        .map(Json)
        .into()
}

/// Records the caller's decision on a pending approval.
#[skyzen::openapi]
#[expect(
    clippy::too_many_arguments,
    reason = "an approved license-bound resize is a decision and a machine \
              change in one request"
)]
async fn decide_approval(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(github): State<GithubClient>,
    params: Params,
    Json(request): Json<DecideApproval>,
    rooms: Rooms,
    hosts: HostRooms,
    db: Db,
    kv: Kv,
    queue: Queue,
) -> Outcome<Json<ApprovalView>> {
    settle_approval(
        &user, &params, request, &config, &github, &rooms, &hosts, &db, &kv, &queue,
    )
    .await
    .into()
}

#[expect(
    clippy::too_many_arguments,
    reason = "an approved license-bound resize is a decision, a machine \
              change and possibly a wake in one request"
)]
async fn settle_approval(
    user: &CurrentUser,
    params: &Params,
    request: DecideApproval,
    config: &ApiConfig,
    github: &GithubClient,
    rooms: &Rooms,
    hosts: &HostRooms,
    db: &Db,
    kv: &Kv,
    queue: &Queue,
) -> Result<Json<ApprovalView>, ApiError> {
    let id = path_id::<ApprovalId>(params, "id")?;

    // D1 first, and only then the room. The decision is durable before it is
    // announced, so a daemon can never act on an approval the database would
    // still call pending — and the conditional `UPDATE` in `approvals` is
    // what makes "decided exactly once" true, so a second caller never gets
    // this far to announce a second decision.
    let decided = approvals::decide(db, user.id, id, request.decision).await?;

    let announced = rooms
        .command(
            db,
            decided.session,
            &ControlToDaemon::ApprovalDecision {
                id,
                decision: request.decision,
            },
        )
        .await;
    if let Err(error) = announced {
        // The decision stands whatever the relay did with it: the daemon
        // re-reads pending approvals when it reconnects, so a room that is
        // asleep or evicted costs a round trip, not the answer.
        tracing::warn!(%error, session = %decided.session, "a decided approval did not reach its room");
    }

    // Deciding is activity, and on a suspended session it is also the wake:
    // the daemon re-reads pending approvals on attach, so the machine has
    // to be coming back for the answer to reach it.
    sessions::touch(db, decided.session).await?;
    wake_interrupted(db, queue, decided.session).await;

    perform_approved(
        &decided,
        request.decision,
        user.id,
        config,
        github,
        rooms,
        hosts,
        db,
        kv,
    )
    .await?;

    tracing::info!(decision = ?request.decision, "decided an approval");
    Ok(Json(decided))
}

/// Carries out the approval the user just allowed, where allowing it *is* the
/// action.
///
/// Most approvals unblock something the daemon is holding — a tool call
/// waiting on a permission — and the daemon performs them. A license-bound
/// resize has nobody waiting: the agent was told the request is pending and
/// went on with its turn, and the machine is the control plane's to change.
/// So the decision and the resize happen in the same request, in that order,
/// and a resize that fails answers with why rather than reporting success on
/// a machine that did not change. The approval stays approved either way —
/// the user did allow it — and the retry is the ordinary resize.
#[expect(
    clippy::too_many_arguments,
    reason = "the resize an approval performs needs everything a resize does"
)]
async fn perform_approved(
    approval: &ApprovalView,
    decision: ApprovalDecision,
    user: UserId,
    config: &ApiConfig,
    github: &GithubClient,
    rooms: &Rooms,
    hosts: &HostRooms,
    db: &Db,
    kv: &Kv,
) -> Result<(), ApiError> {
    if decision != ApprovalDecision::Approved {
        return Ok(());
    }
    match &approval.payload {
        ApprovalPayload::MachineResizeLicenseBound { machine_type, .. } => {
            tracing::info!(
                session = %approval.session,
                machine_type,
                "the user approved a license-bound resize; moving the machine"
            );
            machines::resize(
                db,
                config,
                github,
                kv,
                rooms,
                hosts,
                user,
                approval.session,
                machine_type,
            )
            .await
        }
        ApprovalPayload::RepoAdd { repo, branch, .. } => {
            // The payload stored the resolved branch — `settle_repo_add`
            // filled it at raise — so an approved add asks GitHub nothing.
            let slug = repo
                .parse::<RepoSlug>()
                .map_err(|_| ApiError::InvalidRepo(repo.clone()))?;
            let branch = branch
                .as_deref()
                .ok_or(ApiError::CorruptRecord(
                    "an approved repository add names no branch",
                ))?
                .parse::<BranchName>()
                .map_err(|_| {
                    ApiError::CorruptRecord("an approved repository add names an invalid branch")
                })?;
            let attached = match session_repos::attach(
                db,
                approval.session,
                &slug,
                &branch,
                RepoAddedBy::Agent,
            )
            .await
            {
                Ok(repo) => repo,
                // The user added the same repository themselves between the
                // raise and the decision — the checkout is already a fact,
                // so the approval has nothing left to record.
                Err(ApiError::RepoAlreadyAttached { .. }) => return Ok(()),
                Err(error) => return Err(error),
            };
            tracing::info!(
                session = %approval.session,
                repo = %attached.slug,
                dir = %attached.dir,
                "the user approved a repository the agent asked for; cloning it"
            );
            announce_add_repo(rooms, db, approval.session, &attached).await;
            Ok(())
        }
        _ => Ok(()),
    }
}

// ── Pairing a session with its daemon ──

/// Mints the daemon token that pairs a session with its `flycod`.
///
/// Returned once and stored only as a hash; minting again replaces the
/// previous token, so re-pairing revokes the daemon that held it.
#[skyzen::openapi]
async fn create_daemon_token(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Outcome<Json<DaemonToken>> {
    pair_daemon(&user, &params, &db).await.into()
}

async fn pair_daemon(
    user: &CurrentUser,
    params: &Params,
    db: &Db,
) -> Result<Json<DaemonToken>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    let token = daemon_tokens::issue(db, user.id, id).await?;
    tracing::info!(session = %id, "minted a daemon token");
    Ok(Json(token))
}

// ── The live relay ──

/// Attaches a session's daemon to its room.
///
/// Authenticated by the session's `fd_` token rather than by a user
/// credential, so it sits outside [`RequireAuth`].
#[skyzen::openapi]
async fn attach_daemon(
    params: Params,
    headers: Headers,
    rooms: Rooms,
    db: Db,
    Json(attach): Json<DaemonAttach>,
) -> Outcome<Json<DaemonAttached>> {
    daemon_attach(&params, &headers, &rooms, &db, attach)
        .await
        .into()
}

async fn daemon_attach(
    params: &Params,
    headers: &Headers,
    rooms: &Rooms,
    db: &Db,
    attach: DaemonAttach,
) -> Result<Json<DaemonAttached>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    relay::attach_daemon(rooms, db, id, headers.bearer(), attach)
        .await
        .map(Json)
}

/// Query of the daemon command stream.
#[derive(Debug, Deserialize, skyzen::ToSchema)]
struct CommandEpoch {
    /// The attach this stream serves.
    epoch: u64,
}

/// Opens a session daemon's command stream: the room's SSE stream, handed
/// through still running.
#[skyzen::openapi]
async fn open_daemon_commands(
    params: Params,
    headers: Headers,
    Query(query): Query<CommandEpoch>,
    rooms: Rooms,
    db: Db,
) -> Outcome<Response> {
    daemon_commands(&params, &headers, &query, &rooms, &db)
        .await
        .into()
}

async fn daemon_commands(
    params: &Params,
    headers: &Headers,
    query: &CommandEpoch,
    rooms: &Rooms,
    db: &Db,
) -> Result<Response, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    relay::daemon_commands(rooms, db, id, headers.bearer(), query.epoch).await
}

/// Accepts one batch of a session daemon's outbound frames.
#[skyzen::openapi]
async fn post_daemon_frames(
    params: Params,
    headers: Headers,
    rooms: Rooms,
    db: Db,
    Json(batch): Json<DaemonFrames>,
) -> Outcome<NoContent> {
    daemon_frames(&params, &headers, &rooms, &db, batch)
        .await
        .into()
}

async fn daemon_frames(
    params: &Params,
    headers: &Headers,
    rooms: &Rooms,
    db: &Db,
    batch: DaemonFrames,
) -> Result<NoContent, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    relay::daemon_frames(rooms, db, id, headers.bearer(), batch).await
}

/// Attaches an enrolled machine to its host room.
///
/// Authenticated by the host's own `fh_` token rather than by a user
/// credential, exactly as a session daemon's relay is, so it sits outside
/// [`RequireAuth`].
#[skyzen::openapi]
async fn attach_host(
    params: Params,
    headers: Headers,
    rooms: HostRooms,
    db: Db,
    Json(attach): Json<HostAttach>,
) -> Outcome<Json<HostAttachResponse>> {
    host_attach(&params, &headers, &rooms, &db, attach)
        .await
        .into()
}

async fn host_attach(
    params: &Params,
    headers: &Headers,
    rooms: &HostRooms,
    db: &Db,
    attach: HostAttach,
) -> Result<Json<HostAttachResponse>, ApiError> {
    let id = path_id::<flyco_core::HostId>(params, "id")?;
    relay::attach_host(rooms, db, id, headers.bearer(), attach)
        .await
        .map(Json)
}

/// Opens an enrolled machine's command stream.
#[skyzen::openapi]
async fn open_host_commands(
    params: Params,
    headers: Headers,
    Query(query): Query<CommandEpoch>,
    rooms: HostRooms,
    db: Db,
) -> Outcome<Response> {
    host_commands(&params, &headers, &query, &rooms, &db)
        .await
        .into()
}

async fn host_commands(
    params: &Params,
    headers: &Headers,
    query: &CommandEpoch,
    rooms: &HostRooms,
    db: &Db,
) -> Result<Response, ApiError> {
    let id = path_id::<flyco_core::HostId>(params, "id")?;
    relay::host_commands(rooms, db, id, headers.bearer(), query.epoch).await
}

/// Accepts one batch of an enrolled machine's outbound frames.
#[skyzen::openapi]
async fn post_host_frames(
    params: Params,
    headers: Headers,
    rooms: HostRooms,
    db: Db,
    Json(batch): Json<HostFrames>,
) -> Outcome<NoContent> {
    host_frames(&params, &headers, &rooms, &db, batch)
        .await
        .into()
}

async fn host_frames(
    params: &Params,
    headers: &Headers,
    rooms: &HostRooms,
    db: &Db,
    batch: HostFrames,
) -> Result<NoContent, ApiError> {
    let id = path_id::<flyco_core::HostId>(params, "id")?;
    relay::host_frames(rooms, db, id, headers.bearer(), batch).await
}

/// Query of the user event stream.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
struct UserStreamCursor {
    /// Resume strictly after this position in the stream's buffer. A
    /// reconnecting client passes the `Last-Event-ID` it last saw.
    after: Option<u64>,
    /// Emit only this session's events, when the caller is following one.
    session: Option<SessionId>,
}

/// The one stream every session event rides.
///
/// `GET /v1/events` multiplexes every session the caller owns onto one
/// SSE connection; each event is a [`SessionEvent`](flyco_core::wire::SessionEvent)
/// envelope carrying the session it belongs to. Replays are `Last-Event-ID`
/// deep: past the buffer, the client refills from a session's own
/// `events?after=` history.
#[skyzen::openapi]
async fn open_event_stream(
    State(user): State<CurrentUser>,
    Query(cursor): Query<UserStreamCursor>,
    headers: Headers,
    streams: UserStreams,
) -> Outcome<Response> {
    user_stream(&user, &cursor, &headers, &streams).await.into()
}

async fn user_stream(
    user: &CurrentUser,
    cursor: &UserStreamCursor,
    headers: &Headers,
    streams: &UserStreams,
) -> Result<Response, ApiError> {
    // A browser cannot set `Last-Event-ID` on a fetch the way EventSource
    // can on a reconnect, so the header and the query are the same cursor
    // and the query wins when both are sent. Absent is a first connect:
    // the stream opens live and the session's own event pages carry the
    // past.
    let after = cursor
        .after
        .or_else(|| headers.get("last-event-id").and_then(|v| v.parse().ok()));
    streams.stream(user.id, after, cursor.session).await
}

/// Where in a session's event stream to resume reading.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
struct EventCursor {
    /// Return events strictly after this position.
    after: Option<u64>,
}

/// Reads a session's recorded event tail.
///
/// The catch-up path a browser takes before — and alongside — its live
/// stream: replay from the last position it saw, then follow the relay.
#[skyzen::openapi]
async fn get_session_events(
    State(user): State<CurrentUser>,
    params: Params,
    Query(cursor): Query<EventCursor>,
    rooms: Rooms,
    db: Db,
) -> Outcome<Json<EventPage>> {
    read_events(&user, &params, cursor.after.unwrap_or(0), &rooms, &db)
        .await
        .into()
}

async fn read_events(
    user: &CurrentUser,
    params: &Params,
    after: u64,
    rooms: &Rooms,
    db: &Db,
) -> Result<Json<EventPage>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    // A room cannot reach D1, so ownership is settled here, before the
    // Worker will address the room at all.
    if !sessions::is_owned_by(db, user.id, id).await? {
        return Err(ApiError::SessionNotFound);
    }
    rooms.events(id, after).await.map(Json)
}

// ── Driving a session ──

/// Sends a message to a session's agent.
///
/// Answers `202`: the message is handed to the session's room and the reply
/// arrives on the relay, not in this response.
#[skyzen::openapi]
async fn send_message(
    State(user): State<CurrentUser>,
    params: Params,
    Json(message): Json<SendMessage>,
    rooms: Rooms,
    queue: Queue,
    db: Db,
) -> Outcome<Accepted> {
    say(&user, &params, message, &rooms, &queue, &db)
        .await
        .into()
}

async fn say(
    user: &CurrentUser,
    params: &Params,
    message: SendMessage,
    rooms: &Rooms,
    queue: &Queue,
    db: &Db,
) -> Result<Accepted, ApiError> {
    if message.text.trim().is_empty() {
        return Err(ApiError::EmptyMessage);
    }
    // A session waiting out a spent plan window keeps a usable composer —
    // the user has something to say and the wait can be days long — and what
    // they type is held against the pause rather than handed to a harness
    // that would refuse it. It is sent as the continuation the moment the
    // window turns over, in place of flyco's canned nudge (docs/ux.md §9.8).
    // Tried before `drive`, because `drive` refuses a paused session.
    let queued = queue_while_waiting(user, params, &message.text, db).await?;
    if queued {
        return Ok(Accepted);
    }
    let id = path_id::<SessionId>(params, "id")?;
    // A user message is the one command the room holds for a daemon that
    // is not there: while the machine is still being built or coming back
    // from reclamation, the room's command log carries it to the next
    // attach — which is what the composer means by not gating prompts on
    // a live machine. Everything else a session can be in refuses.
    let state = sessions::state_of(db, user.id, id).await?;
    match state {
        SessionState::Provisioning | SessionState::Active | SessionState::Interrupted => {}
        state => return Err(ApiError::SessionNotActive { state }),
    }
    rooms
        .command(
            db,
            id,
            &ControlToDaemon::UserMessage {
                text: message.text.clone(),
                origin: MessageOrigin::User,
            },
        )
        .await?;

    // The user has spoken and no turn has started yet, which is exactly
    // `Idle` (docs/ux.md §6). Written after the room took the message, so a
    // session whose command was refused is never recorded as having heard
    // one — and `drive` has already proved the session is the caller's.
    sessions::record_activity(db, id, SessionActivity::Idle).await?;

    // A message to an interrupted session is also what wakes its machine:
    // a codespace GitHub suspended starts again, one that was deleted is
    // provisioned around. Best-effort by place — the message is already
    // the room's, and every message after this one asks again.
    if state == SessionState::Interrupted {
        wake_interrupted(db, queue, id).await;
    }
    Ok(Accepted)
}

/// Wakes the machine of a session somebody just spoke to, when it still
/// can be woken.
///
/// [`InterruptedReason::Suspended`] and [`InterruptedReason::MachineLost`]
/// are the interruptions a message answers: the first starts the codespace
/// GitHub stopped, the second provisions around one that is not coming
/// back. A spot reclamation needs nothing here — its recovery was enqueued
/// where the reclaim was reported — and any other reason is not one speech
/// fixes. Nothing returns an error: `say` has already accepted the message,
/// and a failure here is retried by the next one.
async fn wake_interrupted(db: &Db, queue: &Queue, session: SessionId) {
    match wake_interrupted_job(db, session).await {
        Ok(Some(job)) => {
            if let Err(error) = provisioning_queue::enqueue(queue, job).await {
                tracing::warn!(%session, %error, "a wake for an interrupted session would not queue");
            }
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(%session, %error, "an interrupted session's wake could not be read");
        }
    }
}

/// The job a message to an interrupted session enqueues, or `None` when the
/// interruption is not one a message answers.
async fn wake_interrupted_job(
    db: &Db,
    session: SessionId,
) -> Result<Option<ProvisioningJob>, ApiError> {
    match sessions::interruption_reason(db, session).await? {
        Some(InterruptedReason::Suspended) => {
            match machines::for_session(db, session).await? {
                // The ordinary case: the codespace is suspended, not gone —
                // start it.
                Some(row) if row.native_id.is_some() => Ok(Some(ProvisioningJob::resuming(
                    session,
                    row.id,
                    crate::clock::now_unix(),
                ))),
                // The reconcile deleted the codespace between the
                // interruption and this message: suspend became loss, and
                // what answers the message is a fresh machine.
                Some(_) => lost_machine_job(db, session).await.map(Some),
                None => Ok(None),
            }
        }
        Some(InterruptedReason::MachineLost) => lost_machine_job(db, session).await.map(Some),
        // A reclamation's recovery was enqueued where it was reported, and
        // a bare `interrupted` has nothing to wake.
        _ => Ok(None),
    }
}

/// The job that provisions around a machine that is gone for good.
///
/// The reason is rewritten to `machine_lost` first — a suspended machine
/// whose codespace was deleted under it is not suspended any more — then
/// the session goes back to `provisioning` keeping it, so a watching page
/// reads `Migrating` rather than claiming a suspend it can wake from.
async fn lost_machine_job(db: &Db, session: SessionId) -> Result<ProvisioningJob, ApiError> {
    sessions::machine_lost(db, session).await?;
    sessions::recovering(db, session).await?;
    let machine = machines::reset_for_resume(db, session).await?;
    Ok(ProvisioningJob::first(session, machine))
}

/// Holds a message against a session that is waiting out a plan window.
///
/// Answers whether it was held, which is `false` for every session that is
/// not waiting — the ordinary path, which goes on to hand the message to the
/// room.
///
/// Scoped by owner like every other write on a session, so a message can only
/// be queued against the caller's own: the update names both the id and the
/// user, and a session belonging to somebody else simply matches no row.
async fn queue_while_waiting(
    user: &CurrentUser,
    params: &Params,
    text: &str,
    db: &Db,
) -> Result<bool, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    let queued = sessions::queue_usage_limit_message(db, user.id, id, text).await?;
    if queued {
        tracing::info!(
            session = %id,
            "held a message for a session waiting out a spent plan window"
        );
    }
    Ok(queued)
}

/// Ends a session's current turn.
///
/// The daemon interrupts the harness — SIGINT for Claude Code,
/// `turn/interrupt` for Codex — which stops the turn without ending the
/// session.
#[skyzen::openapi]
async fn interrupt_session(
    State(user): State<CurrentUser>,
    params: Params,
    rooms: Rooms,
    db: Db,
) -> Outcome<Accepted> {
    drive(&user, &params, &rooms, &db, ControlToDaemon::Interrupt)
        .await
        .map(|_| Accepted)
        .into()
}

/// Compacts a session's conversation context.
///
/// Claude Code runs its native `/compact` command; Codex runs
/// `thread/compact/start`. The result is reported on the session relay.
#[skyzen::openapi]
async fn compact_session(
    State(user): State<CurrentUser>,
    params: Params,
    rooms: Rooms,
    db: Db,
) -> Outcome<Accepted> {
    drive(&user, &params, &rooms, &db, ControlToDaemon::Compact)
        .await
        .map(|_| Accepted)
        .into()
}

/// Reports what a session's context window is spent on.
///
/// What the usage panel's "detailed breakdown" asks for: a control request
/// the daemon answers out of band —
/// the Claude sidecar's `get_context_usage`, or the window gauge a Codex
/// session already holds — never a message to the model. The answer
/// arrives on the session relay as a `context_usage` harness event.
#[skyzen::openapi]
async fn context_session(
    State(user): State<CurrentUser>,
    params: Params,
    rooms: Rooms,
    db: Db,
) -> Outcome<Accepted> {
    drive(&user, &params, &rooms, &db, ControlToDaemon::ContextUsage)
        .await
        .map(|_| Accepted)
        .into()
}

/// Runs a shell command on a session's machine.
///
/// The composer's `!` escape, over REST: the room mints the run id and
/// reissues the request as [`ControlToDaemon::RunShell`], so every frame
/// about the run is keyed by an identity the browser never chose.
#[skyzen::openapi]
async fn run_shell(
    State(user): State<CurrentUser>,
    params: Params,
    Json(body): Json<RunShell>,
    rooms: Rooms,
    db: Db,
) -> Outcome<Accepted> {
    drive(
        &user,
        &params,
        &rooms,
        &db,
        ControlToDaemon::ShellCommand {
            command: body.command,
        },
    )
    .await
    .map(|_| Accepted)
    .into()
}

/// Writes raw input to a session's web terminal.
///
/// One keystroke batch per call: the terminal panel sends these as the
/// user types, and a dropped one is retried by the user, not the client.
#[skyzen::openapi]
async fn terminal_input(
    State(user): State<CurrentUser>,
    params: Params,
    Json(body): Json<TerminalInput>,
    rooms: Rooms,
    db: Db,
) -> Outcome<Accepted> {
    drive(
        &user,
        &params,
        &rooms,
        &db,
        ControlToDaemon::TerminalInput { data: body.data },
    )
    .await
    .map(|_| Accepted)
    .into()
}

/// Reports the web terminal's fitted size.
///
/// The room remembers it and re-sends it at the head of every daemon
/// attach, so a machine that reboots comes back with a PTY the same size
/// as the pane (issue #253).
#[skyzen::openapi]
async fn terminal_resize(
    State(user): State<CurrentUser>,
    params: Params,
    Json(body): Json<TerminalSize>,
    rooms: Rooms,
    db: Db,
) -> Outcome<Accepted> {
    drive(
        &user,
        &params,
        &rooms,
        &db,
        ControlToDaemon::TerminalResize {
            cols: body.cols,
            rows: body.rows,
        },
    )
    .await
    .map(|_| Accepted)
    .into()
}

/// Puts a session's harness TUI in the terminal's foreground.
///
/// The `flyco claude`/`flyco codex`/`flyco resume` path: the CLI bridges
/// the user's local terminal to the machine's PTY and asks for the
/// harness's own interface rather than the shell. `resume` re-enters the
/// last conversation and makes the request an ensure — a TUI already in
/// the foreground is left alone.
#[skyzen::openapi]
async fn terminal_harness(
    State(user): State<CurrentUser>,
    params: Params,
    Json(body): Json<HarnessTui>,
    rooms: Rooms,
    db: Db,
) -> Outcome<Accepted> {
    drive(
        &user,
        &params,
        &rooms,
        &db,
        ControlToDaemon::TerminalHarness {
            resume: body.resume,
        },
    )
    .await
    .map(|_| Accepted)
    .into()
}

/// Opens the session's desktop stream.
///
/// The screen panel's video feed: an SSE stream of encoded AV1 chunks,
/// replayed from the newest keyframe and then live. The room mints the
/// caller's watcher lease in answering — the stream staying open is the
/// audience the daemon encodes for, and a backgrounded tab closing it is
/// what turns the encoder off.
#[skyzen::openapi]
async fn desktop_stream(
    State(user): State<CurrentUser>,
    params: Params,
    rooms: Rooms,
    db: Db,
) -> Outcome<Response> {
    watch_desktop(&user, &params, &rooms, &db).await.into()
}

async fn watch_desktop(
    user: &CurrentUser,
    params: &Params,
    rooms: &Rooms,
    db: &Db,
) -> Result<Response, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    // Ownership, not liveness: a paused or provisioning session's stream
    // simply idles until a daemon attaches — the watcher is already in
    // place to be the audience it comes up to.
    if !sessions::is_owned_by(db, user.id, id).await? {
        return Err(ApiError::SessionNotFound);
    }
    rooms.desktop_stream(id).await
}

/// Takes the session's screen, or hands it back.
///
/// The `Take over`/`Release` button on the screen panel. The watcher id
/// the desktop stream announced names the lease taking over — a takeover
/// held by a dead browser lapses with it instead of locking the agent
/// out — and the room interrupts the running turn on a take, exactly as
/// `Stop` does.
#[skyzen::openapi]
async fn desktop_takeover(
    State(user): State<CurrentUser>,
    params: Params,
    Json(body): Json<DesktopTakeoverRequest>,
    rooms: Rooms,
    db: Db,
) -> Outcome<Accepted> {
    drive_desktop_takeover(&user, &params, &rooms, &db, body)
        .await
        .map(|_| Accepted)
        .into()
}

async fn drive_desktop_takeover(
    user: &CurrentUser,
    params: &Params,
    rooms: &Rooms,
    db: &Db,
    body: DesktopTakeoverRequest,
) -> Result<SessionId, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    sessions::require_active(db, user.id, id).await?;
    rooms.desktop_takeover(db, id, &body).await?;
    tracing::info!(session = %id, watcher = body.watcher, active = body.active, "drove a desktop takeover");
    Ok(id)
}

/// Sends one batch of the user's desktop input.
///
/// Keystrokes, pointer moves, clicks and scrolls collected by the screen
/// panel since its last send — refused with `409` unless the named
/// watcher holds takeover, so a second tab cannot reach into the screen
/// the first is driving.
#[skyzen::openapi]
async fn desktop_input(
    State(user): State<CurrentUser>,
    params: Params,
    Json(body): Json<DesktopInputRequest>,
    rooms: Rooms,
    db: Db,
) -> Outcome<Accepted> {
    drive_desktop_input(&user, &params, &rooms, &db, body)
        .await
        .map(|_| Accepted)
        .into()
}

async fn drive_desktop_input(
    user: &CurrentUser,
    params: &Params,
    rooms: &Rooms,
    db: &Db,
    body: DesktopInputRequest,
) -> Result<SessionId, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    sessions::require_active(db, user.id, id).await?;
    rooms.desktop_input(id, &body).await?;
    Ok(id)
}

/// Hands one command to a session's room.
///
/// The two checks are in this order for a reason. Ownership settles in D1,
/// because a Durable Object cannot reach it and a room asked to do something
/// has no way of knowing who asked. The lifecycle settles next, because a
/// command sent to a session that is provisioning, paused, or archived would
/// reach a room with no daemon attached and be dropped there — a `202` for
/// work nobody will do. The refusal names the state instead.
///
/// Answers with the session it drove, so a caller that has something to
/// record afterwards — [`say`] writes the activity a user's message leaves
/// the session in — works from the id this function already parsed and
/// proved, rather than reading the path a second time.
async fn drive(
    user: &CurrentUser,
    params: &Params,
    rooms: &Rooms,
    db: &Db,
    command: ControlToDaemon,
) -> Result<SessionId, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    sessions::require_active(db, user.id, id).await?;

    rooms.command(db, id, &command).await?;
    // The room took the command, so the session was provably in use — a
    // terminal keystroke is what keeps an interactive machine off the
    // idle-suspension sweep's list.
    sessions::touch(db, id).await?;
    tracing::info!(session = %id, command = ?core::mem::discriminant(&command), "drove a session");
    Ok(id)
}

/// Puts an interrupted, failed, or archived session back on a machine.
///
/// The same queue every other machine comes from — there is one
/// implementation of provisioning and this is not a second one. The session
/// moves back to `provisioning` durably before the job is enqueued, so a
/// browser that reloads immediately sees a session on its way back rather
/// than the state it was resumed out of.
///
/// The machine row keeps its identity, which is what lets the provider
/// recognise the machine it already made: what a resume rebuilds is the
/// session's own machine, not another one beside it.
#[skyzen::openapi]
async fn resume_session(
    State(user): State<CurrentUser>,
    params: Params,
    queue: Queue,
    rooms: Rooms,
    db: Db,
) -> Outcome<Json<SessionDetail>> {
    restart_session(&user, &params, &queue, &rooms, &db)
        .await
        .into()
}

async fn restart_session(
    user: &CurrentUser,
    params: &Params,
    queue: &Queue,
    rooms: &Rooms,
    db: &Db,
) -> Result<Json<SessionDetail>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    let session = sessions::resume(db, user.id, id).await?;
    let row = machines::for_session(db, id)
        .await?
        .ok_or(ApiError::MachineNotFound)?;

    // What a resume asks for depends on whether the provider still holds
    // the machine the row names. With a native id it may — a suspended
    // codespace, a deallocated spot instance — and starting it keeps the
    // disk the session left behind; without one it cannot, and the row is
    // reset for a fresh provision instead.
    let job = if row.native_id.is_some() {
        ProvisioningJob::resumed(id, row.id, crate::clock::now_unix())
    } else {
        machines::reset_for_resume(db, id).await?;
        ProvisioningJob::first(id, row.id)
    };

    if let Err(error) = provisioning_queue::enqueue(queue, job).await {
        sessions::fail(
            db,
            rooms,
            id,
            "the provisioning queue would not accept this session's job",
        )
        .await?;
        return Err(error);
    }

    tracing::info!(session = %id, machine = %row.id, "resumed a session onto its machine");
    Ok(Json(session))
}

/// Where in a session's turn history to read from.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
pub struct TurnCursor {
    /// Opaque cursor from the previous page. Omitted starts at the oldest
    /// turn.
    pub cursor: Option<String>,
    /// How many turns to return; the control plane caps it.
    pub limit: Option<u32>,
}

/// Lists a session's turns, oldest first.
///
/// Folded out of the room's recorded event stream rather than read from a
/// table, because that stream is both what survives a machine and what a
/// browser replays: a turn list built from anything else would disagree
/// with the conversation shown beside it. See [`crate::turns`].
#[skyzen::openapi]
async fn list_turns(
    State(user): State<CurrentUser>,
    params: Params,
    Query(cursor): Query<TurnCursor>,
    rooms: Rooms,
    db: Db,
) -> Outcome<Json<TurnPage>> {
    read_turns(&user, &params, &cursor, &rooms, &db)
        .await
        .into()
}

async fn read_turns(
    user: &CurrentUser,
    params: &Params,
    cursor: &TurnCursor,
    rooms: &Rooms,
    db: &Db,
) -> Result<Json<TurnPage>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    // A room cannot reach D1, so ownership is settled here, before the
    // Worker will address the room at all.
    if !sessions::is_owned_by(db, user.id, id).await? {
        return Err(ApiError::SessionNotFound);
    }
    turns::page(rooms, id, cursor.cursor.as_deref(), cursor.limit)
        .await
        .map(Json)
}

/// Reads a session's `.env`.
///
/// The response repeats the standing warning that flyco does not restrict a
/// session's network access yet, so a client renders the caveat beside the
/// values instead of hard-coding it.
#[skyzen::openapi]
async fn get_session_env(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    params: Params,
    db: Db,
) -> Outcome<Json<EnvDocument>> {
    read_env(&user, &config, &params, &db).await.into()
}

async fn read_env(
    user: &CurrentUser,
    config: &ApiConfig,
    params: &Params,
    db: &Db,
) -> Result<Json<EnvDocument>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    env::read(db, &config.token_cipher(), user.id, id)
        .await
        .map(Json)
}

/// Replaces a session's `.env`.
///
/// The user's route, and the only one that writes: the agent is given the
/// environment read-only. Nothing is pushed to a running session — a
/// process's environment is fixed when it starts — so the new document is
/// what the *next* harness start reads. See [`crate::env`].
#[skyzen::openapi]
async fn put_session_env(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    params: Params,
    Json(update): Json<UpdateEnv>,
    db: Db,
) -> Outcome<Json<EnvDocument>> {
    write_env(&user, &config, &params, update, &db).await.into()
}

async fn write_env(
    user: &CurrentUser,
    config: &ApiConfig,
    params: &Params,
    update: UpdateEnv,
    db: &Db,
) -> Result<Json<EnvDocument>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    env::replace(db, &config.token_cipher(), user.id, id, update.entries)
        .await
        .map(Json)
}

/// Reports whether a session's working tree has uncommitted changes.
///
/// Load-bearing rather than informational: an agent may not stop while the
/// tree is dirty, and archiving a dirty session warns before the disk goes.
#[skyzen::openapi]
async fn get_repo_status(
    State(user): State<CurrentUser>,
    params: Params,
    rooms: Rooms,
    db: Db,
) -> Outcome<Json<RepoStatus>> {
    read_repo_status(&user, &params, &rooms, &db).await.into()
}

async fn read_repo_status(
    user: &CurrentUser,
    params: &Params,
    rooms: &Rooms,
    db: &Db,
) -> Result<Json<RepoStatus>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    if !sessions::is_owned_by(db, user.id, id).await? {
        return Err(ApiError::SessionNotFound);
    }
    rooms.repo_status(id).await.map(Json)
}

/// Adds a repository to a session's workspace, at the user's hand.
///
/// The row is written before the daemon is told, in the same order a
/// decided approval works in: the repository is a fact of the session
/// whether or not a machine is listening — a daemon that is not attached
/// has the command held for it, and a machine provisioned after this reads
/// the whole set at boot anyway.
#[skyzen::openapi]
async fn add_session_repo(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(github): State<GithubClient>,
    params: Params,
    Json(selection): Json<RepoSelection>,
    rooms: Rooms,
    db: Db,
) -> Outcome<Created<Json<SessionDetail>>> {
    attach_repo(&user, &params, selection, &config, &github, &rooms, &db)
        .await
        .into()
}

async fn attach_repo(
    user: &CurrentUser,
    params: &Params,
    selection: RepoSelection,
    config: &ApiConfig,
    github: &GithubClient,
    rooms: &Rooms,
    db: &Db,
) -> Result<Created<Json<SessionDetail>>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    if !sessions::is_owned_by(db, user.id, id).await? {
        return Err(ApiError::SessionNotFound);
    }
    let slug = selection
        .repo
        .parse::<RepoSlug>()
        .map_err(|_| ApiError::InvalidRepo(selection.repo.clone()))?;
    let token = users::github_token(db, config, github, user.id).await?;
    let branch = resolve_branch(github, &token, &slug, selection.branch.as_deref()).await?;
    let repo = session_repos::attach(db, id, &slug, &branch, RepoAddedBy::User).await?;
    announce_add_repo(rooms, db, id, &repo).await;
    sessions::find(db, user.id, id)
        .await
        .map(|detail| Created(Json(detail)))
}

/// Tells a session's daemon it has another checkout, logging rather than
/// failing when nobody is there to hear it.
///
/// The command is held for a disconnected daemon, so an unreachable room
/// costs a delayed clone rather than a repository the machine never learns
/// about.
async fn announce_add_repo(rooms: &Rooms, db: &Db, session: SessionId, repo: &SessionRepo) {
    let Some(branch) = repo.branch.clone() else {
        // `attach` only ever writes a concrete branch, so this is dead — but
        // an `AddRepo` without one would send the daemon cloning a guess.
        tracing::error!(%session, dir = %repo.dir, "a recorded repository carries no branch");
        return;
    };
    let result = rooms
        .command(
            db,
            session,
            &ControlToDaemon::AddRepo {
                slug: repo.slug.clone(),
                branch,
                dir: repo.dir.clone(),
            },
        )
        .await;
    if let Err(error) = result {
        tracing::warn!(%session, dir = %repo.dir, %error, "an added repository did not reach its room");
    }
}

/// Query of the two `Files` routes.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
struct CheckoutPath {
    /// A path inside the session's checkout, `/`-separated and relative to
    /// its root. Omitted lists the root itself.
    #[serde(default)]
    path: Option<String>,
}

/// Lists one directory of a session's checkout (docs/ux.md §9.4).
///
/// Answered live by the machine: there is no copy of a working tree in the
/// control plane, so this is relayed to the session's daemon and the
/// browser is told plainly when there is no daemon to answer it.
#[skyzen::openapi]
async fn list_session_files(
    State(user): State<CurrentUser>,
    params: Params,
    Query(query): Query<CheckoutPath>,
    rooms: Rooms,
    db: Db,
) -> Outcome<Json<DirectoryListing>> {
    read_session_files(&user, &params, query.path, &rooms, &db)
        .await
        .into()
}

async fn read_session_files(
    user: &CurrentUser,
    params: &Params,
    path: Option<String>,
    rooms: &Rooms,
    db: &Db,
) -> Result<Json<DirectoryListing>, ApiError> {
    let request = WorkdirRequest::Entries {
        path: path.unwrap_or_default(),
    };
    match inspect_workdir(user, params, rooms, db, request).await? {
        WorkdirReply::Entries { listing } => Ok(Json(listing)),
        other => Err(mismatched("a directory listing", &other)),
    }
}

/// Reads one text file out of a session's checkout (docs/ux.md §9.4).
#[skyzen::openapi]
async fn read_session_file(
    State(user): State<CurrentUser>,
    params: Params,
    Query(query): Query<CheckoutPath>,
    rooms: Rooms,
    db: Db,
) -> Outcome<Json<FileContent>> {
    read_file_content(&user, &params, query.path, &rooms, &db)
        .await
        .into()
}

async fn read_file_content(
    user: &CurrentUser,
    params: &Params,
    path: Option<String>,
    rooms: &Rooms,
    db: &Db,
) -> Result<Json<FileContent>, ApiError> {
    let request = WorkdirRequest::File {
        path: path.unwrap_or_default(),
    };
    match inspect_workdir(user, params, rooms, db, request).await? {
        WorkdirReply::File { content } => Ok(Json(content)),
        other => Err(mismatched("a file", &other)),
    }
}

/// Which checkout a `diff` asks about.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
struct DiffQuery {
    /// The checkout's directory under the workdir, as
    /// [`SessionRepo::dir`](flyco_core::SessionRepo::dir) names it.
    /// Omitted on a session whose workdir is itself the checkout — the
    /// developer-machine shape.
    repo: Option<String>,
}

/// Diffs one checkout of a session's workspace against the branch it
/// started from.
#[skyzen::openapi]
async fn get_session_diff(
    State(user): State<CurrentUser>,
    params: Params,
    Query(query): Query<DiffQuery>,
    rooms: Rooms,
    db: Db,
) -> Outcome<Json<WorkdirDiff>> {
    read_session_diff(&user, &params, query, &rooms, &db)
        .await
        .into()
}

async fn read_session_diff(
    user: &CurrentUser,
    params: &Params,
    query: DiffQuery,
    rooms: &Rooms,
    db: &Db,
) -> Result<Json<WorkdirDiff>, ApiError> {
    let request = WorkdirRequest::Diff { repo: query.repo };
    match inspect_workdir(user, params, rooms, db, request).await? {
        WorkdirReply::Diff { diff } => Ok(Json(diff)),
        other => Err(mismatched("a diff", &other)),
    }
}

/// Puts one question about a session's checkout to its daemon.
///
/// Ownership is settled here, before the room is addressed at all: a room
/// cannot reach D1, and the checkout of somebody else's session is
/// indistinguishable from one that does not exist. A refusal the daemon
/// answered with becomes the [`ApiError`] it maps to, so every one of them
/// reaches the browser as its own RFC 9457 problem.
async fn inspect_workdir(
    user: &CurrentUser,
    params: &Params,
    rooms: &Rooms,
    db: &Db,
    request: WorkdirRequest,
) -> Result<WorkdirReply, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    if !sessions::is_owned_by(db, user.id, id).await? {
        return Err(ApiError::SessionNotFound);
    }
    let reply = rooms.inspect_workdir(id, request).await?;
    if let WorkdirReply::Refused { refusal } = reply {
        tracing::info!(session = %id, ?refusal, "a daemon refused a question about its checkout");
        return Err(refusal.into());
    }
    Ok(reply)
}

/// A daemon answered a question with the answer to another one, which is a
/// protocol fault rather than anything the caller did.
fn mismatched(expected: &str, reply: &WorkdirReply) -> ApiError {
    ApiError::Room(format!(
        "the session's daemon answered {expected} with {reply:?}"
    ))
}

// ── Daemon-scoped routes ──

/// The harness-native session id the daemon announced at start.
#[derive(Debug, Deserialize, skyzen::ToSchema)]
struct HarnessSessionIdentity {
    /// Identity the harness minted; resume reopens this conversation.
    harness_session_id: String,
}

/// Records the harness-native session id so a later resume continues it.
#[skyzen::openapi]
async fn put_harness_session(
    State(session): State<DaemonSession>,
    Json(identity): Json<HarnessSessionIdentity>,
    db: Db,
) -> Outcome<NoContent> {
    sessions::record_harness_session(&db, session.0, &identity.harness_session_id)
        .await
        .map(|()| NoContent)
        .into()
}

/// Reads the harness conversation a daemon on this session must continue,
/// and the model it must continue it on.
///
/// The daemon asks at startup instead of trusting the configuration on its
/// disk: that file was written when the machine was created, and a machine
/// that was stopped and started again on the same disk — which is how a
/// spot reclamation is recovered from — boots the same file. A daemon that
/// trusted it would open a second conversation beside the one the user is
/// watching, and would open it on the model the session had before the user
/// changed it.
#[skyzen::openapi]
async fn get_harness_session(
    State(session): State<DaemonSession>,
    db: Db,
) -> Outcome<Json<HarnessSessionView>> {
    sessions::harness_session(&db, session.0)
        .await
        .map(Json)
        .into()
}

/// Records the models this session's harness offers.
///
/// Filed by the daemon once its harness has answered — the earliest moment
/// the answer exists — and stored against the *account*, because the list
/// is a fact about the harness build that account's machines run and the
/// composer needs it before the next session exists. The live half goes to
/// the room in the same call, so a browser watching this session gets the
/// real list without reloading the account.
#[skyzen::openapi]
async fn report_models(
    State(session): State<DaemonSession>,
    Json(report): Json<ReportModels>,
    rooms: Rooms,
    db: Db,
) -> Outcome<NoContent> {
    record_reported_models(session.0, report, &rooms, &db)
        .await
        .into()
}

async fn record_reported_models(
    id: SessionId,
    report: ReportModels,
    rooms: &Rooms,
    db: &Db,
) -> Result<NoContent, ApiError> {
    // An `fd_` token proves which session is calling and nothing about a
    // user, so the owner and the harness are derived from the session row
    // rather than trusted from the body — which is what keeps one session's
    // daemon from rewriting another user's model list.
    let target = sessions::provisioning_target(db, id)
        .await?
        .ok_or(ApiError::SessionNotFound)?;
    // The harness answers in its own vocabulary — Devin lists one id per
    // effort level — so the report is folded into the picker's shape once
    // here: the stored list and the room's live update are the same
    // document a reload would read back.
    let models = flyco_core::normalize_models(target.harness, report.models);
    harness_accounts::record_models(db, target.user_id, target.harness, &models).await?;
    rooms
        .broadcast(db, id, &ClientEvent::Models { models })
        .await?;
    Ok(NoContent)
}

/// Records how much of this session's harness plan is spent.
///
/// Filed by the daemon at session start and after every turn — the two
/// moments the number can have moved — and stored against the *account*,
/// because the plan belongs to the account and the settings page reads it
/// there without a session. The live half goes to the room in the same
/// call, so the composer's rings move as the turn ends rather than on the
/// next page load.
#[skyzen::openapi]
async fn report_usage(
    State(session): State<DaemonSession>,
    Json(report): Json<ReportUsage>,
    rooms: Rooms,
    db: Db,
) -> Outcome<NoContent> {
    record_reported_usage(session.0, report, &rooms, &db)
        .await
        .into()
}

async fn record_reported_usage(
    id: SessionId,
    report: ReportUsage,
    rooms: &Rooms,
    db: &Db,
) -> Result<NoContent, ApiError> {
    // The owner and the harness come from the session row and never from
    // the body, for the same reason they do in `record_reported_models`: an
    // `fd_` token proves which session is calling and nothing about a user.
    let target = sessions::provisioning_target(db, id)
        .await?
        .ok_or(ApiError::SessionNotFound)?;
    harness_accounts::record_usage(db, target.user_id, target.harness, &report.windows).await?;
    rooms
        .broadcast(
            db,
            id,
            &ClientEvent::PlanUsage {
                windows: report.windows,
            },
        )
        .await?;
    Ok(NoContent)
}

/// Records that this session's harness has run out of plan, and stops the
/// session until the window turns over.
///
/// The one thing a daemon reports that stops the session rather than
/// describing it: the harness has refused a turn because a rolling window of
/// the account's plan is spent, and there is nothing to do until it resets.
/// The machine is released so the wait costs nothing and started again ten
/// minutes before the reset, and the conversation is picked back up on the
/// user's behalf — see [`crate::usage_limits`] for the whole sequence.
///
/// A route of its own rather than a flag on [`report_usage`] beside it,
/// because the two are read by different things and filed at different
/// times: a usage snapshot fills the rings and is filed after every turn,
/// and this pauses a session and is filed once per limit.
///
/// Answers `202`: the pause is durable when this returns, and the machine
/// the pause is about is released by the minute sweep rather than in this
/// request.
#[skyzen::openapi]
async fn report_usage_limit(
    State(session): State<DaemonSession>,
    State(config): State<ApiConfig>,
    Json(report): Json<UsageLimitHit>,
    rooms: Rooms,
    db: Db,
) -> Outcome<Accepted> {
    usage_limits::pause(
        &db,
        &config,
        &rooms,
        session.0,
        &report.window,
        crate::clock::now_unix(),
    )
    .await
    .map(|()| Accepted)
    .into()
}

/// Records that this session's machine is being reclaimed by its provider.
///
/// The durable half of a spot notice. The relay frame beside it puts the
/// countdown in front of the user; this is what survives the machine, and
/// it has to be a REST call rather than a room frame because a Durable
/// Object can reach neither D1 nor the provisioning queue.
///
/// Two things happen, in this order:
///
/// 1. The session is marked interrupted, with the reason, so every list
///    and header reads `Interrupted · spot reclaimed` rather than a
///    session that mysteriously stopped.
/// 2. A [`Recover`](crate::provisioning_queue::ProvisioningJob::Recover)
///    job is queued for after the provider's own countdown, because a
///    start issued against a machine that is still running is not a
///    restart.
///
/// Answers `202`: the machine is going whatever the control plane thinks,
/// and what this accepts is the work of getting the session back.
///
/// What browsers see is *not* here. The countdown reaches them as the relay
/// frame the daemon sends immediately after this call, which the room
/// records and forwards in one place — announcing it here as well would put
/// the same notice in the transcript twice.
/// Records that this session's machine is stopping and its filesystem is
/// going with it.
///
/// The container counterpart of
/// [`report_spot_notice`], and a route of its own rather than a flag on
/// that one because the two ask for different things. A reclaimed virtual
/// machine keeps its disk, so the control plane queues a *recovery* against
/// the same machine after the provider's countdown. A stopping container
/// has no disk to come back to: its daemon has already flushed the
/// transcript and stored the working tree as the `workdir-patch`, and there
/// is nothing left to schedule — what the control plane needs is the mark
/// on the row, which is what the container drivers read to tell an
/// execution that was stopped from one that died.
///
/// Answers `202`: the machine is going whatever the control plane thinks,
/// and this is the record of it.
#[skyzen::openapi]
async fn report_stopping(
    State(session): State<DaemonSession>,
    Json(report): Json<ReportStopping>,
    db: Db,
) -> Outcome<Accepted> {
    mark_stopping(session.0, report, &db).await.into()
}

async fn mark_stopping(
    session: SessionId,
    report: ReportStopping,
    db: &Db,
) -> Result<Accepted, ApiError> {
    let machine = machines::for_session(db, session)
        .await?
        .ok_or(ApiError::MachineNotFound)?;
    machines::mark_stopping(db, machine.id, report.reason).await?;

    tracing::warn!(
        %session,
        machine = %machine.id,
        reason = ?report.reason,
        "a session's machine is stopping; its working tree is stored as a patch"
    );
    Ok(Accepted)
}

#[skyzen::openapi]
async fn report_spot_notice(
    State(session): State<DaemonSession>,
    Json(report): Json<ReportSpotNotice>,
    queue: Queue,
    db: Db,
) -> Outcome<Accepted> {
    reclaim(session.0, report, &queue, &db).await.into()
}

async fn reclaim(
    session: SessionId,
    report: ReportSpotNotice,
    queue: &Queue,
    db: &Db,
) -> Result<Accepted, ApiError> {
    sessions::interrupted(db, session, InterruptedReason::SpotReclaimed).await?;

    let machine = machines::for_session(db, session)
        .await?
        .ok_or(ApiError::MachineNotFound)?;
    provisioning_queue::enqueue_after(
        queue,
        ProvisioningJob::recovery(session, machine.id, crate::clock::now_unix()),
        core::time::Duration::from_secs(u64::from(report.seconds_remaining)),
    )
    .await?;

    tracing::warn!(
        %session,
        seconds_remaining = report.seconds_remaining,
        "a session's machine is being reclaimed; a recovery is queued"
    );
    Ok(Accepted)
}

/// Raises an approval against the daemon's own session.
///
/// The daemon records the durable approval here *before* it announces the
/// request on the relay, so the id a browser sees is always one the API can
/// decide.
///
/// Exported like the user-facing routes even though it is daemon-scoped:
/// flycod's contract deserves the same typed description the browser
/// client's does, and unlike the transcript and relay routes beside it, this
/// one answers with an ordinary JSON document.
#[skyzen::openapi]
async fn raise_approval(
    State(session): State<DaemonSession>,
    State(config): State<ApiConfig>,
    State(github): State<GithubClient>,
    Json(payload): Json<ApprovalPayload>,
    db: Db,
) -> Outcome<Created<Json<ApprovalView>>> {
    record_approval(session.0, payload, &db, &config, &github)
        .await
        .into()
}

async fn record_approval(
    session: SessionId,
    payload: ApprovalPayload,
    db: &Db,
    config: &ApiConfig,
    github: &GithubClient,
) -> Result<Created<Json<ApprovalView>>, ApiError> {
    let payload = settle_repo_add(session, payload, db, config, github).await?;
    let id = approvals::raise(db, session, &payload).await?;
    let view = approvals::find_for_session(db, session, id).await?;
    push::notify_approval(db, config, session).await?;
    tracing::info!(%session, "a daemon raised an approval");
    Ok(Created(Json(view)))
}

/// Validates an [`ApprovalPayload::RepoAdd`] the daemon asked for, and
/// resolves the branch it does not name.
///
/// The check is the same one a user's own pick goes through — the slug is
/// `owner/name`, the branch parses, and the owner's stored token can see
/// the repository — because an approval the user can only refuse is a
/// worse answer to the agent than the error itself: a `RepoAdd` for a
/// repository that does not exist should come back as the tool's failure,
/// not as a card asking the user to decide on a clone that cannot happen.
///
/// The payload that is stored carries the *resolved* branch rather than
/// the `None` the agent sent, so approving it later performs the clone it
/// described without asking GitHub again.
async fn settle_repo_add(
    session: SessionId,
    payload: ApprovalPayload,
    db: &Db,
    config: &ApiConfig,
    github: &GithubClient,
) -> Result<ApprovalPayload, ApiError> {
    let ApprovalPayload::RepoAdd {
        repo,
        branch,
        reason,
    } = payload
    else {
        return Ok(payload);
    };
    let slug = repo
        .parse::<RepoSlug>()
        .map_err(|_| ApiError::InvalidRepo(repo.clone()))?;
    let owner = sessions::owner(db, session).await?;
    let token = users::github_token(db, config, github, owner).await?;
    let branch = resolve_branch(github, &token, &slug, branch.as_deref()).await?;
    let attached = session_repos::of_session(db, session).await?;
    if attached.iter().any(|existing| existing.slug == slug) {
        return Err(ApiError::RepoAlreadyAttached { repo: slug });
    }
    if attached.len() >= flyco_core::MAX_SESSION_REPOS {
        return Err(ApiError::SessionRepoCapReached {
            cap: flyco_core::MAX_SESSION_REPOS,
        });
    }
    Ok(ApprovalPayload::RepoAdd {
        repo: slug.as_str().to_owned(),
        branch: Some(branch.as_str().to_owned()),
        reason,
    })
}

/// Records that this session's harness began a turn.
///
/// The mirror of [`notify_turn_completed`], and the reason the home list can
/// say `Working` at all: a turn starting is announced on the relay, and a
/// Durable Object cannot reach D1, so the durable half of the fact needs a
/// route of its own (docs/ux.md §6).
///
/// No notification goes out for it. A turn starting is the user's own
/// message being answered — they are looking at it — and a push for every
/// turn would be noise.
#[skyzen::openapi]
async fn notify_turn_started(State(session): State<DaemonSession>, db: Db) -> Outcome<NoContent> {
    sessions::record_activity(&db, session.0, SessionActivity::after_turn(true))
        .await
        .map(|()| NoContent)
        .into()
}

#[skyzen::openapi]
async fn notify_turn_completed(
    State(session): State<DaemonSession>,
    State(config): State<ApiConfig>,
    db: Db,
) -> Outcome<NoContent> {
    finish_turn(session.0, true, &config, &db).await.into()
}

/// What both terminal turn routes do: record whose move it is, then tell the
/// user's browsers.
///
/// The activity is written first and the notification is what may fail: a
/// row that says `Needs input` is what the list is read from, and losing it
/// because a push endpoint was gone would leave a finished session looking
/// like one still working.
async fn finish_turn(
    session: SessionId,
    completed: bool,
    config: &ApiConfig,
    db: &Db,
) -> Result<NoContent, ApiError> {
    sessions::record_activity(db, session, SessionActivity::after_turn(false)).await?;
    push::notify_turn(db, config, session, completed).await?;
    Ok(NoContent)
}

/// Records a provisioning milestone the session's own machine reached.
///
/// The queue announces everything up to the machine existing; everything
/// after it is a fact only the daemon holds. Most of those ride the relay,
/// but the checkout happens *before* the harness exists and therefore before
/// there is a command stream — so the one stage that cannot be a relay frame
/// gets a route (docs/ux.md §9.2).
///
/// The control plane stamps the time rather than taking the daemon's: a
/// session VM with a wrong clock must not be able to put a line of the
/// timeline in 1970.
#[skyzen::openapi]
async fn report_provisioning_stage(
    State(session): State<DaemonSession>,
    Json(report): Json<ReportProvisioningStage>,
    rooms: Rooms,
    db: Db,
) -> Outcome<NoContent> {
    record_provisioning_stage(session.0, report.stage, &rooms, &db)
        .await
        .into()
}

async fn record_provisioning_stage(
    session: SessionId,
    stage: ProvisioningStage,
    rooms: &Rooms,
    db: &Db,
) -> Result<NoContent, ApiError> {
    sessions::note_progress(db, session).await?;
    rooms
        .broadcast(
            db,
            session,
            &ClientEvent::ProvisioningStage {
                stage,
                at_unix: crate::clock::now_unix(),
            },
        )
        .await
        .map(|()| NoContent)
}

/// Fails the sessions whose machines stopped being built.
///
/// A machine that reserved, booted and installed and then went quiet is
/// not slow: its daemon is crash-looping, or it cannot reach the control
/// plane at all. Nothing else notices — the queue's job finished, and the
/// daemon that would report a failure is the thing that is failing — so
/// without this the page spins under a timeline that never advances
/// (docs/ux.md §9.2). The session is told what happened and offered a
/// resume, which builds it again.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn fail_stalled_provisions(
    db: &Db,
    config: &ApiConfig,
    github: &impl GithubOauth,
    rooms: &Rooms,
    hosts: &HostRooms,
    at_unix: u64,
) -> Result<(), ApiError> {
    for stalled in sessions::stalled_provisions(db, at_unix).await? {
        // A row the provider never finished — the queue's job is still
        // running, still failing, or still following a build it handed
        // back — is not a machine that "was built", and saying so would
        // send whoever reads the sentence to look at a daemon that never
        // existed.
        let built = machines::for_session(db, stalled.id)
            .await?
            .is_some_and(|machine| machine.is_provisioned());
        // The daemon's own sentence when it managed to send one, because
        // "never reported its agent ready" is what flyco saw and not what
        // happened.
        let reason = match (built, sessions::startup_failure(db, stalled.id).await?) {
            (false, _) => format!(
                "the provider had not finished building the machine after {} minutes, so flyco \
                 stopped waiting for it and released the reservation",
                flyco_core::PROVISION_DEADLINE_SECS / 60
            ),
            (true, None) => "the machine was built but never reported its agent ready, so flyco \
                             stopped waiting for it and released it"
                .to_owned(),
            (true, Some(failure)) => format!(
                "the machine was built but its agent never started: {failure}. flyco stopped \
                 waiting for it and released it"
            ),
        };
        // Destroyed first: a machine that never came up is still a machine
        // running up a bill, and a session flyco has given up on must not
        // go on paying for one. Resuming builds a new one on the same row.
        // A provider that refuses is logged rather than raised — the
        // session is failed either way, and a machine left behind is a
        // cost to report, not a reason to keep the page spinning.
        if let Err(error) =
            machines::destroy_for_archive(db, config, github, hosts, stalled.user_id, stalled.id)
                .await
        {
            tracing::warn!(
                session = %stalled.id,
                %error,
                "a stalled session's machine could not be released"
            );
        }
        sessions::fail(db, rooms, stalled.id, &reason).await?;
    }

    // Handoffs whose uploads never finished run on their own, longer
    // clock: `stalled_provisions` skips them while a pending row exists,
    // and this sweep is what fails the ones the sender walked away from.
    for abandoned in handoffs::abandoned(db, at_unix).await? {
        let reason = format!(
            "the handoff's payloads were not uploaded within {} minutes, so flyco \
             stopped waiting for them and released the reservation",
            handoffs::HANDOFF_DEADLINE_SECS / 60
        );
        if let Err(error) = machines::destroy_for_archive(
            db,
            config,
            github,
            hosts,
            abandoned.user_id,
            abandoned.id,
        )
        .await
        {
            tracing::warn!(
                session = %abandoned.id,
                %error,
                "an abandoned handoff's machine could not be released"
            );
        }
        sessions::fail(db, rooms, abandoned.id, &reason).await?;
    }
    Ok(())
}

/// Records why this session's daemon stopped before it could report in.
///
/// `flycod` is restarted on failure, so this is not itself a verdict: the
/// next start may succeed, and a session that comes up keeps nothing of
/// this. What it buys is the sentence — a machine that goes on failing is
/// failed by the stall sweep with the daemon's own words instead of a
/// guess (issue #186).
#[skyzen::openapi]
async fn report_startup_failure(
    State(session): State<DaemonSession>,
    State(config): State<ApiConfig>,
    State(github): State<GithubClient>,
    Json(report): Json<ReportStartupFailure>,
    db: Db,
    rooms: Rooms,
    hosts: HostRooms,
) -> Outcome<NoContent> {
    take_failure_report(
        &db,
        &config,
        &github,
        &rooms,
        &hosts,
        session.0,
        &report.message,
    )
    .await
    .map(|()| NoContent)
    .into()
}

/// Destroys the machines of sessions that are already over.
///
/// The backstop for every release path. Archive, the stall sweep and a
/// daemon's failure report each destroy the machine inline, and each of
/// them is a request that can end early — most of all the failure report,
/// whose caller is a daemon that stops as soon as it has spoken. A machine
/// that outlives its session bills silently and forever, so the invariant
/// is checked here every minute instead of being trusted to whoever should
/// have kept it.
///
/// Idempotent: a row already `Destroyed` is not selected, and destroying a
/// machine a provider has already released is a no-op.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails. A provider that refuses one
/// machine is logged and the sweep goes on to the next: one account whose
/// credentials expired must not stop every other user's machine from being
/// released.
pub async fn release_ended_machines(
    db: &Db,
    config: &ApiConfig,
    github: &impl GithubOauth,
    hosts: &HostRooms,
) -> Result<(), ApiError> {
    for ended in sessions::ended_holding_a_machine(db).await? {
        if let Err(error) =
            machines::destroy_for_archive(db, config, github, hosts, ended.user_id, ended.id).await
        {
            tracing::warn!(
                session = %ended.id,
                %error,
                "a machine outliving its session could not be released"
            );
        } else {
            tracing::info!(
                session = %ended.id,
                "released a machine that outlived its session"
            );
        }
    }
    Ok(())
}

/// Records why a daemon is stopping, and fails the session if that is what
/// it means.
///
/// The two cases differ by whether the session ever went live, which is a
/// fact the control plane holds and the dying daemon does not:
///
/// - **Still provisioning.** `flycod` did not get as far as reporting in,
///   and it is `Restart=on-failure`, so the next attempt may well work. The
///   reason is recorded and said only if the machine never does come up,
///   which is the stall sweep's job.
/// - **Already live.** The agent process is gone, and a `flycod` that
///   reaches this point exits cleanly — so systemd will not restart it and
///   nothing else is coming. Failing now is the whole difference between a
///   session that says what happened and one that spins for fifteen minutes
///   before a sweep notices (docs/ux.md §9.6).
async fn take_failure_report(
    db: &Db,
    config: &ApiConfig,
    github: &GithubClient,
    rooms: &Rooms,
    hosts: &HostRooms,
    id: SessionId,
    message: &str,
) -> Result<(), ApiError> {
    sessions::note_startup_failure(db, id, message).await?;
    let target = sessions::provisioning_target(db, id)
        .await?
        .ok_or(ApiError::SessionNotFound)?;
    if target.state == SessionState::Provisioning {
        return Ok(());
    }
    // Failed first, and only then the machine. The caller here is a daemon
    // saying why it cannot go on, and a process that has said that is about
    // to stop: its connection can close mid-request, and a handler that
    // spent its first seconds tearing down a machine would be cut off
    // before it ever wrote the sentence the page is waiting for. The state
    // change is one row and a broadcast; the teardown is a provider call
    // measured in tens of seconds.
    //
    // `sessions::fail` releases only a machine that was never built, so
    // this session's — which is running right now — is destroyed after
    // (issue #199). A provider that refuses is logged rather than raised:
    // the session failed either way, and `release_ended_machines` sweeps
    // whatever this attempt did not finish.
    sessions::fail(db, rooms, id, message).await?;
    if let Err(error) =
        machines::destroy_for_archive(db, config, github, hosts, target.user_id, id).await
    {
        tracing::warn!(
            session = %id,
            %error,
            "a failed session's machine could not be released"
        );
    }
    Ok(())
}

#[skyzen::openapi]
async fn notify_turn_failed(
    State(session): State<DaemonSession>,
    State(config): State<ApiConfig>,
    db: Db,
) -> Outcome<NoContent> {
    finish_turn(session.0, false, &config, &db).await.into()
}

/// Records one thing this session's daemon observed about the harness
/// account driving it.
///
/// The LLM usage panel is reactive by necessity — no vendor publishes a
/// remaining-quota API — so this is where its numbers come from: the daemon
/// holds the turn's `UsageReport` and is what receives `UsageLimited`, and
/// neither can be asked for after the fact.
///
/// Which account the observation lands on is derived from the session
/// itself, so a daemon never names one. Answers `204`: the row's identity
/// is of no use to the daemon that posted it.
#[skyzen::openapi]
async fn record_harness_observation(
    State(session): State<DaemonSession>,
    Json(observation): Json<HarnessObservation>,
    db: Db,
) -> Outcome<NoContent> {
    observe(session.0, observation, &db).await.into()
}

async fn observe(
    session: SessionId,
    observation: HarnessObservation,
    db: &Db,
) -> Result<NoContent, ApiError> {
    observations::record(db, session, observation).await?;
    Ok(NoContent)
}

/// Stores one batch of a session's transcript.
async fn put_transcript_batch(
    State(session): State<DaemonSession>,
    params: Params,
    body: Bytes,
    storage: Storage,
) -> Outcome<NoContent> {
    store_batch(session.0, &params, body, &storage).await.into()
}

async fn store_batch(
    session: SessionId,
    params: &Params,
    body: Bytes,
    storage: &Storage,
) -> Result<NoContent, ApiError> {
    let stream = path_segment(params, "stream")?;
    let raw = path_segment(params, "seq")?;
    let seq = raw.parse::<u64>().map_err(|_| ApiError::MalformedId(raw))?;

    transcripts::put_batch(storage, session, &stream, seq, body.to_vec()).await?;
    Ok(NoContent)
}

/// Reads a session's transcript stream back, for a resume onto a new host.
async fn get_transcript(
    State(session): State<DaemonSession>,
    params: Params,
    storage: Storage,
) -> Outcome<Response> {
    read_transcript(session.0, &params, &storage).await.into()
}

async fn read_transcript(
    session: SessionId,
    params: &Params,
    storage: &Storage,
) -> Result<Response, ApiError> {
    let stream = path_segment(params, "stream")?;
    let read = transcripts::read_stream(storage, session, &stream).await?;

    let mut response = Response::new(skyzen::Body::from(read.body));
    response.headers_mut().insert(
        skyzen::header::CONTENT_TYPE,
        skyzen::header::HeaderValue::from_static(transcripts::CONTENT_TYPE),
    );
    response.headers_mut().insert(
        transcripts::BATCH_COUNT_HEADER,
        skyzen::header::HeaderValue::from_str(&read.batches.to_string()).map_err(|_| {
            ApiError::CorruptRecord("a transcript batch count did not fit in a header")
        })?,
    );
    Ok(response)
}

/// Which checkout a `workdir-patch` is the diff of.
///
/// Daemon-scoped, so the value is trusted to be one of the session's own
/// `dir`s — the daemon wrote them — but the key is still built from the
/// session id rather than from the caller's claim, which is what keeps the
/// scope airtight. Absent names the workspace root itself: the
/// developer-machine shape, where the workdir is the checkout and no `dir`
/// exists.
#[derive(Debug, Deserialize, skyzen::ToSchema)]
struct PatchQuery {
    /// The checkout's directory under the workdir.
    repo: Option<String>,
}

/// Stores the uncommitted diff of one checkout of a session about to be
/// archived automatically.
#[skyzen::openapi]
async fn put_workdir_patch(
    State(session): State<DaemonSession>,
    Query(query): Query<PatchQuery>,
    body: Bytes,
    storage: Storage,
) -> Outcome<NoContent> {
    store_workdir_patch(session.0, query.repo.as_deref(), body, &storage)
        .await
        .into()
}

async fn store_workdir_patch(
    session: SessionId,
    dir: Option<&str>,
    body: Bytes,
    storage: &Storage,
) -> Result<NoContent, ApiError> {
    workdirs::put(storage, session, dir.unwrap_or("."), body.to_vec()).await?;
    Ok(NoContent)
}

/// Reads a previously stored uncommitted diff of one checkout, for a resume
/// onto a new host.
#[skyzen::openapi]
async fn get_workdir_patch(
    State(session): State<DaemonSession>,
    Query(query): Query<PatchQuery>,
    storage: Storage,
) -> Outcome<Response> {
    read_workdir_patch(session.0, query.repo.as_deref(), &storage)
        .await
        .into()
}

async fn read_workdir_patch(
    session: SessionId,
    dir: Option<&str>,
    storage: &Storage,
) -> Result<Response, ApiError> {
    let Some(body) = workdirs::get(storage, session, dir.unwrap_or(".")).await? else {
        return Err(ApiError::SessionNotFound);
    };
    let mut response = Response::new(skyzen::Body::from(body));
    response.headers_mut().insert(
        skyzen::header::CONTENT_TYPE,
        skyzen::header::HeaderValue::from_static(workdirs::CONTENT_TYPE),
    );
    Ok(response)
}

// ── Handoffs ──
//
// A session created with `source.local_handoff` lands in `provisioning`
// with no queue job: these user-scoped routes are what the CLI uploads
// through, and `complete` is what finally frees the machine to build. The
// patch shares the archive's `workdirs/` object, so the daemon's replay
// path reads it unchanged; the transcript lives under `handoffs/`, where
// only these routes and the daemon's two reads can reach it.

/// Stores a handoff's working-tree patch.
#[skyzen::openapi]
async fn put_handoff_patch(
    State(user): State<CurrentUser>,
    params: Params,
    body: Bytes,
    db: Db,
    storage: Storage,
) -> Outcome<NoContent> {
    store_handoff_patch(&user, &params, body, &db, &storage)
        .await
        .into()
}

async fn store_handoff_patch(
    user: &CurrentUser,
    params: &Params,
    body: Bytes,
    db: &Db,
    storage: &Storage,
) -> Result<NoContent, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    handoffs::require_pending(db, user.id, id).await?;
    let bytes = body.len() as u64;
    if bytes > flyco_core::HANDOFF_PATCH_BYTES_MAX {
        return Err(ApiError::HandoffTooLarge {
            object: "patch",
            bytes,
            limit: flyco_core::HANDOFF_PATCH_BYTES_MAX,
        });
    }
    // The patch lands under the primary checkout's directory — the key the
    // daemon reads it back from — because a handoff always describes the
    // session's first repository.
    let primary = session_repos::of_session(db, id)
        .await?
        .into_iter()
        .next()
        .ok_or(ApiError::CorruptRecord("a session with no repository"))?;
    workdirs::put(storage, id, &primary.dir, body.to_vec()).await?;
    // Recorded after the put, never before: a row claiming an upload that
    // never landed would let `complete` pass against a missing object.
    handoffs::record_patch(db, id, &body).await?;
    Ok(NoContent)
}

/// Stores a handoff's full transcript, which the daemon later writes to
/// disk for the cloud harness to consult.
#[skyzen::openapi]
async fn put_handoff_transcript(
    State(user): State<CurrentUser>,
    params: Params,
    body: Bytes,
    db: Db,
    storage: Storage,
) -> Outcome<NoContent> {
    store_handoff_transcript(&user, &params, body, &db, &storage)
        .await
        .into()
}

async fn store_handoff_transcript(
    user: &CurrentUser,
    params: &Params,
    body: Bytes,
    db: &Db,
    storage: &Storage,
) -> Result<NoContent, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    handoffs::require_pending(db, user.id, id).await?;
    let bytes = body.len() as u64;
    if bytes > flyco_core::HANDOFF_TRANSCRIPT_BYTES_MAX {
        return Err(ApiError::HandoffTooLarge {
            object: "transcript",
            bytes,
            limit: flyco_core::HANDOFF_TRANSCRIPT_BYTES_MAX,
        });
    }
    handoffs::put_transcript(storage, id, body.to_vec()).await?;
    handoffs::record_transcript(db, id, &body).await?;
    Ok(NoContent)
}

/// Verifies a handoff's payloads against its manifest and frees the
/// session to provision. Idempotent: a replayed manifest answers as a
/// second call rather than a second machine.
#[skyzen::openapi]
async fn complete_handoff(
    State(user): State<CurrentUser>,
    params: Params,
    Json(manifest): Json<flyco_core::HandoffManifest>,
    db: Db,
    queue: Queue,
) -> Outcome<NoContent> {
    finish_handoff(&user, &params, &manifest, &db, &queue)
        .await
        .into()
}

async fn finish_handoff(
    user: &CurrentUser,
    params: &Params,
    manifest: &flyco_core::HandoffManifest,
    db: &Db,
    queue: &Queue,
) -> Result<NoContent, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    let detail = sessions::find(db, user.id, id).await?;
    // A session that left `provisioning` — archived mid-upload, failed by
    // the stall sweep — can never take a machine, so completing its
    // handoff would mark it finished with nothing to run.
    if detail.summary.state != SessionState::Provisioning {
        return Err(ApiError::InvalidTransition {
            from: detail.summary.state,
            to: SessionState::Provisioning,
        });
    }
    // `false` is a replayed manifest on a finished row: the machine's job
    // was queued by the call that completed it, and a second enqueue would
    // start a second build.
    if handoffs::complete(db, id, manifest).await? {
        let machine = machines::for_session(db, id)
            .await?
            .ok_or(ApiError::CorruptRecord(
                "a session with no machine row reached handoff completion",
            ))?;
        provisioning_queue::enqueue(queue, ProvisioningJob::first(id, machine.id)).await?;
    }
    Ok(NoContent)
}

/// Reads the handoff manifest behind the daemon's session, or 404s when
/// the session is none — the boot path's way to learn there is a patch to
/// apply and a transcript to land.
#[skyzen::openapi]
async fn get_handoff(
    State(session): State<DaemonSession>,
    db: Db,
) -> Outcome<Json<flyco_core::HandoffView>> {
    read_handoff(session.0, &db).await.map(Json).into()
}

async fn read_handoff(session: SessionId, db: &Db) -> Result<flyco_core::HandoffView, ApiError> {
    handoffs::view_for_daemon(db, session)
        .await?
        .ok_or(ApiError::SessionNotFound)
}

/// Streams the handoff's uploaded transcript.
#[skyzen::openapi]
async fn get_handoff_transcript(
    State(session): State<DaemonSession>,
    storage: Storage,
) -> Outcome<Response> {
    read_handoff_transcript(session.0, &storage).await.into()
}

async fn read_handoff_transcript(
    session: SessionId,
    storage: &Storage,
) -> Result<Response, ApiError> {
    let Some(body) = handoffs::get_transcript(storage, session).await? else {
        return Err(ApiError::SessionNotFound);
    };
    let mut response = Response::new(skyzen::Body::from(body));
    // `application/octet-stream` and not anything narrower: the bytes are a
    // Claude JSONL, a Codex rollout, or a Devin ATIF document depending on
    // which harness the session came from.
    response.headers_mut().insert(
        skyzen::header::CONTENT_TYPE,
        skyzen::header::HeaderValue::from_static("application/octet-stream"),
    );
    Ok(response)
}

// ── What the agent is allowed to know and to change ──
//
// The daemon's local MCP server is the only sanctioned way an agent touches
// its own machine (docs/ARCHITECTURE.md), and these four routes are what it
// is made of. They are daemon-scoped rather than user-scoped because the
// agent has no user credential and must never be given one: an `fd_` token
// proves which session is calling, and the owner every read is scoped to is
// derived from that session here rather than taken from the caller.

/// Tells a session's agent which machine it is on and who chose it.
#[skyzen::openapi]
async fn get_agent_machine(
    State(session): State<DaemonSession>,
    db: Db,
) -> Outcome<Json<AgentMachineView>> {
    read_agent_machine(session.0, &db).await.map(Json).into()
}

async fn read_agent_machine(session: SessionId, db: &Db) -> Result<AgentMachineView, ApiError> {
    let user = sessions::owner(db, session).await?;
    machines::agent_view(db, user, session).await
}

/// Lists the machine types this session can be resized to, with prices.
///
/// The curated catalog of docs/ux.md §7.6, narrowed to the account and
/// region the session's disk already lives in — the two a resize cannot
/// cross. The agent reads exactly the list the user's own slider shows.
#[skyzen::openapi]
async fn get_agent_machine_catalog(
    State(session): State<DaemonSession>,
    State(config): State<ApiConfig>,
    State(github): State<GithubClient>,
    db: Db,
    kv: Kv,
) -> Outcome<Json<Vec<MachineCatalogEntry>>> {
    read_agent_catalog(session.0, &config, &github, &db, &kv)
        .await
        .map(Json)
        .into()
}

async fn read_agent_catalog(
    session: SessionId,
    config: &ApiConfig,
    github: &GithubClient,
    db: &Db,
    kv: &Kv,
) -> Result<Vec<MachineCatalogEntry>, ApiError> {
    let user = sessions::owner(db, session).await?;
    machines::resize_catalog(db, config, github, kv, user, session).await
}

/// Moves the session onto another machine type, on the agent's own say-so.
///
/// Refused for a type that bills a minimum the moment it boots: that is the
/// user's money committed before anything runs, so the daemon raises an
/// approval instead and the resize happens when the user decides.
#[skyzen::openapi]
#[expect(
    clippy::too_many_arguments,
    reason = "a resize names the session, the caller, the type, and every \
              service it touches"
)]
async fn agent_resize_machine(
    State(session): State<DaemonSession>,
    State(config): State<ApiConfig>,
    State(github): State<GithubClient>,
    Json(request): Json<ResizeMachine>,
    rooms: Rooms,
    hosts: HostRooms,
    db: Db,
    kv: Kv,
) -> Outcome<Accepted> {
    run_agent_resize(
        session.0, &request, &config, &github, &rooms, &hosts, &db, &kv,
    )
    .await
    .into()
}

#[expect(
    clippy::too_many_arguments,
    reason = "a resize names the session, the caller, the type, and every \
              service it touches"
)]
async fn run_agent_resize(
    session: SessionId,
    request: &ResizeMachine,
    config: &ApiConfig,
    github: &GithubClient,
    rooms: &Rooms,
    hosts: &HostRooms,
    db: &Db,
    kv: &Kv,
) -> Result<Accepted, ApiError> {
    let user = sessions::owner(db, session).await?;
    machines::resize_for_agent(
        db,
        config,
        github,
        kv,
        rooms,
        hosts,
        user,
        session,
        &request.machine_type,
    )
    .await?;
    Ok(Accepted)
}

/// Tells a session's agent what it has spent and what is left.
#[skyzen::openapi]
async fn get_agent_budget(
    State(session): State<DaemonSession>,
    db: Db,
) -> Outcome<Json<BudgetView>> {
    read_agent_budget(session.0, &db).await.map(Json).into()
}

async fn read_agent_budget(session: SessionId, db: &Db) -> Result<BudgetView, ApiError> {
    let user = sessions::owner(db, session).await?;
    Ok(sessions::find(db, user, session).await?.budget)
}

/// Routes that anyone may call.
///
/// Three of them are public because they cannot be anything else: a browser
/// returning from a harness vendor carries no flyco credential, a browser
/// needs the VAPID public key *before* it can subscribe, and GitHub signs
/// its webhook deliveries rather than presenting a bearer token. Each says
/// in its own module how it establishes who is calling.
fn public_routes() -> Vec<RouteNode> {
    let mut nodes = Route::new((
        "/v1/healthz".at(healthz),
        "/install/{artifact}".at(get_release_artifact),
        "/v1/auth/github".route(("/start".post(oauth::start), "/callback".at(oauth::callback))),
    ))
    .into_route_nodes();
    nodes.extend(push::public_routes());
    nodes.extend(webhooks::routes());
    nodes.extend(hosts::public_routes());
    nodes.extend(provider_oauth::public_routes());
    nodes.extend(codespaces::public_routes());
    nodes.extend(cli::public_routes());
    nodes
}

/// The daemon and host relay routes: REST in, SSE out.
///
/// None carries a user credential — the daemon presents its session's
/// `fd_` token, the host its `fh_` token — so they authenticate
/// themselves rather than sitting behind [`RequireAuth`]. The command
/// routes answer with an SSE stream that never ends, which the `OpenAPI`
/// response model cannot describe — see
/// [`responses::UNDECLARED`](crate::responses::UNDECLARED).
fn relay_routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/sessions/{id}/relay/attach".post(attach_daemon),
        "/v1/sessions/{id}/relay/commands".at(open_daemon_commands),
        "/v1/sessions/{id}/relay/frames".post(post_daemon_frames),
        "/v1/hosts/{id}/relay/attach".post(attach_host),
        "/v1/hosts/{id}/relay/commands".at(open_host_commands),
        "/v1/hosts/{id}/relay/frames".post(post_host_frames),
    ))
    .into_route_nodes()
}

/// Routes a session's own daemon calls, authenticated by its `fd_` token.
/// Routes an enrolled machine calls with its own `fh_` token.
///
/// Beside [`daemon_routes`] rather than inside it: an `fd_` token resolves
/// to a session and an `fh_` token to a machine, and neither may ever be
/// widened into the other. Each route here checks the presented token
/// against the host in its own path.
fn host_routes() -> Vec<RouteNode> {
    hosts::host_routes()
}

fn daemon_routes() -> Vec<RouteNode> {
    // Three trees, one middleware: what the daemon reports about the
    // conversation, what it reports about the machine, and what it asks on
    // the agent's behalf. Split because a route tuple holds sixteen and this
    // is more than sixteen routes, so each seam is where the meaning changes
    // rather than wherever the count ran out.
    let conversation = Route::new((
        "/v1/sessions/{id}/approvals".post(raise_approval),
        "/v1/sessions/{id}/harness-session"
            .at(get_harness_session)
            .put(put_harness_session),
        "/v1/sessions/{id}/models".put(report_models),
        "/v1/sessions/{id}/usage".put(report_usage),
        "/v1/sessions/{id}/usage-limit".post(report_usage_limit),
        "/v1/sessions/{id}/harness-observations".post(record_harness_observation),
        "/v1/sessions/{id}/turn-started".post(notify_turn_started),
        "/v1/sessions/{id}/turn-completed".post(notify_turn_completed),
        "/v1/sessions/{id}/turn-failed".post(notify_turn_failed),
        "/v1/sessions/{id}/transcript/{stream}".at(get_transcript),
        "/v1/sessions/{id}/transcript/{stream}/batches/{seq}".put(put_transcript_batch),
    ))
    .middleware(RequireDaemon::new())
    .into_route_nodes();

    let machine = Route::new((
        "/v1/sessions/{id}/spot-notice".post(report_spot_notice),
        "/v1/sessions/{id}/stopping".post(report_stopping),
        "/v1/sessions/{id}/startup-failure".post(report_startup_failure),
        "/v1/sessions/{id}/provisioning-stage".post(report_provisioning_stage),
        "/v1/sessions/{id}/workdir-patch"
            .at(get_workdir_patch)
            .put(put_workdir_patch),
        "/v1/sessions/{id}/handoff".at(get_handoff),
        "/v1/sessions/{id}/handoff/transcript".at(get_handoff_transcript),
    ))
    .middleware(RequireDaemon::new())
    .into_route_nodes();

    let agent = Route::new((
        "/v1/sessions/{id}/agent/machine".at(get_agent_machine),
        "/v1/sessions/{id}/agent/machine/catalog".at(get_agent_machine_catalog),
        "/v1/sessions/{id}/agent/machine/resize".post(agent_resize_machine),
        "/v1/sessions/{id}/agent/budget".at(get_agent_budget),
    ))
    .middleware(RequireDaemon::new())
    .into_route_nodes();

    conversation
        .into_iter()
        .chain(machine)
        .chain(agent)
        .collect()
}

/// The caller's own account, keys, and approvals.
fn account_routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/me".at(me).patch(update_me),
        // The one stream every session event rides: a browser follows the
        // whole account on a single SSE connection.
        "/v1/events".at(open_event_stream),
        "/v1/harness-features".at(list_harness_features),
        "/v1/api-keys".post(create_api_key).get(list_api_keys),
        "/v1/api-keys/{id}".delete(revoke_api_key),
        "/v1/approvals".at(list_approvals),
        "/v1/approvals/{id}/decision".post(decide_approval),
    ))
    .into_route_nodes()
}

/// A session's lifecycle, and driving the agent inside it.
fn session_routes() -> Vec<RouteNode> {
    let mut nodes = Route::new((
        "/v1/sessions".post(create_session).get(list_sessions),
        "/v1/sessions/{id}".at(get_session).patch(update_session),
        "/v1/sessions/{id}/archive".post(archive_session),
        "/v1/sessions/{id}/budget".at(get_session_budget),
        "/v1/sessions/{id}/daemon-token".post(create_daemon_token),
        "/v1/sessions/{id}/events".at(get_session_events),
        "/v1/sessions/{id}/resume".post(resume_session),
        "/v1/sessions/{id}/turns".at(list_turns),
        "/v1/sessions/{id}/env"
            .at(get_session_env)
            .put(put_session_env),
        "/v1/sessions/{id}/handoff/patch".put(put_handoff_patch),
        "/v1/sessions/{id}/handoff/transcript".put(put_handoff_transcript),
        "/v1/sessions/{id}/handoff/complete".post(complete_handoff),
        "/v1/sessions/{id}/repo-status".at(get_repo_status),
        "/v1/sessions/{id}/repos".post(add_session_repo),
    ))
    .into_route_nodes();
    // Split rather than one tuple: a route tree is a tuple, and tuples stop
    // implementing the trait past sixteen elements.
    nodes.extend(driving_routes());
    nodes.extend(checkout_routes());
    nodes
}

/// Sending a session's daemon work: the client commands of
/// `ControlToDaemon::is_client_command`, one REST route each.
fn driving_routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/sessions/{id}/messages".post(send_message),
        "/v1/sessions/{id}/shell".post(run_shell),
        "/v1/sessions/{id}/terminal/input".post(terminal_input),
        "/v1/sessions/{id}/terminal/harness".post(terminal_harness),
        "/v1/sessions/{id}/terminal/resize".post(terminal_resize),
        "/v1/sessions/{id}/interrupt".post(interrupt_session),
        "/v1/sessions/{id}/compact".post(compact_session),
        "/v1/sessions/{id}/context".post(context_session),
        "/v1/sessions/{id}/desktop/stream".at(desktop_stream),
        "/v1/sessions/{id}/desktop/takeover".post(desktop_takeover),
        "/v1/sessions/{id}/desktop/input".post(desktop_input),
    ))
    .into_route_nodes()
}

/// Reading the disk a session works on (docs/ux.md §9.4).
fn checkout_routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/sessions/{id}/files".at(list_session_files),
        "/v1/sessions/{id}/files/content".at(read_session_file),
        "/v1/sessions/{id}/diff".at(get_session_diff),
    ))
    .into_route_nodes()
}

/// Routes that require a bearer credential.
///
/// One middleware for the whole set: every route below answers to a
/// `CurrentUser` and to nothing else, so authentication is applied once
/// here rather than per domain, where a module could forget it.
fn authenticated_routes() -> Vec<RouteNode> {
    let mut nodes = account_routes();
    nodes.extend(session_routes());
    nodes.extend(agents_md::routes());
    nodes.extend(claude_oauth::routes());
    nodes.extend(cli::routes());
    nodes.extend(codex_oauth::routes());
    nodes.extend(harness_accounts::routes());
    nodes.extend(hosts::routes());
    nodes.extend(machines::routes());
    nodes.extend(mcp::routes());
    nodes.extend(memory::routes());
    nodes.extend(provider_accounts::routes());
    nodes.extend(provider_oauth::routes());
    nodes.extend(push::routes());
    nodes.extend(repos::routes());
    nodes.extend(skills::routes());
    Route::new(nodes)
        .middleware(RequireAuth::new(FlycoAuthenticator::new()))
        .into_route_nodes()
}

/// The complete route tree, before state or middleware is attached.
///
/// Separated from [`router`] so the `OpenAPI` export can describe the API
/// without opening a database or reading any configuration.
fn routes() -> Route {
    let mut nodes = public_routes();
    nodes.extend(relay_routes());
    nodes.extend(host_routes());
    nodes.extend(daemon_routes());
    nodes.extend(authenticated_routes());
    Route::new(nodes)
}

/// The PWA, compiled into the Worker so the control plane and the UI share
/// an origin. Cloudflare Assets is not a skyzen capability; the files ride
/// the wasm binary instead.
fn frontend() -> EmbeddedStaticDir {
    static ASSETS: skyzen::include_dir::Dir<'static> =
        skyzen::embed_dir!("$CARGO_MANIFEST_DIR/../../frontend/dist");
    EmbeddedStaticDir::new("/", &ASSETS)
        .index_file("index.html")
        .spa()
}

/// Gives the route tree somewhere to find session rooms.
///
/// On the Worker a room is resolved from the `SESSION_ROOMS` binding the
/// runtime already put in request extensions, so there is nothing to
/// attach. Natively the simulator's namespace is process-local state and
/// has to be carried: one namespace per router, so two routers in one test
/// binary do not share rooms.
#[cfg(not(target_arch = "wasm32"))]
fn with_rooms(route: Route) -> Route {
    route
        .with(State(crate::rooms::NativeRooms::new()))
        .with(State(crate::rooms::NativeHostRooms::new()))
        .with(State(crate::rooms::NativeUserStreams::new()))
}

/// See the native counterpart above.
#[cfg(target_arch = "wasm32")]
const fn with_rooms(route: Route) -> Route {
    route
}

/// The `OpenAPI` document describing the control plane.
///
/// Only debug native builds collect handler metadata (skyzen gathers it
/// through a `linkme` slice that is compiled out otherwise), so the export
/// binary must be built in debug.
///
/// # Panics
///
/// Panics when that metadata was compiled out, because a document with no
/// paths in it would be checked in as if it described the API.
#[must_use]
pub fn openapi_document() -> utoipa::openapi::OpenApi {
    let collected = routes().openapi();
    assert!(
        collected.is_enabled(),
        "OpenAPI collection is compiled out; build this in debug on a native target"
    );

    let mut spec = collected.to_utoipa_spec();
    // Skyzen stamps its own crate name and version into `info`; the document
    // describes flyco's API, whose version is the `/v1` prefix. Using the
    // crate version instead would churn the checked-in file on every release.
    spec.info = utoipa::openapi::Info::new("Flyco control plane", "v1");
    responses::finish(&mut spec);
    spec
}

/// Builds the control-plane router around an explicit configuration, vendor
/// clients, database, and provisioning queue.
///
/// All of them are injected rather than discovered, so tests can drive the
/// OAuth flows without reaching `github.com`, `console.anthropic.com`, or a
/// real D1, and can read back the jobs a session creation enqueued. In the
/// deployed control plane the database and the queue arrive the way the KV
/// namespace and the R2 bucket do — they are declared in `Skyzen.toml`, and
/// `#[skyzen::main]` wraps the router with them — which is what
/// [`router_from_environment`] builds instead.
#[must_use]
pub fn router(
    config: ApiConfig,
    github: GithubClient,
    vendors: Vendors,
    clouds: Clouds,
    codespaces: Codespaces,
    db: Db,
    queue: Queue,
) -> Router {
    configured(config, github, vendors, clouds, codespaces)
        .with(db)
        .with(queue)
        .build()
}

/// The router without the database and queue the declared `[[database]]`
/// and `[[service]]` entries supply.
///
/// The two vendor clients are injected separately as well as together: a
/// handler that only redeems Anthropic's grant asks for
/// [`ClaudeClient`], the Codex routes ask for [`CodexClient`], and the one
/// place that may renew either — the provisioning consumer — asks for
/// [`Vendors`].
fn configured(
    config: ApiConfig,
    github: GithubClient,
    vendors: Vendors,
    clouds: Clouds,
    codespaces: Codespaces,
) -> Route {
    with_error_handling(
        with_rooms(Route::new((routes(), frontend())))
            .with(State(config))
            .with(State(github))
            .with(State(clouds))
            .with(State(codespaces))
            .with(State(vendors.claude.clone()))
            .with(State(vendors.codex.clone()))
            .with(State(vendors.microsoft.clone()))
            .with(State(vendors.google.clone()))
            .with(State(vendors)),
    )
}

/// Worker path: configuration is read from the request's `env`, not at
/// isolate startup. See [`crate::middleware::LoadApiConfig`].
#[cfg(target_arch = "wasm32")]
fn configured_from_request(
    github: GithubClient,
    vendors: Vendors,
    clouds: Clouds,
    codespaces: Codespaces,
) -> Route {
    with_error_handling(
        with_rooms(Route::new((routes(), frontend())))
            .with(crate::middleware::LoadApiConfig)
            .with(State(github))
            .with(State(clouds))
            .with(State(codespaces))
            .with(State(vendors.claude.clone()))
            .with(State(vendors.codex.clone()))
            .with(State(vendors.microsoft.clone()))
            .with(State(vendors.google.clone()))
            .with(State(vendors)),
    )
}

fn with_error_handling(route: Route) -> Route {
    // Outermost, so extractor and routing failures answer in the same
    // shape flyco's own errors do.
    route.with(ErrorHandlingMiddleware::new(
        |error: skyzen::BoxHttpError| async move {
            let status = error.status();
            let title = status.canonical_reason().unwrap_or("Error");
            let detail = if status.is_server_error() {
                tracing::error!(%error, "request failed");
                "The control plane failed to handle this request.".to_owned()
            } else {
                error.to_string()
            };
            problem::response(
                &flyco_core::Problem::about_blank(status.as_u16(), title, detail),
                None,
            )
        },
    ))
}

/// Builds the router the deployed control plane runs.
///
/// # Panics
///
/// On native targets, panics if any required configuration binding is
/// missing or malformed — a misconfigured control plane must fail at
/// startup, not at the first sign-in attempt. On the Worker the `env` is
/// not published during router construction, so missing bindings fail the
/// request that needs them instead. The database is the manifest's to
/// open, and its own failure to resolve panics there for the same reason.
#[must_use]
pub fn router_from_environment() -> Router {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let config = ApiConfig::from_environment()
            .unwrap_or_else(|error| panic!("flyco control plane is misconfigured: {error}"));
        configured(
            config,
            GithubClient::default(),
            Vendors::default(),
            Clouds::default(),
            Codespaces::default(),
        )
        .build()
    }
    #[cfg(target_arch = "wasm32")]
    {
        configured_from_request(
            GithubClient::default(),
            Vendors::default(),
            Clouds::default(),
            Codespaces::default(),
        )
        .build()
    }
}

#[cfg(test)]
mod tests {
    use flyco_core::{ApiKeySummary, CreateApiKey, CreatedApiKey, CurrentUser, Problem};
    use skyzen_services::{Db, Kv, Queue};
    use skyzen_test::TestContext;

    use crate::testing::{GITHUB_LOGIN, migrated_router, seed_user, test_router};
    use crate::{api_keys, session};

    #[skyzen::test]
    async fn healthz_reports_protocol_version(ctx: TestContext, db: Db, queue: Queue) {
        let client = ctx.client(test_router(db, queue));
        let response = client.get("/v1/healthz").send().await;
        response.assert_status(200);
        let body: serde_json::Value = response.json();
        assert_eq!(
            body["wire_protocol_version"],
            u64::from(flyco_core::WIRE_PROTOCOL_VERSION)
        );
    }

    #[skyzen::test]
    async fn the_spa_is_served_at_the_root(ctx: TestContext, db: Db, queue: Queue) {
        let client = ctx.client(test_router(db, queue));

        let root = client.get("/").send().await;
        root.assert_status(200);
        assert!(
            root.body_text().contains(r#"id="app""#),
            "the root document must mount the PWA"
        );

        let login = client.get("/login").send().await;
        login.assert_status(200);
        assert_eq!(login.body_text(), root.body_text());
    }

    #[skyzen::test]
    async fn an_anonymous_request_is_challenged(ctx: TestContext, _kv: Kv, db: Db) {
        let response = ctx
            .client(migrated_router(&db).await)
            .get("/v1/me")
            .send()
            .await;

        response.assert_status(401);
        response.assert_header("content-type", "application/problem+json");
        response.assert_header("www-authenticate", "Bearer");

        let problem: Problem = response.json();
        assert_eq!(problem.status, 401);
        assert_eq!(
            problem.kind,
            "https://flyco.dev/problems/missing-credential"
        );
    }

    #[skyzen::test]
    async fn a_rejected_credential_is_named_as_such(ctx: TestContext, _kv: Kv, db: Db) {
        let client = ctx.client(migrated_router(&db).await);

        for token in [
            "fk_not-a-real-key",
            "fs_not-a-real-session",
            "neither-prefix",
        ] {
            let response = client.get("/v1/me").bearer(token).send().await;
            response.assert_status(401);
            response.assert_header("www-authenticate", "Bearer error=\"invalid_token\"");
            assert_eq!(
                response.json::<Problem>().kind,
                "https://flyco.dev/problems/invalid-credential"
            );
        }
    }

    #[skyzen::test]
    async fn a_non_bearer_scheme_is_treated_as_no_credential(ctx: TestContext, _kv: Kv, db: Db) {
        let response = ctx
            .client(migrated_router(&db).await)
            .get("/v1/me")
            .header("Authorization", "Basic Zm9vOmJhcg==")
            .send()
            .await;

        response.assert_status(401);
        response.assert_header("www-authenticate", "Bearer");
    }

    #[skyzen::test]
    async fn me_answers_for_a_session_token(ctx: TestContext, kv: Kv, db: Db) {
        let router = migrated_router(&db).await;
        let user = seed_user(&db).await;
        let token = session::issue(&kv, user.id).await.expect("issue a session");
        assert!(token.starts_with(session::TOKEN_PREFIX));

        let response = ctx.client(router).get("/v1/me").bearer(&token).send().await;

        response.assert_status(200);
        assert_eq!(response.json::<CurrentUser>(), user);
    }

    #[skyzen::test]
    async fn me_answers_for_an_api_key(ctx: TestContext, _kv: Kv, db: Db) {
        let router = migrated_router(&db).await;
        let user = seed_user(&db).await;
        let key = api_keys::create(&db, user.id, "ci".to_owned())
            .await
            .expect("mint a key");

        let response = ctx
            .client(router)
            .get("/v1/me")
            .bearer(&key.token)
            .send()
            .await;

        response.assert_status(200);
        assert_eq!(response.json::<CurrentUser>().login, GITHUB_LOGIN);

        let listed = api_keys::list(&db, user.id).await.expect("list");
        assert!(
            listed[0].last_used_unix.is_some(),
            "authenticating with a key stamps it as used"
        );
    }

    #[skyzen::test]
    async fn api_keys_round_trip(ctx: TestContext, kv: Kv, db: Db) {
        let router = migrated_router(&db).await;
        let user = seed_user(&db).await;
        let token = session::issue(&kv, user.id).await.expect("issue a session");
        let client = ctx.client(router);

        let created = client
            .post("/v1/api-keys")
            .bearer(&token)
            .json(&CreateApiKey {
                label: "laptop".to_owned(),
            })
            .send()
            .await;
        created.assert_status(200);
        let created: CreatedApiKey = created.json();
        assert!(created.token.starts_with(api_keys::TOKEN_PREFIX));
        assert_eq!(created.label, "laptop");

        // D1 keeps the hash and nothing else, so the plaintext key cannot be
        // recovered from the table.
        let stored: String = skyzen::sql!(
            db,
            "SELECT token_hash FROM api_keys WHERE id = {created.id}"
        )
        .fetch_scalar()
        .await
        .expect("read the stored key");
        assert_eq!(stored, crate::crypto::token_hash(&created.token));
        assert!(!stored.contains(&created.token));

        let listed = client.get("/v1/api-keys").bearer(&token).send().await;
        listed.assert_status(200);
        let listed: Vec<ApiKeySummary> = listed.json();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, created.id);
        assert_eq!(listed[0].last_used_unix, None);

        let path = format!("/v1/api-keys/{}", created.id);
        client
            .delete(&path)
            .bearer(&token)
            .send()
            .await
            .assert_status(204);

        let gone = client.delete(&path).bearer(&token).send().await;
        gone.assert_status(404);
        assert_eq!(
            gone.json::<Problem>().kind,
            "https://flyco.dev/problems/api-key-not-found"
        );

        let listed = client.get("/v1/api-keys").bearer(&token).send().await;
        assert_eq!(
            listed.json::<Vec<ApiKeySummary>>(),
            [] as [ApiKeySummary; 0]
        );
    }

    #[skyzen::test]
    async fn a_malformed_key_id_is_a_bad_request(ctx: TestContext, kv: Kv, db: Db) {
        let router = migrated_router(&db).await;
        let user = seed_user(&db).await;
        let token = session::issue(&kv, user.id).await.expect("issue a session");

        let response = ctx
            .client(router)
            .delete("/v1/api-keys/not-a-uuid")
            .bearer(&token)
            .send()
            .await;

        response.assert_status(400);
        assert_eq!(
            response.json::<Problem>().kind,
            "https://flyco.dev/problems/malformed-id"
        );
    }

    #[skyzen::test]
    async fn an_extractor_failure_still_answers_with_a_problem(ctx: TestContext, kv: Kv, db: Db) {
        let router = migrated_router(&db).await;
        let user = seed_user(&db).await;
        let token = session::issue(&kv, user.id).await.expect("issue a session");

        // No `Content-Type: application/json`, so the body extractor refuses.
        let response = ctx
            .client(router)
            .post("/v1/api-keys")
            .bearer(&token)
            .body("{}")
            .send()
            .await;

        response.assert_status(400);
        response.assert_header("content-type", "application/problem+json");
        assert_eq!(response.json::<Problem>().kind, "about:blank");
    }
}
