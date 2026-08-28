//! The GitHub side of the OAuth code flow.
//!
//! Everything that leaves the Worker for `github.com` goes through
//! [`GithubOauth`]. The production implementation is [`ZenwaveGithub`]; tests
//! substitute their own so the callback handler can be exercised end to end
//! without the network.

use core::future::Future;

use serde::{Deserialize, Serialize};
use zenwave::{Client as _, Response, ResponseExt as _};

/// GitHub's OAuth token endpoint.
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

/// GitHub's authenticated-user endpoint.
const USER_URL: &str = "https://api.github.com/user";

/// GitHub rejects API requests without a `User-Agent`.
const USER_AGENT: &str = "flyco-control-plane";

/// OAuth scopes flyco needs: session VMs clone and push the user's repos.
pub const SCOPE: &str = "repo";

/// A GitHub account, as flyco stores it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GithubUser {
    /// GitHub's immutable numeric account id — the join key, because logins
    /// are renameable.
    pub id: i64,
    /// The account's current login.
    pub login: String,
}

/// A user-scoped GitHub access token.
#[derive(Clone, Deserialize)]
pub struct GithubToken {
    /// The bearer token itself.
    pub access_token: String,
}

impl core::fmt::Debug for GithubToken {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GithubToken").finish_non_exhaustive()
    }
}

/// What GitHub returns when a code exchange fails: HTTP 200 with an error
/// document instead of a token.
#[derive(Debug, Clone, Deserialize)]
struct GithubOauthError {
    error: String,
    error_description: Option<String>,
}

/// Either half of GitHub's token-endpoint response.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum TokenResponse {
    Token(GithubToken),
    Failure(GithubOauthError),
}

/// Request body of the token exchange.
#[derive(Debug, Serialize)]
struct ExchangeRequest<'a> {
    client_id: &'a str,
    client_secret: &'a str,
    code: &'a str,
    redirect_uri: &'a str,
}

/// Why a call to GitHub did not produce what the control plane needed.
#[derive(Debug, thiserror::Error)]
pub enum GithubError {
    /// The request never completed, or the response was not the expected JSON.
    #[error("GitHub request failed: {0}")]
    Transport(String),
    /// GitHub answered the code exchange with an OAuth error document.
    #[error("GitHub rejected the authorization code: {code} ({description})")]
    Rejected {
        /// GitHub's machine-readable error code.
        code: String,
        /// GitHub's human-readable explanation.
        description: String,
    },
    /// GitHub answered with a non-success status.
    #[error("GitHub responded with HTTP {0}")]
    Status(u16),
}

/// The two GitHub calls the OAuth callback makes.
///
/// Kept behind a trait because the happy path is otherwise untestable: the
/// callback handler cannot be exercised without standing in for `github.com`.
pub trait GithubOauth: Send + Sync + Clone + 'static {
    /// Exchanges an authorization code for a user access token.
    ///
    /// # Errors
    ///
    /// Returns [`GithubError`] if the exchange fails or GitHub rejects the
    /// code.
    fn exchange_code(
        &self,
        client_id: &str,
        client_secret: &str,
        code: &str,
        redirect_uri: &str,
    ) -> impl Future<Output = Result<GithubToken, GithubError>> + Send;

    /// Reads the account a token belongs to.
    ///
    /// # Errors
    ///
    /// Returns [`GithubError`] if the request fails or the token is invalid.
    fn current_user(
        &self,
        token: &GithubToken,
    ) -> impl Future<Output = Result<GithubUser, GithubError>> + Send;
}

/// The production [`GithubOauth`], speaking HTTP through zenwave — which is
/// Fetch-backed on the Worker and hyper-backed natively.
#[derive(Debug, Clone, Copy, Default)]
pub struct ZenwaveGithub;

impl ZenwaveGithub {
    /// Creates the client.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

fn transport(error: impl core::fmt::Display) -> GithubError {
    GithubError::Transport(error.to_string())
}

/// Reads a JSON body, but only after the status line says the call worked —
/// otherwise a GitHub outage page would surface as a deserialization error.
async fn json_body<T: serde::de::DeserializeOwned>(response: Response) -> Result<T, GithubError> {
    let status = response.status();
    if !status.is_success() {
        return Err(GithubError::Status(status.as_u16()));
    }
    response.into_json::<T>().await.map_err(transport)
}

impl GithubOauth for ZenwaveGithub {
    async fn exchange_code(
        &self,
        client_id: &str,
        client_secret: &str,
        code: &str,
        redirect_uri: &str,
    ) -> Result<GithubToken, GithubError> {
        let mut client = zenwave::client();
        let response = client
            .post(ACCESS_TOKEN_URL)
            .map_err(transport)?
            .header("Accept", "application/json")
            .map_err(transport)?
            .header("User-Agent", USER_AGENT)
            .map_err(transport)?
            .json_body(&ExchangeRequest {
                client_id,
                client_secret,
                code,
                redirect_uri,
            })
            .map_err(transport)?
            .await
            .map_err(transport)?;

        match json_body::<TokenResponse>(response).await? {
            TokenResponse::Token(token) => Ok(token),
            TokenResponse::Failure(failure) => Err(GithubError::Rejected {
                description: failure
                    .error_description
                    .unwrap_or_else(|| "no description".to_owned()),
                code: failure.error,
            }),
        }
    }

    async fn current_user(&self, token: &GithubToken) -> Result<GithubUser, GithubError> {
        let mut client = zenwave::client();
        let response = client
            .get(USER_URL)
            .map_err(transport)?
            .header("Accept", "application/vnd.github+json")
            .map_err(transport)?
            .header("User-Agent", USER_AGENT)
            .map_err(transport)?
            .bearer_auth(token.access_token.clone())
            .await
            .map_err(transport)?;

        json_body::<GithubUser>(response).await
    }
}
