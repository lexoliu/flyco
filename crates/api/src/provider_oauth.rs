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
    CurrentUser, FinishAzureOauth, FinishGcpOauth, LinkProvider, ProviderAccountView,
    ProviderCredentials, ProviderOauthAttemptId, ProviderOauthChoice, ProviderOauthProgress,
    ProviderOauthStart, UserId,
};
use serde::{Deserialize, Serialize};
use skyzen::extract::Query;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::utils::{Json, State};
use skyzen_services::{Db, Kv};
use url::Url;

use crate::clouds::Clouds;
use crate::config::ApiConfig;
use crate::crypto::random_token;
use crate::error::ApiError;
use crate::expiring;
use crate::extract::path_id;
use crate::google::{self, GoogleClient, GoogleOauth as _};
use crate::microsoft::{self, AzureTokens, MicrosoftClient, MicrosoftOauth as _};
use crate::problem::Outcome;
use crate::provider_accounts;
use crate::respond::{Created, SeeOther};

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
}

impl Provider {
    /// How the return page and the route paths name it.
    const fn slug(self) -> &'static str {
        match self {
            Self::Azure => "azure",
            Self::Gcp => "gcp",
        }
    }

    /// This vendor's refusal, as the problem a caller sees.
    const fn rejected(self, reason: String) -> ApiError {
        match self {
            Self::Azure => ApiError::MicrosoftRejected { reason },
            Self::Gcp => ApiError::GoogleRejected { reason },
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
}

impl core::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Azure(_) => f.write_str("Secrets::Azure(..)"),
            Self::Gcp(_) => f.write_str("Secrets::Gcp(..)"),
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
    state: String,
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
}

/// Where the browser is sent once the vendor is done with it.
///
/// The SPA's return route on the callback's own origin, naming the provider
/// and — when something went wrong — the problem to explain.
fn return_url(config: &ApiConfig, provider: Provider, problem: Option<&str>) -> Url {
    let mut url = config
        .redirect_uri()
        .join(RETURN_PATH)
        .expect("a rooted path always resolves against an absolute redirect URI");
    {
        let mut query = url.query_pairs_mut();
        query.append_pair(PROVIDER_PARAM, provider.slug());
        if let Some(problem) = problem {
            query.append_pair(PROBLEM_PARAM, problem);
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
            SeeOther(return_url(config, provider, Some(error.slug())))
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
    })
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

async fn record_azure(
    config: &ApiConfig,
    microsoft: &MicrosoftClient,
    kv: &Kv,
    callback: &ProviderCallback,
) -> Result<(), ApiError> {
    let (key, mut attempt) = by_state(kv, Provider::Azure, &callback.state).await?;
    let signed_in = microsoft
        .sign_in(
            microsoft::OauthClient {
                id: config.azure_oauth_client_id(),
                secret: config.azure_oauth_client_secret(),
            },
            callback.code(Provider::Azure)?,
            config.azure_oauth_redirect_uri().as_str(),
        )
        .await?;

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
) -> Outcome<Created<Json<ProviderAccountView>>> {
    let attempt_id = match path_id(&params, "attempt_id") {
        Ok(id) => id,
        Err(error) => return Err(error).into(),
    };
    finish_azure(
        &config, &microsoft, &clouds, &kv, &db, user.id, attempt_id, request,
    )
    .await
    .map(|view| Created(Json(view)))
    .into()
}

async fn finish_azure(
    config: &ApiConfig,
    microsoft: &MicrosoftClient,
    clouds: &Clouds,
    kv: &Kv,
    db: &Db,
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
        user,
        LinkProvider {
            label: chosen_label(&attempt.choices, &request.subscription_id),
            credentials: ProviderCredentials::Azure {
                tenant_id: identity.tenant_id,
                client_id: identity.client_id,
                client_secret: identity.client_secret,
                subscription_id: request.subscription_id,
                admin_ssh_public_key: request.admin_ssh_public_key,
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

async fn record_gcp(
    config: &ApiConfig,
    google: &GoogleClient,
    kv: &Kv,
    callback: &ProviderCallback,
) -> Result<(), ApiError> {
    let (key, mut attempt) = by_state(kv, Provider::Gcp, &callback.state).await?;
    let signed_in = google
        .sign_in(
            google::OauthClient {
                id: config.google_oauth_client_id(),
                secret: config.google_oauth_client_secret(),
            },
            callback.code(Provider::Gcp)?,
            config.gcp_oauth_redirect_uri().as_str(),
        )
        .await?;

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
) -> Outcome<Created<Json<ProviderAccountView>>> {
    let attempt_id = match path_id(&params, "attempt_id") {
        Ok(id) => id,
        Err(error) => return Err(error).into(),
    };
    finish_gcp(
        &config, &google, &clouds, &kv, &db, user.id, attempt_id, request,
    )
    .await
    .map(|view| Created(Json(view)))
    .into()
}

async fn finish_gcp(
    config: &ApiConfig,
    google: &GoogleClient,
    clouds: &Clouds,
    kv: &Kv,
    db: &Db,
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

/// The two public callbacks.
///
/// Public because they cannot be anything else: a browser returning from a
/// cloud vendor carries no flyco credential. Each authenticates itself with
/// the single-use `state` it was minted.
pub fn public_routes() -> Vec<RouteNode> {
    Route::new((
        AZURE_CALLBACK_PATH.at(azure_callback),
        GCP_CALLBACK_PATH.at(gcp_callback),
    ))
    .into_route_nodes()
}

/// The six authenticated routes of the two cloud sign-ins.
pub fn routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/providers/azure/oauth/start".post(azure_start),
        "/v1/providers/azure/oauth/{attempt_id}".at(azure_poll),
        "/v1/providers/azure/oauth/{attempt_id}/finish".post(azure_finish),
        "/v1/providers/gcp/oauth/start".post(gcp_start),
        "/v1/providers/gcp/oauth/{attempt_id}".at(gcp_poll),
        "/v1/providers/gcp/oauth/{attempt_id}/finish".post(gcp_finish),
    ))
    .into_route_nodes()
}
