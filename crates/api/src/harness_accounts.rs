//! Claude and Codex accounts, and the LLM usage panel.
//!
//! Linking runs against the **vendor's own** authorization page — the flow
//! the official CLIs wrap — so flyco never handles a vendor password and
//! stores only the resulting token, sealed. That makes the callback public:
//! the browser arrives back from Anthropic or `OpenAI` carrying no flyco
//! credential, so the `state` minted at the start is what identifies the
//! user, exactly as in [`crate::oauth`].
//!
//! Flyco holds at most one account per harness, which is why the harness
//! itself names the account in a path rather than an identifier.

use flyco_core::{
    AuthorizeUrl, CurrentUser, HarnessAccountId, HarnessAccountView, HarnessKind, LlmUsageView,
    UserId,
};
use flyco_provider::ClaudeCredential;
use serde::Deserialize;
use skyzen::extract::Query;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::sql;
use skyzen::utils::{Json, State};
use skyzen_services::sql::ColumnEnum as _;
use skyzen_services::{Db, Kv};
use url::Url;

use crate::clock::now_unix;
use crate::config::{ApiConfig, HarnessOauthClient};
use crate::crypto::random_token;
use crate::error::ApiError;
use crate::expiring;
use crate::extract::path_id;
use crate::observations;
use crate::problem::Outcome;
use crate::respond::{NoContent, SeeOther};

/// The columns every read on this path projects.
///
/// `token_enc` is deliberately absent: the sealed credential is provisioned
/// onto machines and is never selected by a route that answers a browser.
#[derive(Debug, skyzen::FromRow)]
struct HarnessAccountRow {
    id: HarnessAccountId,
    harness: HarnessKind,
    label: String,
    linked_at_unix: u64,
    expires_at_unix: Option<u64>,
}

impl From<HarnessAccountRow> for HarnessAccountView {
    fn from(row: HarnessAccountRow) -> Self {
        Self {
            id: row.id,
            harness: row.harness,
            label: row.label,
            linked_at_unix: row.linked_at_unix,
            expires_at_unix: row.expires_at_unix,
        }
    }
}

/// Query string the vendor appends when it redirects back.
#[derive(Debug, Deserialize, skyzen::ToSchema)]
pub struct LinkCallback {
    /// The single-use authorization code.
    pub code: String,
    /// The `state` this control plane minted when the link began.
    pub state: String,
}

/// Lists the caller's linked harness accounts.
#[skyzen::openapi]
async fn list_harness_accounts(
    State(user): State<CurrentUser>,
    db: Db,
) -> Outcome<Json<Vec<HarnessAccountView>>> {
    list(&db, user.id).await.map(Json).into()
}

async fn list(db: &Db, user: UserId) -> Result<Vec<HarnessAccountView>, ApiError> {
    let rows: Vec<HarnessAccountRow> = sql!(
        db,
        "SELECT id, harness, label, linked_at_unix, expires_at_unix \
         FROM harness_accounts WHERE user_id = {user} ORDER BY harness"
    )
    .fetch_all()
    .await?;

    Ok(rows.into_iter().map(Into::into).collect())
}

/// Begins linking a harness account, returning the vendor's authorize URL.
#[skyzen::openapi]
async fn start_harness_link(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    params: Params,
    kv: Kv,
) -> Outcome<Json<AuthorizeUrl>> {
    begin_link(&config, &kv, &user, &params)
        .await
        .map(Json)
        .into()
}

/// How long a browser has to finish the round trip.
const LINK_STATE_TTL_SECONDS: u64 = 10 * 60;

/// What the `state` stands for while the browser is away.
///
/// The callback arrives with no flyco credential, so everything the exchange
/// needs is parked here: whose account this is, which vendor, and the PKCE
/// verifier whose challenge went out with the authorize URL.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct PendingLink {
    user: UserId,
    harness: HarnessKind,
    verifier: String,
}

fn link_state_key(state: &str) -> String {
    let mut key = String::with_capacity(18 + state.len());
    key.push_str("auth:harness-link:");
    key.push_str(state);
    key
}

