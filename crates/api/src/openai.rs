//! The `OpenAI` side of the Codex sign-in.
//!
//! Codex's browser flow ends at `http://localhost:1455/auth/callback`, which
//! a hosted web app cannot offer, so flyco runs the other flow the CLI
//! already has: `codex login --device-auth`. Three calls, verified against
//! `codex-rs/login/src/{device_code_auth,server}.rs`:
//!
//! 1. [`usercode_request`] asks for a device authorization and gets back the
//!    code the user types and how often to ask about it.
//! 2. [`device_token_request`] asks whether it has been approved yet.
//!    `FORBIDDEN` and `NOT_FOUND` both mean "not yet".
//! 3. [`token_request`] redeems the authorization code that approval yields,
//!    and later redeems the refresh token, at the same OAuth token endpoint.
//!
//! Everything that leaves the control plane for `OpenAI` goes through an
//! [`HttpTransport`](flyco_provider::HttpTransport), which is zenwave in
//! production and a table of recorded exchanges under test — the same
//! arrangement [`crate::anthropic`] uses, and for the same reason: it is the
//! only way to pin what actually goes on the wire.
//!
//! # Reading a token without verifying it
//!
//! Two things flyco needs are inside the JWTs `OpenAI` hands back and
//! nowhere else: `chatgpt_account_id`, which is the workspace a session's
//! requests are billed to, and the access token's `exp`, which is when the
//! grant has to be renewed. Both are read with
//! [`deserialize_claims_unchecked`](jwt_compact::UntrustedToken::deserialize_claims_unchecked).
//!
//! That is safe *here* and would not be somewhere else. These tokens are not
//! presented to flyco by a caller: they were produced by `auth.openai.com`
//! moments earlier, in the TLS response to a request this module made, and
//! they never leave for anywhere but Codex on the user's own machine.
//! Nothing here is an authorization decision — a forged claim would label a
//! card wrongly and schedule a refresh at the wrong minute, not admit
//! anybody to anything. Verifying the signature would mean fetching and
//! caching `OpenAI`'s JWKS to re-answer a question the transport already
//! answered.

use core::future::Future;

use flyco_provider::http::{HttpRequest, HttpResponse, Method};
use flyco_provider::{HttpError, HttpTransport, LiveTransport};
use jwt_compact::{Empty, UntrustedToken};
use serde::{Deserialize, Serialize};

/// Where a device authorization is created.
const USERCODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";

/// Where a device authorization is asked whether it has been approved.
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";

/// Where an authorization code and a refresh token are both redeemed.
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// The redirect URI the device grant is bound to.
///
/// `OpenAI`'s own page, not flyco's: the device flow never redirects a
/// browser back to the client, which is what lets a Worker with no callback
/// route run it.
pub const REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";

/// Where the user approves the code, the same link `codex login
/// --device-auth` prints.
pub const VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";

/// Where a person turns device-code authorization on for their account.
///
/// `OpenAI` keeps the device flow off by default — it is more open to social
/// engineering than a browser redirect — so a refused
/// [`usercode_request`] is normally this switch rather than anything flyco
/// did. The problem document names this page so the card can link to it.
pub const DEVICE_AUTH_SETTINGS_URL: &str = "https://chatgpt.com/#settings/Security";

/// How often to poll when `OpenAI` states no interval of its own.
///
/// Codex's own client defaults the field to zero and then sleeps for zero,
/// which spins. The poller here is a browser, so flyco states a floor
/// instead: a missing or zero interval becomes this.
pub const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;

/// One device authorization, as `OpenAI` creates it.
#[derive(Clone, Deserialize)]
pub struct DeviceAuth {
    /// Names this authorization when polling. Secret: it is half of what
    /// redeems the grant, so it never reaches the browser.
    pub device_auth_id: String,
    /// The one-time code the user types at [`VERIFICATION_URL`].
    #[serde(alias = "usercode")]
    pub user_code: String,
    /// Seconds between polls, which `OpenAI` states as a string.
    #[serde(default)]
    interval: Option<Interval>,
}

impl core::fmt::Debug for DeviceAuth {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DeviceAuth")
            .field("user_code", &self.user_code)
            .finish_non_exhaustive()
    }
}

impl DeviceAuth {
    /// Seconds to wait between polls, never zero.
    #[must_use]
    pub fn interval_seconds(&self) -> u64 {
        match self.interval {
            Some(Interval::Seconds(seconds)) if seconds > 0 => seconds,
            Some(Interval::Text(ref text)) => text
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|seconds| *seconds > 0)
                .unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS),
            _ => DEFAULT_POLL_INTERVAL_SECONDS,
        }
    }
}

