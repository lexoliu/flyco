//! Linking Codex by signing in with a `ChatGPT` subscription.
//!
//! Two routes, and between them the user does what `codex login
//! --device-auth` asks them to do: read a short code off the screen, open
//! `auth.openai.com/codex/device`, and approve it. [`start`] creates the
//! device authorization and keeps it in KV for fifteen minutes under an
//! opaque attempt id; [`poll`] asks `OpenAI` once per call whether it has
//! been approved, and links the account when it has.
//!
//! The `device_auth_id` never reaches the browser. It is the half that
//! redeems the grant — with the user code, it is enough to obtain tokens —
//! so it stays in the control plane, bound to the user who started the
//! attempt, exactly as the PKCE verifier does in [`crate::claude_oauth`].
//!
//! Polling is the browser's job rather than the Worker's: a Worker request
//! that waited fifteen minutes for an approval would be a Worker request
//! that timed out. One call, one poll, and the card asks again after the
//! interval `OpenAI` stated.

use flyco_core::{
    CodexOauthAttemptId, CodexOauthPending, CodexOauthStart, CurrentUser, HarnessAccountView,
    HarnessKind, UserId,
};
use serde::{Deserialize, Serialize};
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::utils::{Json, State};
use skyzen::{Request, Responder, Response, StatusCode};
use skyzen_services::{Db, Kv};

use crate::clock::now_unix;
use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::expiring;
use crate::extract::path_id;
use crate::harness_accounts::{self, StoredCredential};
use crate::openai::{self, CodexClient, CodexOauth as _, DevicePoll, TokenRequest};
use crate::problem::Outcome;
use crate::respond::Created;

/// How long `OpenAI` keeps a device authorization alive, and therefore how
/// long the attempt beside it is worth keeping.
///
/// Fifteen minutes is `OpenAI`'s own limit, printed to the user as "expires
/// in 15 minutes" by `codex login --device-auth`.
pub const ATTEMPT_TTL_SECONDS: u64 = 15 * 60;

/// What a linked account is called when the id token names no address.
const UNNAMED_ACCOUNT: &str = "ChatGPT subscription";

/// Prefix every attempt is stored under.
const ATTEMPT_KEY_PREFIX: &str = "auth:codex-oauth:";

/// One device sign-in in flight, as KV holds it.
///
/// A secret by construction: `device_auth_id` and `user_code` together
/// redeem the grant, so this value is never returned to anybody.
#[derive(Serialize, Deserialize)]
struct Attempt {
    /// Who started it. An approval polled by anybody else is not this
    /// attempt.
    user: UserId,
    /// `OpenAI`'s name for the device authorization.
    device_auth_id: String,
    /// The code the user was shown.
    user_code: String,
    /// When the attempt began, seconds since the Unix epoch.
    started_at_unix: u64,
}

impl core::fmt::Debug for Attempt {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Attempt")
            .field("user", &self.user)
            .field("started_at_unix", &self.started_at_unix)
            .finish_non_exhaustive()
    }
}

/// Where one attempt lives in the key-value store.
fn attempt_key(id: CodexOauthAttemptId) -> String {
    let rendered = id.to_string();
    let mut key = String::with_capacity(ATTEMPT_KEY_PREFIX.len() + rendered.len());
    key.push_str(ATTEMPT_KEY_PREFIX);
    key.push_str(&rendered);
    key
}

/// How far one poll of a Codex sign-in got.
///
/// Two statuses rather than one nullable body: a poll that found nothing is
/// `200` and says so, and the poll that links the account is the `201` that
/// created it.
#[derive(Debug)]
pub enum CodexOauthProgress {
    /// Nobody has approved the code yet.
    Pending,
    /// The account is linked, and this is it.
    Linked(HarnessAccountView),
}

impl Responder for CodexOauthProgress {
    type Error = <Json<HarnessAccountView> as Responder>::Error;