/// The PKCE challenge for a verifier: base64url of its SHA-256, unpadded.
fn pkce_challenge(verifier: &str) -> String {
    use base64::Engine as _;
    use sha2::{Digest as _, Sha256};
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Where the vendor sends the browser back to, for one harness.
fn link_redirect_uri(config: &ApiConfig, harness: HarnessKind) -> Result<Url, ApiError> {
    let mut path = String::from("/v1/harness-accounts/");
    path.push_str(harness.token());
    path.push_str("/link/callback");
    config
        .redirect_uri()
        .join(&path)
        .map_err(|_| ApiError::CorruptRecord("the configured redirect URI has no base"))
}

/// Reads the `{harness}` path segment as the harness it names.
fn harness_from_path(params: &Params) -> Result<HarnessKind, ApiError> {
    let segment = crate::extract::path_segment(params, "harness")?;
    HarnessKind::from_token(&segment).ok_or(ApiError::MalformedId(segment))
}

/// The client for this harness, or a refusal naming what is missing.
fn oauth_client(config: &ApiConfig, harness: HarnessKind) -> Result<&HarnessOauthClient, ApiError> {
    config
        .harness_oauth(harness)
        .ok_or(ApiError::HarnessLinkUnconfigured {
            harness: harness.token(),
        })
}

async fn begin_link(
    config: &ApiConfig,
    kv: &Kv,
    user: &CurrentUser,
    params: &Params,
) -> Result<AuthorizeUrl, ApiError> {
    let harness = harness_from_path(params)?;
    let client = oauth_client(config, harness)?;

    let state = random_token()?;
    let verifier = random_token()?;
    expiring::put(
        kv,
        &link_state_key(&state),
        &PendingLink {
            user: user.id,
            harness,
            verifier: verifier.clone(),
        },
        LINK_STATE_TTL_SECONDS,
    )
    .await?;

    let mut authorize_url = client.authorize_url.clone();
    authorize_url
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &client.client_id)
        .append_pair("redirect_uri", link_redirect_uri(config, harness)?.as_str())
        .append_pair("scope", &client.scope)
        .append_pair("state", &state)
        .append_pair("code_challenge", &pkce_challenge(&verifier))
        .append_pair("code_challenge_method", "S256");

    tracing::debug!(?harness, "issued a harness authorize URL");
    Ok(AuthorizeUrl {
        authorize_url: authorize_url.into(),
    })
}

/// Completes a harness link and returns the browser to the SPA.
///
/// Public, because the browser arrives from the vendor with no flyco
/// credential; the single-use `state` minted by the start call is what says
/// whose account this is.
#[skyzen::openapi]
async fn complete_harness_link(
    params: Params,
    Query(callback): Query<LinkCallback>,
    State(config): State<ApiConfig>,
    kv: Kv,
    db: Db,
) -> Outcome<SeeOther> {
    complete_link(&config, &kv, &db, &params, callback)
        .await
        .into()
}

/// Where the browser lands once an account is linked.
const POST_LINK_PATH: &str = "/settings/harness-accounts";

/// What a vendor returns for an authorization code, per RFC 6749 §5.1.
///
/// Only the fields flyco stores are named; a vendor sends more and serde
/// discards them.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    /// Seconds until the credential expires, when the vendor states one.
    expires_in: Option<u64>,
    /// Some vendors name the account the token belongs to. Used as the
    /// label, so two linked accounts can be told apart.
    account: Option<String>,
}

/// Exchanges an authorization code for a credential.
///
/// PKCE (RFC 7636) carries the verifier whose challenge went out with the
/// authorize URL, so an intercepted code is useless on its own.
async fn exchange_code(
    client: &HarnessOauthClient,
    code: &str,
    verifier: &str,
    redirect_uri: &Url,
) -> Result<TokenResponse, ApiError> {
    use zenwave::{Client as _, ResponseExt as _};

    /// The exchange request, form-encoded as RFC 6749 §4.1.3 requires.
    #[derive(serde::Serialize)]
    struct ExchangeRequest<'a> {
        grant_type: &'a str,
        code: &'a str,
        redirect_uri: &'a str,
        client_id: &'a str,
        client_secret: &'a str,
        code_verifier: &'a str,
    }

    let form = serde_urlencoded::to_string(ExchangeRequest {
        grant_type: "authorization_code",
        code,
        redirect_uri: redirect_uri.as_str(),
        client_id: &client.client_id,
        client_secret: &client.client_secret,
        code_verifier: verifier,
    })
    .map_err(|error| ApiError::HarnessLinkRejected(error.to_string()))?;

    let mut http = zenwave::client();
    let response = http
        .post(client.token_url.as_str())
        .map_err(|error| ApiError::HarnessLinkRejected(error.to_string()))?
        .header("Accept", "application/json")
        .map_err(|error| ApiError::HarnessLinkRejected(error.to_string()))?
        .header("Content-Type", "application/x-www-form-urlencoded")
        .map_err(|error| ApiError::HarnessLinkRejected(error.to_string()))?
        .bytes_body(form.into_bytes())
        .await
        .map_err(|error| ApiError::HarnessLinkRejected(error.to_string()))?;

    let status = response.status();
    if !status.is_success() {
        return Err(ApiError::HarnessLinkRejected(format!(
            "the token endpoint answered {}",
            status.as_u16()
        )));
    }

    response
        .into_json::<TokenResponse>()
        .await
        .map_err(|error| ApiError::HarnessLinkRejected(error.to_string()))
}