/// `interval` as `OpenAI` sends it, which is a string in the flow Codex
/// speaks and a number in the OAuth device-grant RFC it is modelled on.
#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum Interval {
    /// A JSON number.
    Seconds(u64),
    /// A JSON string holding a number.
    Text(String),
}

/// What one poll of a device authorization found.
#[derive(Debug, Clone)]
pub enum DevicePoll {
    /// Nobody has approved the code yet.
    Pending,
    /// The code was approved, and this is the grant it produced.
    Approved(DeviceCode),
}

/// The authorization code an approved device authorization yields.
///
/// The PKCE verifier comes back with it: `OpenAI` minted the pair, so the
/// device client never generates one of its own.
#[derive(Clone, Deserialize)]
pub struct DeviceCode {
    /// Redeemed at the token endpoint.
    pub authorization_code: String,
    /// The verifier whose challenge the authorization was bound to.
    pub code_verifier: String,
}

impl core::fmt::Debug for DeviceCode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DeviceCode").finish_non_exhaustive()
    }
}

/// Which OAuth grant a token request is.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum GrantType {
    /// Redeems the code an approved device authorization yielded.
    AuthorizationCode,
    /// Redeems a stored refresh token for a fresh set.
    RefreshToken,
}

/// The form body of an authorization-code redemption.
#[derive(Debug, Serialize)]
struct AuthorizationCodeForm<'a> {
    grant_type: GrantType,
    code: &'a str,
    redirect_uri: &'a str,
    client_id: &'a str,
    code_verifier: &'a str,
}

/// The JSON body of a refresh.
///
/// JSON rather than a form, and the fields in this order, because that is
/// what `codex-rs/login/src/auth/manager.rs` sends: the refresh path is the
/// one exchange where `OpenAI`'s own client does not use the form encoding
/// RFC 6749 defines, and flyco speaks the bytes the CLI speaks rather than
/// the bytes the RFC would allow.
#[derive(Debug, Serialize)]
struct RefreshTokenBody<'a> {
    client_id: &'a str,
    grant_type: GrantType,
    refresh_token: &'a str,
}

/// One request to `OpenAI`'s token endpoint.
///
/// One endpoint, two media types: the Codex CLI form-encodes the code
/// redemption and sends the refresh as JSON, so [`token_request`] does
/// exactly that rather than picking one for both.
#[derive(Debug, Clone, Copy)]
pub enum TokenRequest<'a> {
    /// Redeems the code an approved device authorization yielded.
    AuthorizationCode {
        /// `authorization_code` from [`DeviceCode`].
        code: &'a str,
        /// Must be [`REDIRECT_URI`], which the grant is bound to.
        redirect_uri: &'a str,
        /// The Codex OAuth client id this deployment presents.
        client_id: &'a str,
        /// `code_verifier` from [`DeviceCode`].
        code_verifier: &'a str,
    },
    /// Exchanges a stored refresh token for a fresh set.
    RefreshToken {
        /// The refresh token from a previous exchange.
        refresh_token: &'a str,
        /// The same client id the grant was issued to.
        client_id: &'a str,
    },
}

/// What `OpenAI` hands back for either grant.
///
/// Every field is optional because a refresh legitimately omits the halves
/// it did not rotate; a code redemption that omits any of them is a
/// malformed answer, which is what [`issued`](Self::issued) says.
#[derive(Clone, Deserialize)]
pub struct TokenSet {
    /// The `ChatGPT` id token, when this answer carries one.
    pub id_token: Option<String>,
    /// The bearer token, when this answer carries one.
    pub access_token: Option<String>,
    /// The next refresh token, when this answer rotates it.
    pub refresh_token: Option<String>,
}

impl core::fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TokenSet")
            .field("rotated_refresh_token", &self.refresh_token.is_some())
            .finish_non_exhaustive()
    }
}

impl TokenSet {
    /// The complete grant a code redemption must have produced.
    ///
    /// # Errors
    ///
    /// Returns [`OpenAiError::Malformed`] if `OpenAI` answered a code
    /// redemption without all three tokens, which is an answer flyco cannot
    /// store as an account.
    pub fn issued(self) -> Result<Grant, OpenAiError> {
        Ok(Grant {
            id_token: self.id_token.ok_or(OpenAiError::Malformed(
                "the token response carries no id_token",
            ))?,
            access_token: self.access_token.ok_or(OpenAiError::Malformed(
                "the token response carries no access_token",
            ))?,
            refresh_token: self.refresh_token.ok_or(OpenAiError::Malformed(
                "the token response carries no refresh_token",
            ))?,
        })
    }