    fn respond_to(self, request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        match self {
            Self::Pending => Json(CodexOauthPending::Pending).respond_to(request, response),
            Self::Linked(view) => Created(Json(view)).respond_to(request, response),
        }
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<Vec<skyzen::openapi::ResponseSchema>> {
        let mut schemas = crate::respond::described(
            <Json<CodexOauthPending> as Responder>::openapi()?,
            StatusCode::OK,
            "Nobody has approved the code yet; ask again after `interval_seconds`.",
        );
        schemas.extend(<Created<Json<HarnessAccountView>> as Responder>::openapi()?);
        Some(schemas)
    }

    #[cfg(feature = "openapi")]
    fn register_openapi_schemas(
        defs: &mut std::collections::BTreeMap<String, skyzen::openapi::SchemaRef>,
    ) {
        <Json<CodexOauthPending> as Responder>::register_openapi_schemas(defs);
        <Json<HarnessAccountView> as Responder>::register_openapi_schemas(defs);
    }
}

/// `POST /v1/harness-accounts/codex/oauth/start` — begins a Codex sign-in.
#[skyzen::openapi]
pub async fn start(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(codex): State<CodexClient>,
    kv: Kv,
) -> Outcome<Json<CodexOauthStart>> {
    begin(&config, &codex, &kv, user.id).await.map(Json).into()
}

async fn begin(
    config: &ApiConfig,
    codex: &CodexClient,
    kv: &Kv,
    user: UserId,
) -> Result<CodexOauthStart, ApiError> {
    let device = codex
        .request_user_code(config.codex_oauth_client_id())
        .await?;
    let attempt_id = CodexOauthAttemptId::generate();

    expiring::put(
        kv,
        &attempt_key(attempt_id),
        &Attempt {
            user,
            device_auth_id: device.device_auth_id.clone(),
            user_code: device.user_code.clone(),
            started_at_unix: now_unix(),
        },
        ATTEMPT_TTL_SECONDS,
    )
    .await?;

    tracing::debug!("created a Codex device authorization");
    Ok(CodexOauthStart {
        attempt_id,
        interval_seconds: device.interval_seconds(),
        user_code: device.user_code,
        verification_url: openai::VERIFICATION_URL.to_owned(),
    })
}

/// `GET /v1/harness-accounts/codex/oauth/{attempt_id}` — polls it once.
#[skyzen::openapi]
pub async fn poll(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(codex): State<CodexClient>,
    params: Params,
    kv: Kv,
    db: Db,
) -> Outcome<CodexOauthProgress> {
    let attempt_id = match path_id(&params, "attempt_id") {
        Ok(id) => id,
        Err(error) => return Err(error).into(),
    };
    ask(&config, &codex, &kv, &db, user.id, attempt_id)
        .await
        .into()
}

async fn ask(
    config: &ApiConfig,
    codex: &CodexClient,
    kv: &Kv,
    db: &Db,
    user: UserId,
    attempt_id: CodexOauthAttemptId,
) -> Result<CodexOauthProgress, ApiError> {
    let key = attempt_key(attempt_id);
    // Read rather than take: a poll that consumed the attempt would end the
    // sign-in every time the user had not approved it yet.
    let attempt = expiring::get::<Attempt>(kv, &key)
        .await?
        .filter(|attempt| attempt.user == user)
        .ok_or(ApiError::CodexOauthAttemptExpired)?;

    if now_unix().saturating_sub(attempt.started_at_unix) >= ATTEMPT_TTL_SECONDS {
        kv.delete(&key).await?;
        return Err(ApiError::CodexOauthAttemptExpired);
    }

    let poll = codex
        .poll_device_code(&attempt.device_auth_id, &attempt.user_code)
        .await?;
    let DevicePoll::Approved(code) = poll else {
        return Ok(CodexOauthProgress::Pending);
    };

    // Approved once is approved for good: the code redeems exactly once, so
    // the attempt is spent here whether or not the exchange then works.
    kv.delete(&key).await?;

    let grant = codex
        .exchange(TokenRequest::AuthorizationCode {
            code: &code.authorization_code,
            redirect_uri: openai::REDIRECT_URI,
            client_id: config.codex_oauth_client_id(),
            code_verifier: &code.code_verifier,
        })
        .await?
        .issued()?;

    let credential = StoredCredential::from_grant(&grant)?;
    let label = grant
        .email_address()
        .unwrap_or_else(|| UNNAMED_ACCOUNT.to_owned());
    let view =
        harness_accounts::store(db, config, user, &label, HarnessKind::Codex, &credential).await?;
    Ok(CodexOauthProgress::Linked(view))
}

/// The two authenticated routes of the Codex sign-in.
pub fn routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/harness-accounts/codex/oauth/start".post(start),
        "/v1/harness-accounts/codex/oauth/{attempt_id}".at(poll),
    ))
    .into_route_nodes()
}
