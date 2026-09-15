//! Linking a cloud account by signing in with it.
//!
//! Four routes per vendor, and between them the user does one thing: approve
//! a consent screen. [`azure_start`] mints an attempt and hands back an
//! authorize URL; the vendor returns the browser to [`azure_callback`],
//! which redeems the code and records who signed in and what they may link;
//! the page [`azure_poll`]s until that lands; and [`azure_finish`] creates
//! the credential inside the account the user chose and links it. Google is
//! the same four with different nouns.
//!
//! **Why an attempt rather than a cookie.** The callback is a browser
//! navigation from `login.microsoftonline.com`, so it carries no flyco
//! credential at all — it is as public as
//! [`crate::oauth::callback`]. The `state` is therefore the only thing
//! tying the returning browser to the sign-in that started, and it is kept
//! in the key-value store beside the attempt: `state → attempt_id` written
//! at the start and *taken* at the callback, so a state redeems exactly
//! once, and the attempt itself keyed by its own opaque id, which is what
//! the signed-in page polls with. The attempt is bound to the user who
//! started it, so one signed-in user cannot finish another's sign-in.
//!
//! **Why the callback always redirects.** Its caller is a browser mid-
//! navigation, and a problem document rendered into a tab is a dead end. So
//! every outcome — including every failure — is a `303` to the SPA's return
//! page, carrying the provider and, when something went wrong, the problem
//! slug the page explains.
//!
//! The vendor tokens the callback obtained live in the attempt until the
//! finish spends them. They are secrets: nothing here returns them, and the
//! attempt's `Debug` shows neither.
#![expect(
    clippy::too_many_arguments,
    reason = "a finish handler takes four injected services, the path, the body and two \
              stores; bundling them would be a struct that exists only to shorten one \
              signature"
)]

use flyco_core::{
    CurrentUser, FinishAzureOauth, FinishCodespacesOauth, FinishGcpOauth, LinkProvider,
    ProviderAccountView, ProviderCredentials, ProviderOauthAttemptId, ProviderOauthChoice,
    ProviderOauthProgress, ProviderOauthStart, UserId,
};
use serde::{Deserialize, Serialize};
use skyzen::extract::Query;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::utils::{Json, State};
use skyzen_services::{Db, Kv, Queue};
use url::Url;

use crate::clouds::Clouds;
use crate::codespaces::{Codespaces, CodespacesLink as _};
use crate::config::ApiConfig;
use crate::crypto::random_token;
use crate::error::ApiError;
use crate::expiring;
use crate::extract::path_id;
use crate::github::{
    CODESPACE_SCOPE, GithubClient, GithubGrant, GithubOauth as _, GithubToken, REPO_SCOPE, SCOPE,
};
use crate::google::{self, GoogleClient, GoogleOauth as _};
use crate::microsoft::{self, AzureTokens, MicrosoftClient, MicrosoftOauth as _};
use crate::problem::Outcome;
use crate::provider_accounts;
use crate::respond::{Created, SeeOther};
use crate::users;

/// Where Microsoft returns the browser.
///
/// One constant for the route and for the redirect URI the authorize URL
/// carries, so the two cannot drift into a sign-in that comes back to a
/// path nothing serves.
pub const AZURE_CALLBACK_PATH: &str = "/v1/providers/azure/oauth/callback";

/// Where Google returns the browser.
pub const GCP_CALLBACK_PATH: &str = "/v1/providers/gcp/oauth/callback";

/// Where the SPA takes over once the browser is back.
const RETURN_PATH: &str = "/connect/return";

/// Names the vendor on the return page.
const PROVIDER_PARAM: &str = "provider";

/// Names what went wrong on the return page, when something did.
const PROBLEM_PARAM: &str = "problem";

/// Carries the vendor's own words about it, when there are any.
const REASON_PARAM: &str = "reason";

/// How long a browser has to complete the round trip.
///
/// Longer than the GitHub sign-in's ten minutes: a first sign-in at a cloud
/// vendor routinely runs through a password, a second factor, an account
/// picker and a consent screen, and the first live run expired at exactly
/// ten minutes with the user still on the vendor's pages. Before the
/// callback the attempt holds nothing but a state; after it, the vendor
/// tokens sit in the store only until the finish spends them.
const ATTEMPT_TTL_SECONDS: u64 = 30 * 60;

/// Prefix every attempt is stored under.
const ATTEMPT_KEY_PREFIX: &str = "auth:provider-oauth:";

/// Prefix the `state → attempt` pointer is stored under.
const STATE_KEY_PREFIX: &str = "auth:provider-oauth-state:";

/// Which vendor one sign-in belongs to.
///
/// Carried in the attempt as well as in the path so a Google callback cannot
/// complete an Azure sign-in that happens to share a state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Provider {
    /// Microsoft, for an Azure subscription.
    Azure,
    /// Google, for a GCP project.
    Gcp,
    /// GitHub, for a Codespaces account.
    Codespaces,
}