    /// `previous`, with whatever this refresh rotated written over it.
    ///
    /// A refresh that returns only an access token leaves the other two
    /// alone: they are still the tokens the account is signed in with, and
    /// dropping them would unlink an account that `OpenAI` just renewed.
    #[must_use]
    pub fn rotated(self, previous: &Grant) -> Grant {
        Grant {
            id_token: self.id_token.unwrap_or_else(|| previous.id_token.clone()),
            access_token: self
                .access_token
                .unwrap_or_else(|| previous.access_token.clone()),
            refresh_token: self
                .refresh_token
                .unwrap_or_else(|| previous.refresh_token.clone()),
        }
    }
}

/// The three tokens a `ChatGPT` sign-in is made of.
#[derive(Clone, PartialEq, Eq)]
pub struct Grant {
    /// Names the account and the workspace.
    pub id_token: String,
    /// What the agent runs under, until its `exp`.
    pub access_token: String,
    /// Redeemed for the next set.
    pub refresh_token: String,
}

impl core::fmt::Debug for Grant {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Grant").finish_non_exhaustive()
    }
}

/// The `https://api.openai.com/auth` claim of a `ChatGPT` id token.
#[derive(Debug, Default, Deserialize)]
struct AuthClaim {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
}

/// The `https://api.openai.com/profile` claim of a `ChatGPT` id token.
#[derive(Debug, Default, Deserialize)]
struct ProfileClaim {
    #[serde(default)]
    email: Option<String>,
}

/// The claims of a `ChatGPT` id token flyco reads.
#[derive(Debug, Default, Deserialize)]
struct IdClaims {
    #[serde(default)]
    email: Option<String>,
    #[serde(rename = "https://api.openai.com/auth", default)]
    auth: Option<AuthClaim>,
    #[serde(rename = "https://api.openai.com/profile", default)]
    profile: Option<ProfileClaim>,
}

impl Grant {
    /// The workspace this grant belongs to, from the id token.
    ///
    /// # Errors
    ///
    /// Returns [`OpenAiError::Malformed`] if the id token is not a readable
    /// JWT or names no `chatgpt_account_id` — without it Codex cannot say
    /// which workspace a request is billed to.
    pub fn account_id(&self) -> Result<String, OpenAiError> {
        claims::<IdClaims>(&self.id_token)?
            .auth
            .and_then(|auth| auth.chatgpt_account_id)
            .ok_or(OpenAiError::Malformed(
                "the id token names no chatgpt_account_id",
            ))
    }

    /// The address to label the linked account with, if the id token names
    /// one.
    #[must_use]
    pub fn email_address(&self) -> Option<String> {
        let claims = claims::<IdClaims>(&self.id_token).ok()?;
        claims
            .email
            .or_else(|| claims.profile.and_then(|profile| profile.email))
    }

    /// When the access token stops working, as a Unix timestamp.
    ///
    /// # Errors
    ///
    /// Returns [`OpenAiError::Malformed`] if the access token is not a
    /// readable JWT or carries no `exp`: a grant with no known end is one
    /// flyco cannot renew before a session runs on it.
    pub fn expires_at_unix(&self) -> Result<u64, OpenAiError> {
        let token = UntrustedToken::new(&self.access_token)
            .map_err(|_| OpenAiError::Malformed("the access token is not a JWT"))?;
        let expiration = token
            .deserialize_claims_unchecked::<Empty>()
            .map_err(|_| OpenAiError::Malformed("the access token's claims are not readable"))?
            .expiration
            .ok_or(OpenAiError::Malformed("the access token carries no exp"))?;
        u64::try_from(expiration.timestamp())
            .map_err(|_| OpenAiError::Malformed("the access token expires before the Unix epoch"))
    }
}

/// Reads one token's custom claims without checking its signature.
///
/// One helper for all three vendors that hand flyco an id token — see
/// [`crate::jwt`], which states once why reading one unverified is the right
/// thing here and would not be elsewhere.
fn claims<T: serde::de::DeserializeOwned>(jwt: &str) -> Result<T, OpenAiError> {
    crate::jwt::claims(jwt).map_err(|error| OpenAiError::Malformed(error.detail()))
}

