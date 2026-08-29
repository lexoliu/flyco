//! The GitHub side of the OAuth code flow.
//!
//! Everything that leaves the Worker for `github.com` goes through
//! [`GithubOauth`]. The production implementation is [`ZenwaveGithub`]; tests
//! substitute their own so the callback handler can be exercised end to end
//! without the network.

use core::future::Future;

use flyco_core::RepoSummary;

use serde::{Deserialize, Serialize};
use zenwave::{Client as _, Response, ResponseExt as _};

/// GitHub's OAuth token endpoint.
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

/// GitHub's authenticated-user endpoint.
/// The caller's own repositories, most recently pushed first.
///
/// One page of 100 is what the picker reads. A user with more repositories
/// than that narrows the list by typing rather than by paging: the filter is
/// applied to this page, which is documented on the route.
const REPOS_URL: &str = "https://api.github.com/user/repos?sort=pushed&per_page=100";

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

    /// Lists the repositories a token can see, most recently pushed first.
    ///
    /// # Errors
    ///
    /// Returns [`GithubError`] if the request fails or the token is invalid.
    fn list_repos(
        &self,
        token: &GithubToken,
    ) -> impl Future<Output = Result<Vec<RepoSummary>, GithubError>> + Send;
}

/// One repository as GitHub's REST API reports it.
///
/// Only the fields the picker needs are named; GitHub sends many more and
/// serde discards them.
#[derive(Debug, Deserialize)]
struct GithubRepo {
    full_name: String,
    private: bool,
    default_branch: String,
    description: Option<String>,
    pushed_at: Option<String>,
}

impl GithubRepo {
    /// Converts to the picker's shape, dropping anything flyco cannot name.
    ///
    /// A repository whose `full_name` is not `owner/name` is skipped rather
    /// than guessed at: the slug is what `POST /v1/sessions` takes, and an
    /// unparseable one would fail at session creation instead of here.
    fn into_summary(self) -> Option<RepoSummary> {
        Some(RepoSummary {
            slug: self.full_name.parse().ok()?,
            private: self.private,
            default_branch: self.default_branch,
            description: self.description,
            pushed_at_unix: self.pushed_at.as_deref().and_then(parse_rfc3339_seconds),
        })
    }
}

/// Reads GitHub's RFC 3339 timestamps into Unix seconds.
///
/// Hand-rolled rather than pulling a date library into the Worker for one
/// field: the format GitHub emits is fixed-width UTC (`2026-08-29T05:16:31Z`),
/// so anything else is simply not a timestamp flyco can order by.
fn parse_rfc3339_seconds(value: &str) -> Option<u64> {
    let bytes = value.as_bytes();
    if bytes.len() != 20 || bytes[19] != b'Z' {
        return None;
    }
    let num = |range: core::ops::Range<usize>| value.get(range)?.parse::<u64>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // Days from the civil epoch, after Howard Hinnant's algorithm.
    let year_adjusted = if month <= 2 { year - 1 } else { year };
    let era = year_adjusted / 400;
    let year_of_era = year_adjusted - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;

    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// The GitHub client the router actually carries.
///
/// Handlers name this concrete type rather than a `G: GithubOauth`
/// parameter, and that is a deliberate constraint rather than a missing
/// abstraction: `#[skyzen::openapi]` emits module-level items naming every
/// argument type, so a generic handler cannot be annotated, and its
/// operation id would carry the substituted type — `list_repos<ZenwaveGithub>`
/// — which is not a name a generated client can be written against. One
/// concrete type keeps every operation id stable.
///
/// Dispatch is an enum rather than a trait object because [`GithubOauth`]
/// returns `impl Future`, which is not object-safe.
#[derive(Debug, Clone)]
pub enum GithubClient {
    /// Talks to `api.github.com`.
    Live(ZenwaveGithub),
    /// Answers from fixtures, for tests.
    #[cfg(test)]
    Fake(crate::testing::TestGithub),
}

impl Default for GithubClient {
    fn default() -> Self {
        Self::Live(ZenwaveGithub::new())
    }
}

/// Forwards to whichever client this is.
///
/// Written out rather than macro-generated: three methods is less code than
/// the macro that would write them.
impl GithubOauth for GithubClient {
    async fn exchange_code(
        &self,
        client_id: &str,
        client_secret: &str,
        code: &str,
        redirect_uri: &str,
    ) -> Result<GithubToken, GithubError> {
        match self {
            Self::Live(client) => {
                client
                    .exchange_code(client_id, client_secret, code, redirect_uri)
                    .await
            }
            #[cfg(test)]
            Self::Fake(client) => {
                client
                    .exchange_code(client_id, client_secret, code, redirect_uri)
                    .await
            }
        }
    }

    async fn current_user(&self, token: &GithubToken) -> Result<GithubUser, GithubError> {
        match self {
            Self::Live(client) => client.current_user(token).await,
            #[cfg(test)]
            Self::Fake(client) => client.current_user(token).await,
        }
    }

    async fn list_repos(&self, token: &GithubToken) -> Result<Vec<RepoSummary>, GithubError> {
        match self {
            Self::Live(client) => client.list_repos(token).await,
            #[cfg(test)]
            Self::Fake(client) => client.list_repos(token).await,
        }
    }
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

    async fn list_repos(&self, token: &GithubToken) -> Result<Vec<RepoSummary>, GithubError> {
        let mut client = zenwave::client();
        let response = client
            .get(REPOS_URL)
            .map_err(transport)?
            .header("Accept", "application/vnd.github+json")
            .map_err(transport)?
            .header("User-Agent", USER_AGENT)
            .map_err(transport)?
            .bearer_auth(token.access_token.clone())
            .await
            .map_err(transport)?;

        let repos = json_body::<Vec<GithubRepo>>(response).await?;
        Ok(repos
            .into_iter()
            .filter_map(GithubRepo::into_summary)
            .collect())
    }
}