impl Provider {
    /// How the return page and the route paths name it.
    const fn slug(self) -> &'static str {
        match self {
            Self::Azure => "azure",
            Self::Gcp => "gcp",
            Self::Codespaces => "codespaces",
        }
    }

    /// This vendor's refusal, as the problem a caller sees.
    const fn rejected(self, reason: String) -> ApiError {
        match self {
            Self::Azure => ApiError::MicrosoftRejected { reason },
            Self::Gcp => ApiError::GoogleRejected { reason },
            Self::Codespaces => ApiError::GithubRejected { reason },
        }
    }
}

/// The vendor tokens one authorized attempt holds.
///
/// Secrets by construction: they act as the user inside their own cloud
/// account until the finish spends them, so this value is never returned to
/// anybody and never rendered.
#[derive(Serialize, Deserialize)]
enum Secrets {
    /// Microsoft's pair, plus the directory they act in.
    Azure(AzureTokens),
    /// Google's single short-lived token.
    Gcp(String),
    /// GitHub's OAuth grant, plus what `GET /user` answered beside it.
    Codespaces {
        /// The granted token — the account credential in full.
        token: String,
        /// What the grant is renewed with, when the OAuth app expires user
        /// tokens — `None` for an attempt recorded before flyco kept it.
        #[serde(default)]
        refresh_token: Option<String>,
        /// When `token` stops working, seconds since the Unix epoch.
        #[serde(default)]
        token_expires_at_unix: Option<u64>,
        /// The account's immutable id, which `verify` re-checks on every
        /// link so a renamed login cannot slip a credential onto the wrong
        /// account.
        owner_id: i64,
        /// The plan's included core-hours, read at the callback rather
        /// than at the finish so the stored grant is the one the user was
        /// told they had.
        included_core_hours: u32,
    },
}

impl core::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Azure(_) => f.write_str("Secrets::Azure(..)"),
            Self::Gcp(_) => f.write_str("Secrets::Gcp(..)"),
            Self::Codespaces { .. } => f.write_str("Secrets::Codespaces(..)"),
        }
    }
}

/// How far one sign-in has got.
#[derive(Debug, Serialize, Deserialize)]
enum Stage {
    /// The browser has not come back from the vendor yet.
    Started,
    /// The vendor said who signed in and what they may link.
    Authorized {
        /// The account that signed in, as the vendor names it.
        account: String,
        /// What that account may link.
        choices: Vec<ProviderOauthChoice>,
        /// What the finish will act with.
        secrets: Secrets,
    },
    /// The browser came back without a sign-in. Kept so the page polling
    /// this attempt learns it is over, with the same problem the return
    /// page was sent.
    Failed {
        /// The problem slug.
        problem: String,
        /// The vendor's own words.
        reason: String,
    },
}

/// One cloud sign-in in flight, as the key-value store holds it.
#[derive(Serialize, Deserialize)]
struct Attempt {
    /// Who started it. An attempt polled or finished by anybody else is not
    /// this attempt.
    user: UserId,
    /// Which vendor it is with.
    provider: Provider,
    /// The `state` published in the authorize URL.
    state: String,
    /// How far it has got.
    stage: Stage,
}

impl core::fmt::Debug for Attempt {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Attempt")
            .field("user", &self.user)
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}

/// Where one attempt lives in the key-value store.
fn attempt_key(id: ProviderOauthAttemptId) -> String {
    let rendered = id.to_string();
    let mut key = String::with_capacity(ATTEMPT_KEY_PREFIX.len() + rendered.len());
    key.push_str(ATTEMPT_KEY_PREFIX);
    key.push_str(&rendered);
    key
}

/// Where the pointer from a published `state` to its attempt lives.
fn state_key(state: &str) -> String {
    let mut key = String::with_capacity(STATE_KEY_PREFIX.len() + state.len());
    key.push_str(STATE_KEY_PREFIX);
    key.push_str(state);
    key
}

/// The query string a vendor appends when it returns the browser.
///
/// Every field but `state` is optional because a refusal is a redirect too:
/// a user who closes the consent screen comes back with `error` and no
/// `code`, and that has to reach the return page as a problem rather than
/// as an extractor failure.
#[derive(Debug, Deserialize, skyzen::ToSchema)]
pub struct ProviderCallback {
    /// The single-use authorization code.
    #[serde(default)]
    code: Option<String>,
    /// The `state` this control plane minted in the matching `start`.
    // `pub(crate)` because `crate::oauth::complete` reads it for the
    // sign-in half of the shared GitHub callback.
    pub(crate) state: String,
    /// The vendor's machine-readable refusal, when it refused.
    #[serde(default)]
    error: Option<String>,
    /// Its explanation.
    #[serde(default)]
    error_description: Option<String>,
}

impl ProviderCallback {
    /// The code, or the vendor's own reason there is none.
    fn code(&self, provider: Provider) -> Result<&str, ApiError> {
        if let Some(refusal) = self.error.as_deref() {
            let reason = self.error_description.as_deref().map_or_else(
                || refusal.to_owned(),
                |description| {
                    let mut reason = String::with_capacity(refusal.len() + description.len() + 2);
                    reason.push_str(refusal);
                    reason.push_str(": ");
                    reason.push_str(description);
                    reason
                },
            );
            return Err(provider.rejected(reason));
        }
        self.code.as_deref().ok_or_else(|| {
            provider.rejected("the sign-in came back with neither a code nor a reason".to_owned())
        })
    }