/// The OAuth error document a refusal carries.
#[derive(Debug, Clone, Deserialize)]
struct OauthError {
    error: String,
    error_description: Option<String>,
}

/// Why a call to `OpenAI` did not produce what flyco asked for.
#[derive(Debug, thiserror::Error)]
pub enum OpenAiError {
    /// The request never completed, or the response was not the expected
    /// JSON.
    #[error("OpenAI request failed: {0}")]
    Transport(String),
    /// `OpenAI` refused to create a device authorization at all.
    ///
    /// Its own answer to this is a bare `404`, which it also uses for a
    /// device authorization that has expired — but a *creation* cannot name
    /// something that expired, so at that endpoint it means exactly one
    /// thing: this account does not have device-code authorization turned
    /// on.
    #[error(
        "OpenAI will not start a device sign-in for this account; turn on device code \
         authorization at {DEVICE_AUTH_SETTINGS_URL} (a workspace admin does it for a workspace \
         account) and try again"
    )]
    DeviceAuthDisabled,
    /// `OpenAI` refused the grant and said why.
    #[error("{code}: {description}")]
    Rejected {
        /// `OpenAI`'s machine-readable error code.
        code: String,
        /// `OpenAI`'s human-readable explanation.
        description: String,
    },
    /// `OpenAI` answered with something flyco cannot use.
    #[error("OpenAI answered with something flyco cannot use: {0}")]
    Malformed(&'static str),
    /// `OpenAI` answered with a status flyco cannot interpret.
    #[error("OpenAI responded with HTTP {0}")]
    Status(u16),
}

impl From<HttpError> for OpenAiError {
    fn from(error: HttpError) -> Self {
        Self::Transport(error.to_string())
    }
}

/// Body of the device-authorization creation.
#[derive(Debug, Serialize)]
struct UserCodeBody<'a> {
    client_id: &'a str,
}

/// Body of one poll of a device authorization.
#[derive(Debug, Serialize)]
struct DeviceTokenBody<'a> {
    device_auth_id: &'a str,
    user_code: &'a str,
}

/// Describes the device-authorization creation as an [`HttpRequest`].
///
/// # Errors
///
/// Returns [`HttpError::Encoding`] if the body does not serialize, which
/// would mean this module's own request type is malformed.
pub fn usercode_request(client_id: &str) -> Result<HttpRequest, HttpError> {
    HttpRequest::new(Method::Post, USERCODE_URL)
        .header("accept", "application/json")
        .json_body(&UserCodeBody { client_id })
}

/// Describes one poll of a device authorization as an [`HttpRequest`].
///
/// # Errors
///
/// Returns [`HttpError::Encoding`] if the body does not serialize.
pub fn device_token_request(
    device_auth_id: &str,
    user_code: &str,
) -> Result<HttpRequest, HttpError> {
    HttpRequest::new(Method::Post, DEVICE_TOKEN_URL)
        .header("accept", "application/json")
        .json_body(&DeviceTokenBody {
            device_auth_id,
            user_code,
        })
}

/// Describes one token exchange as an [`HttpRequest`].
///
/// The media type is the grant's, not the endpoint's: `codex login`
/// form-encodes the authorization-code redemption — RFC 6749 §4.1.3's own
/// encoding — and sends the refresh as JSON. Flyco sends the same bytes for
/// each, because the point of pinning these requests is that a deployed
/// control plane and the CLI are indistinguishable to `OpenAI`.
///
/// # Errors
///
/// Returns [`HttpError::Encoding`] if the body does not serialize, which
/// would mean this module's own request type is malformed.
pub fn token_request(request: TokenRequest<'_>) -> Result<HttpRequest, HttpError> {
    let endpoint = HttpRequest::new(Method::Post, TOKEN_URL).header("accept", "application/json");

    match request {
        TokenRequest::AuthorizationCode {
            code,
            redirect_uri,
            client_id,
            code_verifier,
        } => {
            let form = serde_urlencoded::to_string(AuthorizationCodeForm {
                grant_type: GrantType::AuthorizationCode,
                code,
                redirect_uri,
                client_id,
                code_verifier,
            })
            .map_err(|error| HttpError::Encoding(error.to_string()))?;
            Ok(endpoint.body("application/x-www-form-urlencoded", form.into_bytes()))
        }
        TokenRequest::RefreshToken {
            refresh_token,
            client_id,
        } => endpoint.json_body(&RefreshTokenBody {
            client_id,
            grant_type: GrantType::RefreshToken,
            refresh_token,
        }),
    }
}

