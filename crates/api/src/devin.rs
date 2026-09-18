//! The Devin side of linking a Devin account.
//!
//! Devin runs a PKCE-only public OAuth client — the flow `devin auth
//! login` runs. Its redirect URI allowlist admits only localhost-shaped
//! addresses, because the CLI binds a port there and reads the code off
//! the redirect, and nothing on a flyco user's machine can listen. What
//! flyco runs instead is the CLI's own port-free variant, the one
//! `devin auth login --force-manual-token-flow` runs: the authorize URL
//! carries no `redirect_uri` and is marked `cli_pkce_marker=1`, and after
//! sign-in Devin's page shows the authorization code itself, which the
//! user copies into flyco (see [`crate::devin_oauth`]). The paste-a-key
//! route stays beside it for a token minted in Devin's settings.
//!
//! The two routes mint different strings — a pasted token is already a
//! `devi…` key, while the exchange answers a bare session JWT that opens
//! `/v3/self` only wrapped as `devin-session-token$<jwt>`, the shape
//! `devin auth login` itself stores — but the credential's label is
//! not the caller's to choose either way: `GET /v3/self` names
//! the principal it opens, so linking asks Devin who the credential
//! belongs to rather than trusting a string in the request. The call
//! doubles as validation: a credential Devin refuses is refused here, at
//! link time, instead of being stored and discovered by the first session
//! that tries to run on it.
//!
//! Everything that leaves the control plane for Devin goes through an
//! [`HttpTransport`](flyco_provider::HttpTransport), which is zenwave in
//! production and a table of recorded exchanges under test — the same seam
//! [`crate::anthropic`] pins its token exchanges on.

use core::future::Future;

use flyco_provider::http::{HttpRequest, HttpResponse, Method};
use flyco_provider::{HttpError, HttpTransport, LiveTransport};
use serde::{Deserialize, Serialize};
use url::Url;

/// Where a key proves itself and names its principal.
const SELF_URL: &str = "https://api.devin.ai/v3/self";

/// Where the browser approves the grant.
const AUTHORIZE_URL: &str = "https://app.devin.ai/auth/cli/continue";

/// Where an authorization code is redeemed for the session JWT.
const TOKEN_URL: &str = "https://api.devin.ai/auth/cli/token";

/// The wrapper that makes the exchange's answer a usable credential.
///
/// `/auth/cli/token` answers a bare session JWT, which `/v3/self`
/// refuses; the credential `devin auth login` writes into
/// `credentials.toml`'s `windsurf_api_key` wraps it as
/// `devin-session-token$<jwt>`, so the exchange mints the wrapped form
/// directly.
const SESSION_TOKEN_PREFIX: &str = "devin-session-token$";

/// The principal a credential authenticates as.
///
/// `/v3/self` answers one of four shapes, tagged by `principal_type`: the
/// `devin-session-token$…` credential `devin auth login` writes is a
/// `windsurf_session`, a PAT is a
/// `pat_user`, and a `cog_…` service-user key is a `service_user`.
/// `devin_brain` is included for completeness — flyco never issues one,
/// but a key that resolves to it is still a valid credential and links
/// under the unnamed-account label.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "principal_type", rename_all = "snake_case")]
pub enum DevinSelf {
    /// A `cog_…` service-user key, named by whoever created it.
    ServiceUser {
        /// The service user's display name, e.g. "CI Bot".
        service_user_name: String,
    },
    /// A personal access token, named by the human it authenticates as.
    PatUser {
        /// The user's display name.
        user_name: String,
    },
    /// A Devin brain session — a principal with no human name.
    DevinBrain {
        /// The brain's own identifier.
        devin_id: String,
    },
    /// The `devin-session-token$…` credential `devin auth login` writes.
    WindsurfSession {
        /// The user's display name, when the principal states one.
        user_name: Option<String>,
    },
}

impl DevinSelf {
    /// The name to label the linked account with, when Devin states one.
    ///
    /// Optional rather than defaulted here because the fallback wording is
    /// the linker's choice, not a fact about the principal.
    #[must_use]
    pub fn account_name(&self) -> Option<&str> {
        match self {
            Self::ServiceUser {
                service_user_name, ..
            } => Some(service_user_name),
            Self::PatUser { user_name, .. } => Some(user_name),
            Self::WindsurfSession { user_name, .. } => user_name.as_deref(),
            Self::DevinBrain { .. } => None,
        }
    }
}