    /// The code, or GitHub's own reason there is none.
    ///
    /// [`code`](Self::code) without naming a provider, for the one caller
    /// that is not a link road: the shared GitHub callback's sign-in half,
    /// whose refusal is the same `GithubRejected`.
    pub(crate) fn github_code(&self) -> Result<&str, ApiError> {
        self.code(Provider::Codespaces)
    }
}

/// Where the browser is sent once the vendor is done with it.
///
/// The SPA's return route on the callback's own origin, naming the provider
/// and — when something went wrong — the problem to explain.
fn return_url(config: &ApiConfig, provider: Provider, problem: Option<&ApiError>) -> Url {
    let mut url = config
        .redirect_uri()
        .join(RETURN_PATH)
        .expect("a rooted path always resolves against an absolute redirect URI");
    {
        let mut query = url.query_pairs_mut();
        query.append_pair(PROVIDER_PARAM, provider.slug());
        if let Some(problem) = problem {
            query.append_pair(PROBLEM_PARAM, problem.slug());
            query.append_pair(REASON_PARAM, &problem.to_string());
        }
    }
    url
}

/// Turns whatever the callback did into the one answer a browser can use.
fn returned(config: &ApiConfig, provider: Provider, outcome: Result<(), ApiError>) -> SeeOther {
    match outcome {
        Ok(()) => {
            tracing::info!(
                provider = provider.slug(),
                "a cloud sign-in came back authorized"
            );
            SeeOther(return_url(config, provider, None))
        }
        Err(error) => {
            tracing::warn!(%error, provider = provider.slug(), "a cloud sign-in did not complete");
            SeeOther(return_url(config, provider, Some(&error)))
        }
    }
}

/// Mints an attempt and the authorize URL that belongs to it.
async fn begin(
    config: &ApiConfig,
    kv: &Kv,
    user: UserId,
    provider: Provider,
) -> Result<ProviderOauthStart, ApiError> {
    let state = random_token()?;
    let attempt_id = ProviderOauthAttemptId::generate();

    let authorize_url = match provider {
        Provider::Azure => microsoft::authorize_url(
            config.azure_oauth_client_id(),
            config.azure_oauth_redirect_uri().as_str(),
            &state,
        ),
        Provider::Gcp => google::authorize_url(
            config.google_oauth_client_id(),
            config.gcp_oauth_redirect_uri().as_str(),
            &state,
        ),
        Provider::Codespaces => crate::oauth::authorize_url(
            config.github_client_id(),
            // The sign-in's own callback: the OAuth app registers one URI
            // per hostname, and which flow a return belongs to is a
            // question the `state` answers, not the path it lands on.
            config.redirect_uri(),
            SCOPE,
            &state,
        ),
    };

    expiring::put(
        kv,
        &attempt_key(attempt_id),
        &Attempt {
            user,
            provider,
            state: state.clone(),
            stage: Stage::Started,
        },
        ATTEMPT_TTL_SECONDS,
    )
    .await?;
    // The pointer the public callback resolves: it has no user, no attempt
    // id and nothing but the state the vendor echoed back.
    expiring::put(kv, &state_key(&state), &attempt_id, ATTEMPT_TTL_SECONDS).await?;

    tracing::debug!(provider = provider.slug(), "issued a cloud authorize URL");
    Ok(ProviderOauthStart {
        attempt_id,
        authorize_url: authorize_url.into(),
    })
}

/// Resolves the state a callback carries to the attempt that minted it.
///
/// The pointer is *taken*, so a state redeems exactly once however many
/// times the browser reloads.
async fn by_state(kv: &Kv, provider: Provider, state: &str) -> Result<(String, Attempt), ApiError> {
    let attempt_id = expiring::take::<ProviderOauthAttemptId>(kv, &state_key(state))
        .await?
        .ok_or(ApiError::UnknownOauthState)?;

    let key = attempt_key(attempt_id);
    let attempt = expiring::get::<Attempt>(kv, &key)
        .await?
        .filter(|attempt| attempt.provider == provider && attempt.state == state)
        .ok_or(ApiError::UnknownOauthState)?;
    Ok((key, attempt))
}

/// Reads the caller's own attempt, whatever stage it is at.
///
/// An attempt that is unknown, expired, another vendor's or another user's
/// is one answer, deliberately: the attempt id is the only thing that names
/// an attempt, and telling a caller which of the four theirs is would be a
/// free oracle over somebody else's sign-in.
async fn mine(
    kv: &Kv,
    user: UserId,
    provider: Provider,
    attempt_id: ProviderOauthAttemptId,
) -> Result<(String, Attempt), ApiError> {
    let key = attempt_key(attempt_id);
    let attempt = expiring::get::<Attempt>(kv, &key)
        .await?
        .filter(|attempt| attempt.user == user && attempt.provider == provider)
        .ok_or(ApiError::ProviderOauthAttemptExpired)?;
    Ok((key, attempt))
}