/// Turns a refusal into the reason `OpenAI` gave for it.
fn refusal(response: &HttpResponse) -> OpenAiError {
    response.json::<OauthError>().map_or_else(
        |_| OpenAiError::Status(response.status),
        |failure| OpenAiError::Rejected {
            description: failure
                .error_description
                .unwrap_or_else(|| "no description".to_owned()),
            code: failure.error,
        },
    )
}

/// Reads a JSON body, or reports why the exchange failed.
fn decoded<T: serde::de::DeserializeOwned>(response: &HttpResponse) -> Result<T, OpenAiError> {
    if response.is_success() {
        return response
            .json::<T>()
            .map_err(|error| OpenAiError::Transport(error.to_string()));
    }
    Err(refusal(response))
}

/// Creates one device authorization over any transport.
///
/// # Errors
///
/// Returns [`OpenAiError::DeviceAuthDisabled`] when `OpenAI` will not start
/// a device sign-in for this account, and [`OpenAiError`] otherwise.
pub async fn request_user_code_over<T: HttpTransport>(
    transport: &T,
    client_id: &str,
) -> Result<DeviceAuth, OpenAiError> {
    let response = transport.send(usercode_request(client_id)?).await?;
    if response.status == 404 {
        return Err(OpenAiError::DeviceAuthDisabled);
    }
    decoded(&response)
}

/// Polls one device authorization once over any transport.
///
/// `403` and `404` are both "not approved yet", exactly as `codex login`
/// reads them: `OpenAI` uses the first while the code is outstanding and
/// the second in the window before it is registered.
///
/// # Errors
///
/// Returns [`OpenAiError`] if the poll cannot be sent or `OpenAI` answers
/// something that is neither an approval nor a wait.
pub async fn poll_device_code_over<T: HttpTransport>(
    transport: &T,
    device_auth_id: &str,
    user_code: &str,
) -> Result<DevicePoll, OpenAiError> {
    let response = transport
        .send(device_token_request(device_auth_id, user_code)?)
        .await?;

    if response.is_success() {
        return Ok(DevicePoll::Approved(decoded(&response)?));
    }
    if matches!(response.status, 403 | 404) {
        return Ok(DevicePoll::Pending);
    }
    Err(refusal(&response))
}

/// Performs one token exchange over any transport.
///
/// # Errors
///
/// Returns [`OpenAiError`] if the request cannot be built or sent, or if
/// `OpenAI` refuses the grant.
pub async fn exchange_over<T: HttpTransport>(
    transport: &T,
    request: TokenRequest<'_>,
) -> Result<TokenSet, OpenAiError> {
    let response = transport.send(token_request(request)?).await?;
    decoded(&response)
}

/// The three calls the Codex sign-in and the refresh-on-use path make.
///
/// Behind a trait for the same reason [`crate::anthropic::ClaudeOauth`] is:
/// a route handler cannot be exercised without standing in for
/// `auth.openai.com`.
pub trait CodexOauth: Send + Sync + Clone + 'static {
    /// Creates a device authorization.
    ///
    /// # Errors
    ///
    /// Returns [`OpenAiError`] if the call fails or `OpenAI` will not start
    /// a device sign-in for this account.
    fn request_user_code(
        &self,
        client_id: &str,
    ) -> impl Future<Output = Result<DeviceAuth, OpenAiError>> + Send;

    /// Asks once whether a device authorization has been approved.
    ///
    /// # Errors
    ///
    /// Returns [`OpenAiError`] if the call fails or the authorization is
    /// gone.
    fn poll_device_code(
        &self,
        device_auth_id: &str,
        user_code: &str,
    ) -> impl Future<Output = Result<DevicePoll, OpenAiError>> + Send;

    /// Redeems an authorization code or a refresh token.
    ///
    /// # Errors
    ///
    /// Returns [`OpenAiError`] if the exchange fails or `OpenAI` refuses
    /// the grant.
    fn exchange(
        &self,
        request: TokenRequest<'_>,
    ) -> impl Future<Output = Result<TokenSet, OpenAiError>> + Send;
}

/// The production client, speaking HTTP through zenwave — hyper natively,
/// Fetch inside the Worker.
#[derive(Debug, Clone, Copy, Default)]
pub struct ZenwaveCodex {
    transport: LiveTransport,
}

impl ZenwaveCodex {
    /// Creates the client.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            transport: LiveTransport::new(),
        }
    }
}