/// Why a call to Devin did not produce an identity.
#[derive(Debug, thiserror::Error)]
pub enum DevinError {
    /// The request never completed, or the response was not the expected
    /// JSON.
    #[error("Devin request failed: {0}")]
    Transport(String),
    /// Devin looked at the key and refused it — `401` or `403`.
    #[error("Devin refused this key")]
    Rejected,
    /// Devin refused to redeem the authorization code.
    ///
    /// A caller error rather than an outage: the code was mistyped,
    /// already used, or stale, and the answer is to run the flow again.
    /// The reason is Devin's own where it stated one.
    #[error("Devin refused this sign-in: {0}")]
    GrantRejected(String),
    /// Devin answered with a status flyco cannot interpret.
    #[error("Devin responded with HTTP {0}")]
    Status(u16),
}

impl From<HttpError> for DevinError {
    fn from(error: HttpError) -> Self {
        Self::Transport(error.to_string())
    }
}

/// Reads an identity response, turning a refusal into [`DevinError`].
fn identity(response: &HttpResponse) -> Result<DevinSelf, DevinError> {
    match response.status {
        401 | 403 => Err(DevinError::Rejected),
        _ if response.is_success() => response
            .json::<DevinSelf>()
            .map_err(|error| DevinError::Transport(error.to_string())),
        _ => Err(DevinError::Status(response.status)),
    }
}

/// The URL the browser opens to approve the grant.
///
/// The parameters, and their order, are the ones the Devin CLI sends for
/// its manual flow: no `redirect_uri`, and `cli_pkce_marker=1` last,
/// which is what makes Devin's page show the code instead of redirecting.
/// `state` never comes back — the page shows the bare code — so it is
/// the nonce the CLI also sends, nothing flyco reads later.
///
/// # Panics
///
/// Panics if [`AUTHORIZE_URL`] is not an absolute URL, which would mean
/// this module's own constant was edited into something that is not one.
#[must_use]
pub fn authorize_url(challenge: &str, state: &str) -> Url {
    let mut url = Url::parse(AUTHORIZE_URL).expect("the Devin authorize URL is absolute");
    url.query_pairs_mut()
        .append_pair("state", state)
        .append_pair("prompt", "select_account")
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("cli_pkce_marker", "1");
    url
}

/// The code out of what the user pasted, or `None` when the field held
/// nothing.
///
/// Devin's page shows the bare code, so the paste is the code with
/// whatever whitespace the copy picked up around it; nothing else is read
/// into it, and Devin is the one to refuse a code that is not one.
#[must_use]
pub fn pasted_code(pasted: &str) -> Option<&str> {
    let trimmed = pasted.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// What the token endpoint answers a redeemed code with.
#[derive(Debug, Clone, Deserialize)]
struct GrantResponse {
    /// The bare session JWT — not yet a usable credential; see
    /// [`SESSION_TOKEN_PREFIX`].
    token: String,
}

/// The OAuth error document a refused grant carries.
#[derive(Debug, Clone, Deserialize)]
struct OauthError {
    error: String,
    error_description: Option<String>,
}

/// Reads a token response, turning a refusal into [`DevinError`].
///
/// A `4xx` is the caller's grant being refused — the code was mistyped,
/// spent, or stale — so the reason is surfaced as
/// [`DevinError::GrantRejected`]; anything else is an answer flyco cannot
/// interpret and keeps its status. A successful answer is wrapped into
/// the `devin-session-token$…` credential shape `/v3/self` opens.
fn grant_token(response: &HttpResponse) -> Result<String, DevinError> {
    if response.is_success() {
        let body = response
            .json::<GrantResponse>()
            .map_err(|error| DevinError::Transport(error.to_string()))?;
        // An empty token is no credential at all — the exchange that
        // produced it is malformed, not successful.
        if body.token.is_empty() {
            return Err(DevinError::Transport(
                "the grant redeemed to an empty token".to_owned(),
            ));
        }
        return Ok(format!("{SESSION_TOKEN_PREFIX}{}", body.token));
    }
    if (400..500).contains(&response.status) {
        let reason = response.json::<OauthError>().ok().map_or_else(
            || format!("HTTP {}", response.status),
            |failure| {
                failure.error_description.map_or_else(
                    || failure.error.clone(),
                    |description| format!("{}: {description}", failure.error),
                )
            },
        );
        return Err(DevinError::GrantRejected(reason));
    }
    Err(DevinError::Status(response.status))
}

/// The grant-redemption request as an [`HttpRequest`].
///
/// Devin's token endpoint takes the code and the verifier as JSON — no
/// `grant_type`, no client id: the CLI's own flow is a fixed public
/// client.
///
/// # Errors
///
/// Returns [`HttpError::Encoding`] if the body does not serialize, which
/// would mean this module's own request type is malformed.
pub fn token_request(code: &str, verifier: &str) -> Result<HttpRequest, HttpError> {
    #[derive(Serialize)]
    struct Body<'a> {
        code: &'a str,
        code_verifier: &'a str,
    }
    HttpRequest::new(Method::Post, TOKEN_URL)
        .header("accept", "application/json")
        .json_body(&Body {
            code,
            code_verifier: verifier,
        })
}