/// One poll of a sign-in. Reads, never consumes.
async fn progress(
    kv: &Kv,
    user: UserId,
    provider: Provider,
    attempt_id: ProviderOauthAttemptId,
) -> Result<ProviderOauthProgress, ApiError> {
    let (_, attempt) = mine(kv, user, provider, attempt_id).await?;
    Ok(match attempt.stage {
        Stage::Started => ProviderOauthProgress::Pending,
        Stage::Authorized {
            account, choices, ..
        } => ProviderOauthProgress::Authorized { account, choices },
        Stage::Failed { problem, reason } => ProviderOauthProgress::Failed { problem, reason },
    })
}

/// Marks an attempt as over, so the poll stops waiting.
///
/// The failure is recorded beside the attempt rather than by deleting it:
/// a deleted attempt polls as expired, which tells the user to start again
/// without telling them why the last one ended.
async fn record_failure(
    kv: &Kv,
    key: &str,
    mut attempt: Attempt,
    error: &ApiError,
) -> Result<(), ApiError> {
    attempt.stage = Stage::Failed {
        problem: error.slug().to_owned(),
        reason: error.to_string(),
    };
    expiring::put(kv, key, &attempt, ATTEMPT_TTL_SECONDS).await?;
    Ok(())
}

/// What an authorized attempt holds, for the finish that spends it.
struct Authorized {
    /// Where the attempt lives, so the finish can delete it.
    key: String,
    /// The account that signed in.
    account: String,
    /// What it may link.
    choices: Vec<ProviderOauthChoice>,
    /// What to act with.
    secrets: Secrets,
}

/// Reads the caller's attempt, insisting the callback has already been.
async fn authorized(
    kv: &Kv,
    user: UserId,
    provider: Provider,
    attempt_id: ProviderOauthAttemptId,
) -> Result<Authorized, ApiError> {
    let (key, attempt) = mine(kv, user, provider, attempt_id).await?;
    let Stage::Authorized {
        account,
        choices,
        secrets,
    } = attempt.stage
    else {
        return Err(ApiError::ProviderOauthNotAuthorized);
    };
    Ok(Authorized {
        key,
        account,
        choices,
        secrets,
    })
}

/// What to call the account, given what the user chose.
///
/// The vendor's own name for the subscription or project, which is what the
/// user recognises in a list. An id the vendor did not list is still a name:
/// whether it can be acted on is the vendor's answer, not this function's.
fn chosen_label(choices: &[ProviderOauthChoice], id: &str) -> String {
    choices
        .iter()
        .find(|choice| choice.id == id)
        .map_or_else(|| id.to_owned(), |choice| choice.name.clone())
}

// ── Azure ──

/// `POST /v1/providers/azure/oauth/start` — begins a Microsoft sign-in.
#[skyzen::openapi]
pub async fn azure_start(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    kv: Kv,
) -> Outcome<Json<ProviderOauthStart>> {
    begin(&config, &kv, user.id, Provider::Azure)
        .await
        .map(Json)
        .into()
}

/// `GET /v1/providers/azure/oauth/callback` — records what Microsoft said.
///
/// Public, because the browser arrives from `login.microsoftonline.com` with
/// no flyco credential. It authenticates itself with the `state` it carries.
#[skyzen::openapi]
pub async fn azure_callback(
    Query(callback): Query<ProviderCallback>,
    State(config): State<ApiConfig>,
    State(microsoft): State<MicrosoftClient>,
    kv: Kv,
) -> Outcome<SeeOther> {
    let outcome = record_azure(&config, &microsoft, &kv, &callback).await;
    Ok(returned(&config, Provider::Azure, outcome)).into()
}

/// Redeems what Microsoft sent back, or says why it cannot.
async fn sign_in_azure(
    config: &ApiConfig,
    microsoft: &MicrosoftClient,
    callback: &ProviderCallback,
) -> Result<microsoft::SignIn, ApiError> {
    Ok(microsoft
        .sign_in(
            microsoft::OauthClient {
                id: config.azure_oauth_client_id(),
                secret: config.azure_oauth_client_secret(),
            },
            callback.code(Provider::Azure)?,
            config.azure_oauth_redirect_uri().as_str(),
        )
        .await?)
}

async fn record_azure(
    config: &ApiConfig,
    microsoft: &MicrosoftClient,
    kv: &Kv,
    callback: &ProviderCallback,
) -> Result<(), ApiError> {
    let (key, mut attempt) = by_state(kv, Provider::Azure, &callback.state).await?;
    let signed_in = match sign_in_azure(config, microsoft, callback).await {
        Ok(signed_in) => signed_in,
        Err(error) => {
            record_failure(kv, &key, attempt, &error).await?;
            return Err(error);
        }
    };

    attempt.stage = Stage::Authorized {
        account: signed_in.account,
        choices: signed_in.choices,
        secrets: Secrets::Azure(signed_in.tokens),
    };
    expiring::put(kv, &key, &attempt, ATTEMPT_TTL_SECONDS).await?;
    Ok(())
}

