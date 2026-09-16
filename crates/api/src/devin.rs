//! The Devin side of linking a Devin account.
//!
//! Devin has no third-party OAuth flow: the credential is the `devi…` key
//! `devin auth login` writes into `credentials.toml`, and the paste-a-key
//! route is the only way in. That does not make the account's label the
//! caller's to choose — `GET /v3/self` accepts the same key and names the
//! principal it opens, so linking asks Devin who the key belongs to rather
//! than trusting a string in the request. The call doubles as the key's
//! validation: a key Devin refuses is refused here, at link time, instead
//! of being stored and discovered by the first session that tries to run
//! on it.
//!
//! Everything that leaves the control plane for Devin goes through an
//! [`HttpTransport`](flyco_provider::HttpTransport), which is zenwave in
//! production and a table of recorded exchanges under test — the same seam
//! [`crate::anthropic`] pins its token exchanges on.

use core::future::Future;

use flyco_provider::http::{HttpRequest, HttpResponse, Method};
use flyco_provider::{HttpError, HttpTransport, LiveTransport};
use serde::Deserialize;

/// Where a key proves itself and names its principal.
const SELF_URL: &str = "https://api.devin.ai/v3/self";

/// The principal a `devi…` key authenticates as.
///
/// `/v3/self` answers one of four shapes, tagged by `principal_type`: the
/// key `devin auth login` issues is a `windsurf_session`, a PAT is a
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
    /// The `devi…` key `devin auth login` writes.
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

/// The one call the link route makes.
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
}

#[cfg(test)]
mod tests {
    use flyco_provider::http::HttpResponse;
    use flyco_provider::testing::RecordedTransport;

    use super::{DevinError, DevinSelf, self_identity_over, self_request};

    /// An identity as Devin returns one for a `devi…` key.
    const SELF_BODY: &str = include_str!("../fixtures/devin/self.json");

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