impl CodexOauth for ZenwaveCodex {
    async fn request_user_code(&self, client_id: &str) -> Result<DeviceAuth, OpenAiError> {
        request_user_code_over(&self.transport, client_id).await
    }

    async fn poll_device_code(
        &self,
        device_auth_id: &str,
        user_code: &str,
    ) -> Result<DevicePoll, OpenAiError> {
        poll_device_code_over(&self.transport, device_auth_id, user_code).await
    }

    async fn exchange(&self, request: TokenRequest<'_>) -> Result<TokenSet, OpenAiError> {
        exchange_over(&self.transport, request).await
    }
}

/// The Codex client the router and the provisioning consumer carry.
///
/// An enum rather than a type parameter for the same reason
/// [`ClaudeClient`](crate::anthropic::ClaudeClient) is one: `#[skyzen::openapi]`
/// cannot annotate a generic handler.
#[derive(Debug, Clone)]
pub enum CodexClient {
    /// Talks to `auth.openai.com`.
    Live(ZenwaveCodex),
    /// Answers from fixtures, for tests.
    #[cfg(test)]
    Fake(crate::testing::TestCodex),
}

impl Default for CodexClient {
    fn default() -> Self {
        Self::Live(ZenwaveCodex::new())
    }
}

impl CodexOauth for CodexClient {
    async fn request_user_code(&self, client_id: &str) -> Result<DeviceAuth, OpenAiError> {
        match self {
            Self::Live(client) => client.request_user_code(client_id).await,
            #[cfg(test)]
            Self::Fake(client) => client.request_user_code(client_id).await,
        }
    }

    async fn poll_device_code(
        &self,
        device_auth_id: &str,
        user_code: &str,
    ) -> Result<DevicePoll, OpenAiError> {
        match self {
            Self::Live(client) => client.poll_device_code(device_auth_id, user_code).await,
            #[cfg(test)]
            Self::Fake(client) => client.poll_device_code(device_auth_id, user_code).await,
        }
    }