/// `GET /v1/providers/azure/oauth/{attempt_id}` — polls it once.
#[skyzen::openapi]
pub async fn azure_poll(
    State(user): State<CurrentUser>,
    params: Params,
    kv: Kv,
) -> Outcome<Json<ProviderOauthProgress>> {
    let attempt_id = match path_id(&params, "attempt_id") {
        Ok(id) => id,
        Err(error) => return Err(error).into(),
    };
    progress(&kv, user.id, Provider::Azure, attempt_id)
        .await
        .map(Json)
        .into()
}

/// `POST /v1/providers/azure/oauth/{attempt_id}/finish` — links the chosen
/// subscription.
#[skyzen::openapi]
pub async fn azure_finish(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(microsoft): State<MicrosoftClient>,
    State(clouds): State<Clouds>,
    params: Params,
    Json(request): Json<FinishAzureOauth>,
    kv: Kv,
    db: Db,
    queue: Queue,
) -> Outcome<Created<Json<ProviderAccountView>>> {
    let attempt_id = match path_id(&params, "attempt_id") {
        Ok(id) => id,
        Err(error) => return Err(error).into(),
    };
    finish_azure(
        &config, &microsoft, &clouds, &kv, &db, &queue, user.id, attempt_id, request,
    )
    .await
    .map(|view| Created(Json(view)))
    .into()
}

#[expect(
    clippy::too_many_arguments,
    reason = "finishing a sign-in names both vendors, both stores, and the \
              attempt it is redeeming"
)]
async fn finish_azure(
    config: &ApiConfig,
    microsoft: &MicrosoftClient,
    clouds: &Clouds,
    kv: &Kv,
    db: &Db,
    queue: &Queue,
    user: UserId,
    attempt_id: ProviderOauthAttemptId,
    request: FinishAzureOauth,
) -> Result<ProviderAccountView, ApiError> {
    let attempt = authorized(kv, user, Provider::Azure, attempt_id).await?;
    let Secrets::Azure(tokens) = attempt.secrets else {
        return Err(ApiError::CorruptRecord(
            "an Azure attempt is holding Google's tokens",
        ));
    };

    let identity = microsoft
        .create_identity(&tokens, &request.subscription_id)
        .await?;
    tracing::info!(
        account = %attempt.account,
        "created an Azure service principal for a linked subscription"
    );

    let view = provider_accounts::link(
        db,
        config,
        clouds,
        kv,
        queue,
        user,
        LinkProvider {
            label: chosen_label(&attempt.choices, &request.subscription_id),
            credentials: ProviderCredentials::Azure {
                tenant_id: identity.tenant_id,
                client_id: identity.client_id,
                client_secret: identity.client_secret,
                subscription_id: request.subscription_id,
            },
        },
    )
    .await?;

    // Spent: the account exists, and the vendor tokens in the attempt have
    // done the one thing they were kept for.
    kv.delete(&attempt.key).await?;
    Ok(view)
}

// ── GCP ──

/// `POST /v1/providers/gcp/oauth/start` — begins a Google sign-in.
#[skyzen::openapi]
pub async fn gcp_start(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    kv: Kv,
) -> Outcome<Json<ProviderOauthStart>> {
    begin(&config, &kv, user.id, Provider::Gcp)
        .await
        .map(Json)
        .into()
}

/// `GET /v1/providers/gcp/oauth/callback` — records what Google said.
///
/// Public, for the reason [`azure_callback`] is.
#[skyzen::openapi]
pub async fn gcp_callback(
    Query(callback): Query<ProviderCallback>,
    State(config): State<ApiConfig>,
    State(google): State<GoogleClient>,
    kv: Kv,
) -> Outcome<SeeOther> {
    let outcome = record_gcp(&config, &google, &kv, &callback).await;
    Ok(returned(&config, Provider::Gcp, outcome)).into()
}

/// Redeems what Google sent back, or says why it cannot.
async fn sign_in_gcp(
    config: &ApiConfig,
    google: &GoogleClient,
    callback: &ProviderCallback,
) -> Result<google::SignIn, ApiError> {
    Ok(google
        .sign_in(
            google::OauthClient {
                id: config.google_oauth_client_id(),
                secret: config.google_oauth_client_secret(),
            },
            callback.code(Provider::Gcp)?,
            config.gcp_oauth_redirect_uri().as_str(),
        )
        .await?)
}

async fn record_gcp(
    config: &ApiConfig,
    google: &GoogleClient,
    kv: &Kv,
    callback: &ProviderCallback,
) -> Result<(), ApiError> {
    let (key, mut attempt) = by_state(kv, Provider::Gcp, &callback.state).await?;
    let signed_in = match sign_in_gcp(config, google, callback).await {
        Ok(signed_in) => signed_in,
        Err(error) => {
            record_failure(kv, &key, attempt, &error).await?;
            return Err(error);
        }
    };

    attempt.stage = Stage::Authorized {
        account: signed_in.account,
        choices: signed_in.choices,
        secrets: Secrets::Gcp(signed_in.access_token),
    };
    expiring::put(kv, &key, &attempt, ATTEMPT_TTL_SECONDS).await?;
    Ok(())
}