/// Redeems an authorization code over any transport, returning the
/// `devin-session-token$…` credential the session JWT wraps into.
///
/// # Errors
///
/// Returns [`DevinError::GrantRejected`] when Devin refuses the grant,
/// [`DevinError::Transport`] if the call cannot complete, or
/// [`DevinError::Status`] on an answer flyco cannot interpret.
pub async fn redeem_grant_over<T: HttpTransport>(
    transport: &T,
    code: &str,
    verifier: &str,
) -> Result<String, DevinError> {
    let response = transport.send(token_request(code, verifier)?).await?;
    grant_token(&response)
}

/// The calls the link routes make.
///
/// Behind a trait for the same reason [`ClaudeOauth`](crate::anthropic::ClaudeOauth)
/// is: a route handler cannot be exercised without standing in for
/// `api.devin.ai`.
pub trait DevinApi: Send + Sync + Clone + 'static {
    /// The principal this key authenticates as.
    ///
    /// # Errors
    ///
    /// Returns [`DevinError::Rejected`] if Devin refuses the key,
    /// [`DevinError::Transport`] if the call cannot complete, or
    /// [`DevinError::Status`] on an answer flyco cannot interpret.
    fn self_identity(
        &self,
        key: &str,
    ) -> impl Future<Output = Result<DevinSelf, DevinError>> + Send;

    /// Redeems the authorization code the user copied off Devin's page
    /// for the `devin-session-token$…` session credential.
    ///
    /// # Errors
    ///
    /// Returns [`DevinError::GrantRejected`] when Devin refuses the grant
    /// — a mistyped, spent, or stale code — [`DevinError::Transport`] if
    /// the call cannot complete, or [`DevinError::Status`] on an answer
    /// flyco cannot interpret.
    fn redeem_grant(
        &self,
        code: &str,
        verifier: &str,
    ) -> impl Future<Output = Result<String, DevinError>> + Send;
}

/// The production client, speaking HTTP through zenwave — hyper natively,
/// Fetch inside the Worker.
#[derive(Debug, Clone, Copy, Default)]
pub struct ZenwaveDevin {
    transport: LiveTransport,
}

impl ZenwaveDevin {
    /// Creates the client.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            transport: LiveTransport::new(),
        }
    }
}

/// The `/v3/self` request as an [`HttpRequest`].
///
/// Shared by every transport so a recorded exchange is the same bytes the
/// deployed control plane sends.
#[must_use]
pub fn self_request(key: &str) -> HttpRequest {
    HttpRequest::new(Method::Get, SELF_URL)
        .header("accept", "application/json")
        .bearer(key)
}

/// Performs the identity read over any transport.
///
/// # Errors
///
/// Returns [`DevinError`] if the call fails or Devin refuses the key.
pub async fn self_identity_over<T: HttpTransport>(
    transport: &T,
    key: &str,
) -> Result<DevinSelf, DevinError> {
    let response = transport.send(self_request(key)).await?;
    identity(&response)
}

impl DevinApi for ZenwaveDevin {
    async fn self_identity(&self, key: &str) -> Result<DevinSelf, DevinError> {
        self_identity_over(&self.transport, key).await
    }

    async fn redeem_grant(&self, code: &str, verifier: &str) -> Result<String, DevinError> {
        redeem_grant_over(&self.transport, code, verifier).await
    }
}