async fn complete_link(
    config: &ApiConfig,
    kv: &Kv,
    db: &Db,
    params: &Params,
    callback: LinkCallback,
) -> Result<SeeOther, ApiError> {
    let harness = harness_from_path(params)?;

    // The state is consumed by being presented, so a replayed callback finds
    // nothing — and it carries whose link this is, since the browser arrives
    // from the vendor with no flyco credential of its own.
    let pending: PendingLink = expiring::take(kv, &link_state_key(&callback.state))
        .await?
        .ok_or(ApiError::UnknownOauthState)?;
    if pending.harness != harness {
        return Err(ApiError::UnknownOauthState);
    }

    let client = oauth_client(config, harness)?;
    let redirect_uri = link_redirect_uri(config, harness)?;
    let token = exchange_code(client, &callback.code, &pending.verifier, &redirect_uri).await?;

    let sealed = config.token_cipher().seal(&token.access_token)?;
    let now = now_unix();
    let expires_at = token.expires_in.map(|lifetime| now + lifetime);
    let label = token.account.unwrap_or_else(|| harness.token().to_owned());

    sql!(
        db,
        "INSERT INTO harness_accounts \
         (id, user_id, harness, label, token_enc, linked_at_unix, expires_at_unix) \
         VALUES ({HarnessAccountId::generate()}, {pending.user}, {harness}, {label}, \
                 {sealed}, {now}, {expires_at})"
    )
    .execute()
    .await?;

    tracing::info!(?harness, "linked a harness account");
    Ok(SeeOther(
        config
            .redirect_uri()
            .join(POST_LINK_PATH)
            .map_err(|_| ApiError::CorruptRecord("the configured redirect URI has no base"))?,
    ))
}

/// The credential a session's machine authenticates its harness with.
///
/// `None` linked account is not a failure: the machine comes up with
/// [`ClaudeCredential::Inherit`] and the harness reports itself
/// unauthenticated, which is a far better outcome than a machine that never
/// provisions because the user has not linked a vendor account yet.
///
/// `token_enc` seals the OAuth token the vendor's own authorization page
/// returned, which is the only kind of credential
/// [`crate::harness_accounts`] ever stores. The same three modes — inherit,
/// oauth token, api key — are written into the Claude or Codex table of
/// the `flycod` config depending on [`HarnessKind`].
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails or the sealed token was
/// written under a different key.
pub async fn credential(
    db: &Db,
    cipher: &crate::crypto::TokenCipher,
    user: UserId,
    harness: HarnessKind,
) -> Result<ClaudeCredential, ApiError> {
    let sealed: Option<String> = sql!(
        db,
        "SELECT token_enc FROM harness_accounts \
         WHERE user_id = {user} AND harness = {harness} ORDER BY linked_at_unix LIMIT 1"
    )
    .fetch_scalar_optional()
    .await?;

    sealed.map_or(Ok(ClaudeCredential::Inherit), |sealed| {
        Ok(ClaudeCredential::OauthToken {
            token: cipher.open(&sealed)?,
        })
    })
}

/// Unlinks the caller's account for one harness.
#[skyzen::openapi]
async fn unlink_harness_account(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Outcome<NoContent> {
    unlink(&db, user.id, &params).await.into()
}

/// Unlinking stops the credential reaching machines built from here on.
///
/// It does not reach into machines already running: their environment was
/// fixed when the process started, and a session mid-turn keeps the
/// credential it was given until it is archived.
///
/// Keyed by account id like every other resource flyco owns. The harness
/// kind was the key once, which quietly made "one account per harness" a
/// property of the *API* rather than of the table — the list response has
/// always carried a per-account id, and there was no way to name the second
/// account with it.
async fn unlink(db: &Db, user: UserId, params: &Params) -> Result<NoContent, ApiError> {
    let id: HarnessAccountId = path_id(params, "id")?;

    let removed = sql!(
        db,
        "DELETE FROM harness_accounts WHERE id = {id} AND user_id = {user}"
    )
    .execute()
    .await?;

    if removed.rows_written == 0 {
        return Err(ApiError::HarnessAccountNotFound);
    }

    tracing::info!(account = %id, "unlinked a harness account");
    Ok(NoContent)
}

/// Reports what flyco has observed of each harness account's usage.
///
/// Reactive by necessity: neither Anthropic nor `OpenAI` publishes a
/// remaining-quota API, so this reports the cost telemetry the harness
/// emitted and the rate limits it actually hit. A panel built on it says
/// what has happened, never what is left.
///
/// The rows come out of [`crate::observations`], which is filled in by the
/// sessions' own daemons as they run — the only place either number exists.
/// An account with nothing observed about it still appears, reporting
/// nothing, because "nothing has happened" is an answer and a missing row
/// would read as an account that is not linked.
#[skyzen::openapi]
async fn llm_usage(State(user): State<CurrentUser>, db: Db) -> Outcome<Json<Vec<LlmUsageView>>> {
    observations::usage(&db, user.id).await.map(Json).into()
}

/// The user-scoped harness-account routes.
pub fn routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/harness-accounts".at(list_harness_accounts),
        "/v1/harness-accounts/{id}".delete(unlink_harness_account),
        "/v1/harness-accounts/{harness}/link/start".post(start_harness_link),
        "/v1/usage/llm".at(llm_usage),
    ))
    .into_route_nodes()
}

/// The link callback, which carries no flyco credential.
pub fn public_routes() -> Vec<RouteNode> {
    Route::new(("/v1/harness-accounts/{harness}/link/callback".at(complete_harness_link),))
        .into_route_nodes()
}