/// `GET /v1/providers/gcp/oauth/{attempt_id}` — polls it once.
#[skyzen::openapi]
pub async fn gcp_poll(
    State(user): State<CurrentUser>,
    params: Params,
    kv: Kv,
) -> Outcome<Json<ProviderOauthProgress>> {
    let attempt_id = match path_id(&params, "attempt_id") {
        Ok(id) => id,
        Err(error) => return Err(error).into(),
    };
    progress(&kv, user.id, Provider::Gcp, attempt_id)
        .await
        .map(Json)
        .into()
}

/// `POST /v1/providers/gcp/oauth/{attempt_id}/finish` — links the chosen
/// project.
#[skyzen::openapi]
pub async fn gcp_finish(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(google): State<GoogleClient>,
    State(clouds): State<Clouds>,
    params: Params,
    Json(request): Json<FinishGcpOauth>,
    kv: Kv,
    db: Db,
    queue: Queue,
) -> Outcome<Created<Json<ProviderAccountView>>> {
    let attempt_id = match path_id(&params, "attempt_id") {
        Ok(id) => id,
        Err(error) => return Err(error).into(),
    };
    finish_gcp(
        &config, &google, &clouds, &kv, &db, &queue, user.id, attempt_id, request,
    )
    .await
    .map(|view| Created(Json(view)))
    .into()
}

#[expect(
    clippy::too_many_arguments,
    reason = "finishing a sign-in names both vendors, both stores, and the \
              attempt it is redeeming"
)]
async fn finish_gcp(
    config: &ApiConfig,
    google: &GoogleClient,
    clouds: &Clouds,
    kv: &Kv,
    db: &Db,
    queue: &Queue,
    user: UserId,
    attempt_id: ProviderOauthAttemptId,
    request: FinishGcpOauth,
) -> Result<ProviderAccountView, ApiError> {
    let attempt = authorized(kv, user, Provider::Gcp, attempt_id).await?;
    let Secrets::Gcp(token) = attempt.secrets else {
        return Err(ApiError::CorruptRecord(
            "a Google attempt is holding Microsoft's tokens",
        ));
    };

    let service_account_json = google.create_identity(&token, &request.project_id).await?;
    tracing::info!(
        account = %attempt.account,
        "created a Google service account for a linked project"
    );

    let view = provider_accounts::link(
        db,
        config,
        clouds,
        kv,
        queue,
        user,
        LinkProvider {
            label: chosen_label(&attempt.choices, &request.project_id),
            credentials: ProviderCredentials::Gcp {
                service_account_json,
            },
        },
    )
    .await?;

    kv.delete(&attempt.key).await?;
    Ok(view)
}

// ── Codespaces ──

/// `POST /v1/providers/codespaces/oauth/start` — begins a GitHub sign-in
/// for the `codespace` scope.
#[skyzen::openapi]
pub async fn codespaces_start(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    kv: Kv,
) -> Outcome<Json<ProviderOauthStart>> {
    begin(&config, &kv, user.id, Provider::Codespaces)
        .await
        .map(Json)
        .into()
}

/// Redeems a Codespaces return out of the shared GitHub callback.
///
/// The OAuth app registers one callback URI per hostname, so a Codespaces
/// consent comes back on the sign-in's own path and
/// [`crate::oauth::callback`] asks this first: `Some` — the redirect the
/// return page needs — when the `state` named a codespaces attempt, `None`
/// when it did not, leaving the sign-in to claim it. A miss is
/// `UnknownOauthState` from [`by_state`] and nothing else: the pointer is
/// *taken* on a hit, so the state still redeems exactly once.
pub(crate) async fn redeem_codespaces(
    config: &ApiConfig,
    github: &GithubClient,
    kv: &Kv,
    callback: &ProviderCallback,
) -> Result<Option<SeeOther>, ApiError> {
    let (key, attempt) = match by_state(kv, Provider::Codespaces, &callback.state).await {
        Ok(found) => found,
        Err(ApiError::UnknownOauthState) => return Ok(None),
        Err(error) => return Err(error),
    };
    let outcome = record_codespaces(config, github, kv, callback, &key, attempt).await;
    Ok(Some(returned(config, Provider::Codespaces, outcome)))
}