/// The Devin client the router carries.
///
/// An enum rather than a type parameter for the same reason
/// [`ClaudeClient`](crate::anthropic::ClaudeClient) is one:
/// `#[skyzen::openapi]` cannot annotate a generic handler, and one
/// concrete type keeps every operation id stable.
#[derive(Debug, Clone)]
pub enum DevinClient {
    /// Talks to `api.devin.ai`.
    Live(ZenwaveDevin),
    /// Answers from fixtures, for tests.
    #[cfg(test)]
    Fake(crate::testing::TestDevin),
}

impl Default for DevinClient {
    fn default() -> Self {
        Self::Live(ZenwaveDevin::new())
    }
}

impl DevinApi for DevinClient {
    async fn self_identity(&self, key: &str) -> Result<DevinSelf, DevinError> {
        match self {
            Self::Live(client) => client.self_identity(key).await,
            #[cfg(test)]
            Self::Fake(client) => client.self_identity(key).await,
        }
    }

    async fn redeem_grant(&self, code: &str, verifier: &str) -> Result<String, DevinError> {
        match self {
            Self::Live(client) => client.redeem_grant(code, verifier).await,
            #[cfg(test)]
            Self::Fake(client) => client.redeem_grant(code, verifier).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use flyco_provider::http::HttpResponse;
    use flyco_provider::testing::RecordedTransport;

    use super::{
        DevinError, DevinSelf, authorize_url, pasted_code, redeem_grant_over, self_identity_over,
        self_request,
    };
    use crate::crypto::pkce;

    /// An identity as Devin returns one for a session credential.
    const SELF_BODY: &str = include_str!("../fixtures/devin/self.json");

    fn query(url: &url::Url, name: &str) -> String {
        url.query_pairs()
            .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
            .unwrap_or_else(|| panic!("the authorize URL carries `{name}`"))
    }

    #[test]
    fn the_authorize_url_is_the_manual_flow_the_devin_cli_runs() {
        let pkce = pkce().expect("mint a verifier");
        let url = authorize_url(&pkce.challenge, "the-state");

        assert_eq!(url.host_str(), Some("app.devin.ai"));
        assert_eq!(url.path(), "/auth/cli/continue");
        // What `devin auth login --force-manual-token-flow` sends, in its
        // order: the marker is what makes the page show the code, and a
        // redirect URI would send the browser to a port nothing binds.
        assert_eq!(
            url.query_pairs()
                .map(|(name, _)| name.into_owned())
                .collect::<Vec<_>>(),
            [
                "state",
                "prompt",
                "code_challenge",
                "code_challenge_method",
                "cli_pkce_marker"
            ]
        );
        assert_eq!(query(&url, "state"), "the-state");
        assert_eq!(query(&url, "prompt"), "select_account");
        assert_eq!(query(&url, "code_challenge"), pkce.challenge);
        assert_eq!(query(&url, "code_challenge_method"), "S256");
        assert_eq!(query(&url, "cli_pkce_marker"), "1");
    }

    #[test]
    fn a_paste_is_the_code_with_its_whitespace_dropped() {
        assert_eq!(pasted_code("  the-code\n"), Some("the-code"));
        assert_eq!(pasted_code(""), None);
        assert_eq!(pasted_code("   "), None);
    }

    #[skyzen::test]
    async fn a_code_exchange_posts_exactly_what_devin_expects() {
        // The answer is a bare session JWT; the credential is the wrapped
        // `devin-session-token$…` form `devin auth login` stores.
        let transport = RecordedTransport::new(vec![HttpResponse::new(
            200,
            r#"{"token":"eyJhbGciOiJIUzI1NiJ9.eyJzZXNzaW9uX2lkIjoid2luZHN1cmYtc2Vzc2lvbi0xIn0.sig"}"#,
        )]);
        let token = redeem_grant_over(&transport, "the-code", "the-verifier")
            .await
            .expect("redeem the code");
        assert_eq!(
            token,
            "devin-session-token$eyJhbGciOiJIUzI1NiJ9.eyJzZXNzaW9uX2lkIjoid2luZHN1cmYtc2Vzc2lvbi0xIn0.sig"
        );

        let request = transport.request(0);
        assert_eq!(request.method.as_str(), "POST");
        assert_eq!(request.url, "https://api.devin.ai/auth/cli/token");
        let body: serde_json::Value =
            serde_json::from_str(request.body_text().expect("UTF-8")).expect("a JSON body");
        assert_eq!(body["code"], "the-code");
        assert_eq!(body["code_verifier"], "the-verifier");
    }

    #[skyzen::test]
    async fn a_refused_grant_keeps_devins_own_reason() {
        let transport = RecordedTransport::new(vec![HttpResponse::new(
            400,
            r#"{"error":"invalid_grant","error_description":"The code has expired."}"#,
        )]);
        let error = redeem_grant_over(&transport, "stale", "the-verifier")
            .await
            .expect_err("a refused grant");

        assert!(
            matches!(&error, DevinError::GrantRejected(reason) if reason.contains("expired")),
            "Devin's own reason: {error:?}"
        );
    }

    #[skyzen::test]
    async fn a_tokenless_answer_is_no_credential() {
        let transport = RecordedTransport::new(vec![
            HttpResponse::new(200, r#"{"token":""}"#),
            HttpResponse::new(200, r#"{"not":"a token"}"#),
        ]);
        for _ in 0..2 {
            let error = redeem_grant_over(&transport, "the-code", "the-verifier")
                .await
                .expect_err("a malformed grant");
            assert!(matches!(error, DevinError::Transport(_)), "{error:?}");
        }
    }

    #[skyzen::test]
    async fn an_unreadable_exchange_failure_is_reported_as_its_status() {
        let transport = RecordedTransport::new(vec![HttpResponse::new(503, "<html>nope</html>")]);
        let error = redeem_grant_over(&transport, "the-code", "the-verifier")
            .await
            .expect_err("an outage page");

        assert!(matches!(error, DevinError::Status(503)));
    }

    #[test]
    fn the_self_request_is_the_documented_call() {
        let request = self_request("devi-secret-key");

        assert_eq!(request.method.as_str(), "GET");
        assert_eq!(request.url, "https://api.devin.ai/v3/self");
        assert!(request.headers.contains(&(
            "authorization".to_owned(),
            "Bearer devi-secret-key".to_owned()
        )));
    }

    #[skyzen::test]
    async fn an_identity_names_the_account_it_opens() {
        let transport = RecordedTransport::new(vec![HttpResponse::new(200, SELF_BODY)]);
        let identity = self_identity_over(&transport, "devi-secret-key")
            .await
            .expect("read the identity");

        assert_eq!(identity.account_name(), Some("Lexo Liu"));
    }

    #[skyzen::test]
    async fn a_refused_key_is_a_rejection_not_an_outage() {
        let transport = RecordedTransport::new(vec![
            HttpResponse::new(401, r#"{"title":"Unauthorized","status":401}"#),
            HttpResponse::new(403, r#"{"title":"Forbidden","status":403}"#),
        ]);
        for _ in 0..2 {
            let error = self_identity_over(&transport, "devi-bad-key")
                .await
                .expect_err("a refused key");
            assert!(matches!(error, DevinError::Rejected));
        }
    }

    #[test]
    fn every_principal_kind_decodes() {
        for (body, expected) in [
            (
                r#"{"principal_type":"service_user","service_user_id":"su-1","service_user_name":"CI Bot"}"#,
                Some("CI Bot"),
            ),
            (
                r#"{"principal_type":"pat_user","user_id":"u-1","user_name":"Lexo Liu","api_key_id":"k-1","api_key_name":"laptop"}"#,
                Some("Lexo Liu"),
            ),
            (
                r#"{"principal_type":"windsurf_session","user_id":"u-1","user_name":null,"org_id":"o-1"}"#,
                None,
            ),
            (
                r#"{"principal_type":"devin_brain","devin_id":"d-1","org_id":"o-1","user_id":"u-1"}"#,
                None,
            ),
        ] {
            let identity: DevinSelf =
                serde_json::from_str(body).expect("the documented shape decodes");
            assert_eq!(identity.account_name(), expected);
        }
    }

    #[skyzen::test]
    async fn an_unreadable_failure_is_reported_as_its_status() {
        let transport = RecordedTransport::new(vec![HttpResponse::new(503, "<html>nope</html>")]);
        let error = self_identity_over(&transport, "devi-secret-key")
            .await
            .expect_err("an outage page");

        assert!(matches!(error, DevinError::Status(503)));
    }
}