    async fn exchange(&self, request: TokenRequest<'_>) -> Result<TokenSet, OpenAiError> {
        match self {
            Self::Live(client) => client.exchange(request).await,
            #[cfg(test)]
            Self::Fake(client) => client.exchange(request).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use flyco_provider::http::HttpResponse;
    use flyco_provider::testing::RecordedTransport;

    use super::{
        DEFAULT_POLL_INTERVAL_SECONDS, DevicePoll, Grant, OpenAiError, REDIRECT_URI, TokenRequest,
        exchange_over, poll_device_code_over, request_user_code_over,
    };

    /// A device authorization as `OpenAI` creates one.
    const USERCODE_BODY: &str = include_str!("../fixtures/openai/usercode.json");

    /// The refusal that means device-code login is switched off.
    const DISABLED_BODY: &str = include_str!("../fixtures/openai/device_auth_disabled.json");

    /// What an outstanding device authorization answers a poll with.
    const PENDING_BODY: &str = include_str!("../fixtures/openai/pending.json");

    /// What an approved device authorization answers a poll with.
    const DEVICE_CODE_BODY: &str = include_str!("../fixtures/openai/device_code.json");

    /// A token response as `OpenAI` returns one for a code redemption.
    const TOKEN_BODY: &str = include_str!("../fixtures/openai/token.json");

    /// A token response as `OpenAI` returns one for a refresh.
    const REFRESHED_BODY: &str = include_str!("../fixtures/openai/refreshed.json");

    /// A refusal as `OpenAI` returns one.
    const REFUSAL_BODY: &str = include_str!("../fixtures/openai/invalid_grant.json");

    /// One field of a form-encoded body.
    fn field(body: &str, name: &str) -> String {
        url::form_urlencoded::parse(body.as_bytes())
            .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
            .unwrap_or_else(|| panic!("the form body carries `{name}`"))
    }

    #[skyzen::test]
    async fn a_usercode_request_asks_openai_for_a_device_authorization() {
        let transport = RecordedTransport::new(vec![HttpResponse::new(200, USERCODE_BODY)]);
        let device = request_user_code_over(&transport, "app_client")
            .await
            .expect("create a device authorization");

        let request = transport.request(0);
        assert_eq!(request.method.as_str(), "POST");
        assert_eq!(
            request.url,
            "https://auth.openai.com/api/accounts/deviceauth/usercode"
        );
        let body: serde_json::Value =
            serde_json::from_str(request.body_text().expect("UTF-8")).expect("a JSON body");
        assert_eq!(body["client_id"], "app_client");

        assert_eq!(device.device_auth_id, "devauth_01JD5XKQZ8");
        assert_eq!(device.user_code, "FLYC-8QK2");
        // OpenAI states the interval as a string.
        assert_eq!(device.interval_seconds(), 5);
    }

    #[skyzen::test]
    async fn a_refused_usercode_request_is_device_code_login_being_switched_off() {
        let transport = RecordedTransport::new(vec![HttpResponse::new(404, DISABLED_BODY)]);
        let error = request_user_code_over(&transport, "app_client")
            .await
            .expect_err("a refused device sign-in");

        assert!(matches!(error, OpenAiError::DeviceAuthDisabled));
        assert!(error.to_string().contains("chatgpt.com"));
    }

    #[skyzen::test]
    async fn an_outstanding_device_authorization_polls_as_pending() {
        let transport = RecordedTransport::new(vec![
            HttpResponse::new(403, PENDING_BODY),
            HttpResponse::new(404, PENDING_BODY),
        ]);

        for _ in 0..2 {
            let poll = poll_device_code_over(&transport, "devauth", "FLYC-8QK2")
                .await
                .expect("a poll of an outstanding authorization");
            assert!(matches!(poll, DevicePoll::Pending));
        }

        let body: serde_json::Value =
            serde_json::from_str(transport.request(0).body_text().expect("UTF-8"))
                .expect("a JSON body");
        assert_eq!(
            transport.request(0).url,
            "https://auth.openai.com/api/accounts/deviceauth/token"
        );
        assert_eq!(body["device_auth_id"], "devauth");
        assert_eq!(body["user_code"], "FLYC-8QK2");
    }

    #[skyzen::test]
    async fn an_approved_device_authorization_yields_a_code_and_its_verifier() {
        let transport = RecordedTransport::new(vec![HttpResponse::new(200, DEVICE_CODE_BODY)]);
        let poll = poll_device_code_over(&transport, "devauth", "FLYC-8QK2")
            .await
            .expect("a poll of an approved authorization");

        let DevicePoll::Approved(code) = poll else {
            panic!("an approved authorization must poll as approved");
        };
        assert_eq!(code.authorization_code, "ac_01JD5XKQZ8-device");
        assert_eq!(code.code_verifier, "cGtjZS12ZXJpZmllci1mcm9tLW9wZW5haQ");
    }

    #[skyzen::test]
    async fn a_code_exchange_posts_exactly_what_openai_expects() {
        let transport = RecordedTransport::new(vec![HttpResponse::new(200, TOKEN_BODY)]);
        let tokens = exchange_over(
            &transport,
            TokenRequest::AuthorizationCode {
                code: "ac_01JD5XKQZ8-device",
                redirect_uri: REDIRECT_URI,
                client_id: "app_client",
                code_verifier: "the-verifier",
            },
        )
        .await
        .expect("redeem the code");

        let request = transport.request(0);
        assert_eq!(request.method.as_str(), "POST");
        assert_eq!(request.url, "https://auth.openai.com/oauth/token");
        assert_eq!(
            request
                .headers
                .iter()
                .find(|(name, _)| name == "content-type")
                .map(|(_, value)| value.as_str()),
            Some("application/x-www-form-urlencoded")
        );
        let body = request.body_text().expect("UTF-8");
        assert_eq!(field(body, "grant_type"), "authorization_code");
        assert_eq!(field(body, "code"), "ac_01JD5XKQZ8-device");
        assert_eq!(field(body, "redirect_uri"), REDIRECT_URI);
        assert_eq!(field(body, "client_id"), "app_client");
        assert_eq!(field(body, "code_verifier"), "the-verifier");

        let grant = tokens.issued().expect("a complete grant");
        assert_eq!(grant.refresh_token, "rt_01JD5XKQZ8-issued");
        assert_eq!(grant.account_id().expect("an account id"), "acc_01JD5XKQZ8");
        assert_eq!(grant.email_address().as_deref(), Some("me@lexo.cool"));
        assert_eq!(grant.expires_at_unix().expect("an expiry"), 1_787_003_600);
    }

    #[skyzen::test]
    async fn a_refresh_sends_the_json_body_the_codex_cli_sends() {
        let transport = RecordedTransport::new(vec![HttpResponse::new(200, REFRESHED_BODY)]);
        let tokens = exchange_over(
            &transport,
            TokenRequest::RefreshToken {
                refresh_token: "rt_01JD5XKQZ8-issued",
                client_id: "app_client",
            },
        )
        .await
        .expect("refresh the grant");

        // The one exchange OpenAI's own client does *not* form-encode. It
        // is the same endpoint as the code redemption above, and a
        // different media type, which is exactly the kind of thing a
        // refactor unifies by accident.
        let request = transport.request(0);
        assert_eq!(request.url, "https://auth.openai.com/oauth/token");
        assert_eq!(
            request
                .headers
                .iter()
                .find(|(name, _)| name == "content-type")
                .map(|(_, value)| value.as_str()),
            Some("application/json")
        );
        let body: serde_json::Value =
            serde_json::from_str(request.body_text().expect("UTF-8")).expect("a JSON body");
        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["refresh_token"], "rt_01JD5XKQZ8-issued");
        assert_eq!(body["client_id"], "app_client");
        assert!(body.get("code_verifier").is_none());
        assert!(body.get("redirect_uri").is_none());

        let previous = Grant {
            id_token: "stale-id".to_owned(),
            access_token: "stale-access".to_owned(),
            refresh_token: "rt_01JD5XKQZ8-issued".to_owned(),
        };
        let rotated = tokens.rotated(&previous);
        assert_eq!(rotated.refresh_token, "rt_01JD5XKQZ8-renewed");
        assert_eq!(rotated.expires_at_unix().expect("an expiry"), 1_787_007_200);
    }

    #[skyzen::test]
    async fn a_refresh_that_rotates_nothing_keeps_what_the_account_already_has() {
        let transport = RecordedTransport::new(vec![HttpResponse::new(200, "{}")]);
        let previous = Grant {
            id_token: "the-id".to_owned(),
            access_token: "the-access".to_owned(),
            refresh_token: "the-refresh".to_owned(),
        };
        let rotated = exchange_over(
            &transport,
            TokenRequest::RefreshToken {
                refresh_token: "the-refresh",
                client_id: "app_client",
            },
        )
        .await
        .expect("refresh the grant")
        .rotated(&previous);

        assert_eq!(rotated, previous);
    }

    #[skyzen::test]
    async fn a_refusal_keeps_openais_own_reason() {
        let transport = RecordedTransport::new(vec![HttpResponse::new(400, REFUSAL_BODY)]);
        let error = exchange_over(
            &transport,
            TokenRequest::RefreshToken {
                refresh_token: "stale",
                client_id: "app_client",
            },
        )
        .await
        .expect_err("a refused grant");

        assert!(matches!(
            error,
            OpenAiError::Rejected { code, description }
                if code == "invalid_grant" && description.contains("expired")
        ));
    }

    #[skyzen::test]
    async fn an_unreadable_failure_is_reported_as_its_status() {
        let transport = RecordedTransport::new(vec![HttpResponse::new(503, "<html>nope</html>")]);
        let error = exchange_over(
            &transport,
            TokenRequest::RefreshToken {
                refresh_token: "fine",
                client_id: "app_client",
            },
        )
        .await
        .expect_err("an outage page");

        assert!(matches!(error, OpenAiError::Status(503)));
    }

    #[test]
    fn a_grant_whose_tokens_are_not_jwts_is_refused_rather_than_guessed_at() {
        let grant = Grant {
            id_token: "not-a-jwt".to_owned(),
            access_token: "not-a-jwt".to_owned(),
            refresh_token: "rt".to_owned(),
        };
        assert!(matches!(grant.account_id(), Err(OpenAiError::Malformed(_))));
        assert!(matches!(
            grant.expires_at_unix(),
            Err(OpenAiError::Malformed(_))
        ));
        assert!(grant.email_address().is_none());
    }

    #[skyzen::test]
    async fn a_device_authorization_with_no_interval_still_names_one() {
        let transport = RecordedTransport::new(vec![HttpResponse::new(
            200,
            "{\"device_auth_id\":\"d\",\"user_code\":\"C\"}",
        )]);
        let device = request_user_code_over(&transport, "app_client")
            .await
            .expect("create a device authorization");
        assert_eq!(device.interval_seconds(), DEFAULT_POLL_INTERVAL_SECONDS);
    }

    #[test]
    fn a_grant_missing_a_token_is_not_an_account() {
        let partial = super::TokenSet {
            id_token: Some("id".to_owned()),
            access_token: None,
            refresh_token: Some("rt".to_owned()),
        };
        assert!(matches!(partial.issued(), Err(OpenAiError::Malformed(_))));
    }
}