/// Redeems what GitHub sent back: the token, the account it acts as, and
/// the proof it carries the scopes a codespace needs.
///
/// The scope check happens here rather than at the finish because here is
/// where the answer can still change something — a refusal fails the
/// attempt, and the page polling it learns the sign-in is over instead of
/// discovering it one click later.
async fn sign_in_codespaces(
    config: &ApiConfig,
    github: &GithubClient,
    callback: &ProviderCallback,
) -> Result<CodespacesSignIn, ApiError> {
    let grant = github
        .exchange_code(
            config.github_client_id(),
            config.github_client_secret(),
            callback.code(Provider::Codespaces)?,
            config.redirect_uri().as_str(),
        )
        .await?;
    let identity = github.current_user(&grant.token).await?;
    if !identity.grants_scope(CODESPACE_SCOPE) {
        return Err(Provider::Codespaces.rejected(format!(
            "the sign-in did not grant the `{CODESPACE_SCOPE}` scope — without it flyco \
             cannot create or drive a codespace"
        )));
    }
    if !identity.grants_repo_scope() {
        return Err(Provider::Codespaces.rejected(
            "the sign-in did not grant the `repo` scope — without it flyco cannot create \
             or write the private repository a codespace is built from"
                .to_owned(),
        ));
    }
    Ok(CodespacesSignIn {
        grant,
        login: identity.user.login,
        owner_id: identity.user.id,
        included_core_hours: flyco_provider::codespaces::included_core_hours(
            identity.user.plan.as_ref().map(|plan| plan.name.as_str()),
        ),
    })
}

/// What the callback keeps of a GitHub sign-in until the finish spends it.
struct CodespacesSignIn {
    /// The grant in full — the token is the credential, and the refresh
    /// half is what renews it once the grant nears its end.
    grant: crate::github::GithubGrant,
    /// Who it acts as.
    login: String,
    /// The account's immutable id.
    owner_id: i64,
    /// Core-hours the account's plan includes monthly.
    included_core_hours: u32,
}

/// Writes what a returned consent proved onto the attempt it belongs to.
async fn record_codespaces(
    config: &ApiConfig,
    github: &GithubClient,
    kv: &Kv,
    callback: &ProviderCallback,
    key: &str,
    mut attempt: Attempt,
) -> Result<(), ApiError> {
    let signed_in = match sign_in_codespaces(config, github, callback).await {
        Ok(signed_in) => signed_in,
        Err(error) => {
            record_failure(kv, key, attempt, &error).await?;
            return Err(error);
        }
    };

    // No choices — there is nothing a GitHub account has many of that a
    // codespace provisions into; flyco creates the one repository it needs.
    attempt.stage = Stage::Authorized {
        account: signed_in.login,
        choices: Vec::new(),
        secrets: Secrets::Codespaces {
            token: signed_in.grant.token.access_token,
            refresh_token: signed_in.grant.refresh_token,
            token_expires_at_unix: signed_in.grant.expires_at_unix,
            owner_id: signed_in.owner_id,
            included_core_hours: signed_in.included_core_hours,
        },
    };
    expiring::put(kv, key, &attempt, ATTEMPT_TTL_SECONDS).await?;
    Ok(())
}

/// `GET /v1/providers/codespaces/oauth/{attempt_id}` — polls it once.
#[skyzen::openapi]
pub async fn codespaces_poll(
    State(user): State<CurrentUser>,
    params: Params,
    kv: Kv,
) -> Outcome<Json<ProviderOauthProgress>> {
    let attempt_id = match path_id(&params, "attempt_id") {
        Ok(id) => id,
        Err(error) => return Err(error).into(),
    };
    progress(&kv, user.id, Provider::Codespaces, attempt_id)
        .await
        .map(Json)
        .into()
}

/// `POST /v1/providers/codespaces/oauth/{attempt_id}/finish` — creates the
/// environment repository and links the account.
#[skyzen::openapi]
pub async fn codespaces_finish(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(codespaces): State<Codespaces>,
    State(clouds): State<Clouds>,
    params: Params,
    Json(_request): Json<FinishCodespacesOauth>,
    kv: Kv,
    db: Db,
    queue: Queue,
) -> Outcome<Created<Json<ProviderAccountView>>> {
    let attempt_id = match path_id(&params, "attempt_id") {
        Ok(id) => id,
        Err(error) => return Err(error).into(),
    };
    finish_codespaces(
        &config,
        &codespaces,
        &clouds,
        &kv,
        &db,
        &queue,
        user.id,
        attempt_id,
    )
    .await
    .map(|view| Created(Json(view)))
    .into()
}

async fn finish_codespaces(
    config: &ApiConfig,
    codespaces: &Codespaces,
    clouds: &Clouds,
    kv: &Kv,
    db: &Db,
    queue: &Queue,
    user: UserId,
    attempt_id: ProviderOauthAttemptId,
) -> Result<ProviderAccountView, ApiError> {
    let attempt = authorized(kv, user, Provider::Codespaces, attempt_id).await?;
    let Secrets::Codespaces {
        token,
        refresh_token,
        token_expires_at_unix,
        owner_id,
        included_core_hours,
    } = attempt.secrets
    else {
        return Err(ApiError::CorruptRecord(
            "a Codespaces attempt is holding another vendor's tokens",
        ));
    };

    let view = link_codespaces_account(
        config,
        codespaces,
        clouds,
        kv,
        db,
        queue,
        user,
        &attempt.account,
        GithubGrant {
            token: GithubToken {
                access_token: token,
            },
            refresh_token,
            expires_at_unix: token_expires_at_unix,
        },
        owner_id,
        included_core_hours,
    )
    .await?;

    kv.delete(&attempt.key).await?;
    Ok(view)
}

/// `POST /v1/providers/codespaces/link` — links the account straight from
/// the sign-in grant.
///
/// Sign-in has asked for the Codespaces scope set since grants began
/// carrying it, so the grant a user signed in with usually *is* the
/// credential the link stores — this route proves it with the scopes
/// `GET /user` reports, then does the finish's own work without the
/// OAuth attempt. A grant that predates the ask answers
/// [`ApiError::GithubScopeMissing`], which is what sends the page through
/// the OAuth flow instead.
#[skyzen::openapi]
pub async fn codespaces_link(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(github): State<GithubClient>,
    State(codespaces): State<Codespaces>,
    State(clouds): State<Clouds>,
    kv: Kv,
    db: Db,
    queue: Queue,
) -> Outcome<Created<Json<ProviderAccountView>>> {
    link_codespaces(
        &config,
        &github,
        &codespaces,
        &clouds,
        &kv,
        &db,
        &queue,
        user.id,
    )
    .await
    .map(|view| Created(Json(view)))
    .into()
}

async fn link_codespaces(
    config: &ApiConfig,
    github: &GithubClient,
    codespaces: &Codespaces,
    clouds: &Clouds,
    kv: &Kv,
    db: &Db,
    queue: &Queue,
    user: UserId,
) -> Result<ProviderAccountView, ApiError> {
    let grant = users::github_grant(db, config, github, user).await?;
    let identity = github.current_user(&grant.token).await?;
    for scope in [REPO_SCOPE, CODESPACE_SCOPE] {
        if !identity.grants_scope(scope) {
            return Err(ApiError::GithubScopeMissing { scope });
        }
    }
    link_codespaces_account(
        config,
        codespaces,
        clouds,
        kv,
        db,
        queue,
        user,
        &identity.user.login,
        grant,
        identity.user.id,
        flyco_provider::codespaces::included_core_hours(
            identity.user.plan.as_ref().map(|plan| plan.name.as_str()),
        ),
    )
    .await
}

/// The work both doors into a Codespaces link share once each has proven
/// the grant carries what provisioning needs: the environment repository is
/// created first — `link`'s own verify re-reads it, so building it here is
/// what makes the recorded credential one that already provisions.
async fn link_codespaces_account(
    config: &ApiConfig,
    codespaces: &Codespaces,
    clouds: &Clouds,
    kv: &Kv,
    db: &Db,
    queue: &Queue,
    user: UserId,
    login: &str,
    grant: GithubGrant,
    owner_id: i64,
    included_core_hours: u32,
) -> Result<ProviderAccountView, ApiError> {
    let environment = codespaces
        .ensure_environment(
            &grant.token,
            login,
            &flyco_provider::codespaces::devcontainer_json(&config.control_plane_url()),
        )
        .await
        .map_err(crate::clouds::rejected)?;
    tracing::info!(
        account = %login,
        repository = %environment.full_name,
        "prepared a Codespaces environment repository"
    );

    provider_accounts::link(
        db,
        config,
        clouds,
        kv,
        queue,
        user,
        LinkProvider {
            // The GitHub login is the name the user recognises: it is the
            // one account this credential can ever act as.
            label: login.to_owned(),
            credentials: ProviderCredentials::Codespaces {
                token: grant.token.access_token,
                refresh_token: grant.refresh_token,
                token_expires_at_unix: grant.expires_at_unix,
                env_repo: environment.full_name,
                env_repo_id: environment.id,
                owner_id,
                included_core_hours,
            },
        },
    )
    .await
}

/// The two public callbacks.
///
/// Public because they cannot be anything else: a browser returning from a
/// cloud vendor carries no flyco credential. Each authenticates itself with
/// the single-use `state` it was minted. GitHub's Codespaces return is the
/// exception: it shares the sign-in callback in [`crate::oauth`], which
/// routes it back here by `state`.
pub fn public_routes() -> Vec<RouteNode> {
    Route::new((
        AZURE_CALLBACK_PATH.at(azure_callback),
        GCP_CALLBACK_PATH.at(gcp_callback),
    ))
    .into_route_nodes()
}

/// The ten authenticated routes of the three cloud sign-ins.
pub fn routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/providers/azure/oauth/start".post(azure_start),
        "/v1/providers/azure/oauth/{attempt_id}".at(azure_poll),
        "/v1/providers/azure/oauth/{attempt_id}/finish".post(azure_finish),
        "/v1/providers/gcp/oauth/start".post(gcp_start),
        "/v1/providers/gcp/oauth/{attempt_id}".at(gcp_poll),
        "/v1/providers/gcp/oauth/{attempt_id}/finish".post(gcp_finish),
        "/v1/providers/codespaces/link".post(codespaces_link),
        "/v1/providers/codespaces/oauth/start".post(codespaces_start),
        "/v1/providers/codespaces/oauth/{attempt_id}".at(codespaces_poll),
        "/v1/providers/codespaces/oauth/{attempt_id}/finish".post(codespaces_finish),
    ))
    .into_route_nodes()
}
