//! The GitHub side of the OAuth code flow.
//!
//! Everything that leaves the Worker for `github.com` goes through
//! [`GithubOauth`]. Native builds use [`ZenwaveGithub`], while Cloudflare
//! Workers use [`WorkerGithub`]; tests substitute their own so the callback
//! handler can be exercised end to end without the network.

use core::future::Future;

use flyco_core::{BranchName, RepoSlug, RepoSummary};

use serde::{Deserialize, Serialize};
#[cfg(target_arch = "wasm32")]
use skyzen_cloudflare::worker::send::IntoSendFuture as _;
#[cfg(not(target_arch = "wasm32"))]
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

/// How many branches one page of the branch picker holds.
///
/// GitHub's own maximum, so a repository with fewer than a hundred branches
/// — which is nearly all of them — is one request and one page.
const BRANCHES_PER_PAGE: u32 = 100;

/// Header GitHub reports an OAuth token's granted scopes in.
const SCOPES_HEADER: &str = "x-oauth-scopes";

/// OAuth scopes flyco needs: session VMs clone and push the user's repos.
pub const SCOPE: &str = "repo";

/// OAuth scopes a Codespaces link needs.
///
/// `repo` creates the private environment repository and writes its
/// devcontainer, `codespace` creates and drives the codespaces on it, and
/// `read:packages` is what lets GitHub pull the private session image into
/// them — a codespace is created by the account's token before its own
/// `GITHUB_TOKEN` exists, so the image grant has to ride on this one.
pub const CODESPACES_SCOPE: &str = "repo codespace read:packages";

/// The one scope of [`CODESPACES_SCOPE`] nothing else in flyco ever asks
/// for — its presence is what distinguishes a link token from the sign-in
/// token, so it is checked by name.
pub const CODESPACE_SCOPE: &str = "codespace";

/// The single scope a session's machine cannot work without.
///
/// `repo` is what lets a clone reach a *private* repository and what lets a
/// push land, and it is the whole of what [`SCOPE`] asks for — so a stored
/// token that does not carry it is one flyco cannot open a session with, no
/// matter how recently it was minted.
pub const REPO_SCOPE: &str = "repo";

/// A GitHub account, as flyco stores it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GithubUser {
    /// GitHub's immutable numeric account id — the join key, because logins
    /// are renameable.
    pub id: i64,
    /// The account's current login.
    pub login: String,
    /// The account's display name, when it has one.
    ///
    /// Read rather than stored: it is used for one thing — the `user.name`
    /// a session's commits are authored under — and a copy in D1 would be
    /// the stale one the day somebody renames themselves.
    #[serde(default)]
    pub name: Option<String>,
    /// The billing plan the account is on, when GitHub reports one.
    ///
    /// Read rather than stored, for the same reason `name` is: the
    /// Codespaces link translates it into the included core-hours the
    /// account's provision catalog is priced from, at link time.
    #[serde(default)]
    pub plan: Option<GithubPlan>,
}

/// An account's billing plan, as `GET /user` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GithubPlan {
    /// GitHub's plan name — `free`, `pro`, a team tier.
    pub name: String,
}

impl GithubUser {
    /// Who commits from this account are authored as.
    ///
    /// The display name when GitHub has one and the login otherwise, which
    /// is exactly what GitHub itself falls back to.
    #[must_use]
    pub fn commit_name(&self) -> &str {
        self.name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(&self.login)
    }

    /// The address commits from this account are authored under.
    ///
    /// GitHub's own `id+login@users.noreply.github.com` form: it attributes
    /// the commit to the account without publishing a private address flyco
    /// has no business putting in a public history.
    #[must_use]
    pub fn commit_email(&self) -> String {
        format!("{}+{}@users.noreply.github.com", self.id, self.login)
    }
}

/// A GitHub account together with what its token is allowed to do.
///
/// The two arrive in one response — `GET /user` answers with the account and
/// reports the token's scopes in a header — and flyco needs both at exactly
/// the same moments, so they are not two calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubIdentity {
    /// The account the token belongs to.
    pub user: GithubUser,
    /// Scopes GitHub says the token was granted.
    ///
    /// `None` when GitHub sent no [`SCOPES_HEADER`] at all, which means the
    /// credential is not an OAuth token and flyco cannot establish what it
    /// may do. Treated as insufficient rather than as unlimited: guessing
    /// in the permissive direction is how a session gets provisioned and
    /// then fails its clone five minutes later.
    pub scopes: Option<Vec<String>>,
}

impl GithubIdentity {
    /// Whether GitHub reported this token carrying `scope`.
    ///
    /// Scope names are not case-sensitive, and `None` scopes — a credential
    /// that is not an OAuth token — answer false rather than being guessed
    /// permissive.
    #[must_use]
    pub fn grants_scope(&self, scope: &str) -> bool {
        self.scopes.as_ref().is_some_and(|scopes| {
            scopes
                .iter()
                .any(|granted| granted.trim().eq_ignore_ascii_case(scope))
        })
    }

    /// Whether this token can read and push the user's private
    /// repositories.
    #[must_use]
    pub fn grants_repo_scope(&self) -> bool {
        self.grants_scope(REPO_SCOPE)
    }
}

/// Splits GitHub's comma-separated `X-OAuth-Scopes` header.
fn parse_scopes(header: &str) -> Vec<String> {
    header
        .split(',')
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// One page of a repository's branches, as GitHub serves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchListing {
    /// The branch names on this page, in GitHub's own order.
    pub names: Vec<BranchName>,
    /// Whether a further page exists.
    ///
    /// Derived from the page being full rather than from GitHub's `Link`
    /// header: a full last page costs one extra empty request, and parsing
    /// `Link` costs a parser for a header format used exactly here.
    pub has_more: bool,
}

/// The URL of a repository's own record.
fn repo_url(slug: &RepoSlug) -> String {
    format!("https://api.github.com/repos/{slug}")
}

/// The URL of one page of a repository's branches.
fn branches_url(slug: &RepoSlug, page: u32) -> String {
    format!("https://api.github.com/repos/{slug}/branches?per_page={BRANCHES_PER_PAGE}&page={page}")
}

/// A user-scoped GitHub access token.
#[derive(Clone)]
pub struct GithubToken {
    /// The bearer token itself.
    pub access_token: String,
}

impl core::fmt::Debug for GithubToken {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GithubToken").finish_non_exhaustive()
    }
}

/// What the token endpoint hands back for one authorization: the usable
/// token and — only when the OAuth app expires user tokens — the credential
/// that renews it and the moment it stops working.
///
/// The pair is `Some` together and `None` together: GitHub issues a
/// `refresh_token` exactly when it issues an `expires_in`, so
/// `expires_at_unix` being set is what tells a stored grant it can and must
/// be renewed rather than used until revoked.
#[derive(Clone)]
pub struct GithubGrant {
    /// The bearer credential API calls run under.
    pub token: GithubToken,
    /// Redeemed for the next grant once this one nears its end.
    ///
    /// GitHub rotates it on every redemption: whichever grant stored it
    /// last is the only one GitHub will renew.
    pub refresh_token: Option<String>,
    /// When [`token`](Self::token) stops working, seconds since the Unix
    /// epoch.
    pub expires_at_unix: Option<u64>,
}

impl core::fmt::Debug for GithubGrant {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GithubGrant").finish_non_exhaustive()
    }
}

/// The token endpoint's success document, as GitHub writes it.
///
/// `expires_in` is seconds *from the answer* — a relative duration only the
/// receiving side can pin to a wall clock, so the wire name is kept here
/// and [`GrantBody::into_grant`] is the one place it becomes absolute.
#[derive(Debug, Clone, Deserialize)]
struct GrantBody {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

impl GrantBody {
    /// Reads the document into a grant, pinning the relative lifetime to
    /// the moment it was received.
    fn into_grant(self) -> GithubGrant {
        GithubGrant {
            token: GithubToken {
                access_token: self.access_token,
            },
            refresh_token: self.refresh_token,
            expires_at_unix: self
                .expires_in
                .map(|seconds| crate::clock::now_unix().saturating_add(seconds)),
        }
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
    Token(GrantBody),
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

/// Request body of a refresh-token redemption.
///
/// Same endpoint, same client pair as the exchange; `grant_type` is what
/// turns it from "redeem this code" into "renew this grant".
#[derive(Debug, Serialize)]
struct RefreshRequest<'a> {
    client_id: &'a str,
    client_secret: &'a str,
    grant_type: &'static str,
    refresh_token: &'a str,
}

/// Which of flyco's calls to GitHub a failure came out of.
///
/// Carried by every failure that is a *call* going wrong, because "GitHub
/// said 401" is only actionable once the reader knows whether the 401 was
/// the sign-in exchanging a code or a repository picker reading a listing.
/// The [`Display`](core::fmt::Display) form is prose, since it is read in a
/// sentence: "GitHub answered HTTP 401 to the account profile request".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubCall {
    /// `POST /login/oauth/access_token` — the sign-in's code exchange.
    TokenExchange,
    /// `POST /login/oauth/access_token` — renewing a grant before use.
    TokenRefresh,
    /// `GET /user` — who a token belongs to, and what it may do.
    UserProfile,
    /// `GET /user/repos` — the picker's list of the caller's repositories.
    Repositories,
    /// `GET /repos/{slug}` — one repository, which is where a default
    /// branch comes from.
    Repository,
    /// `GET /repos/{slug}/branches` — one page of a repository's branches.
    Branches,
}

impl core::fmt::Display for GithubCall {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::TokenExchange => "authorization code exchange",
            Self::TokenRefresh => "token refresh",
            Self::UserProfile => "account profile",
            Self::Repositories => "repository list",
            Self::Repository => "repository lookup",
            Self::Branches => "branch list",
        })
    }
}

/// Why a call to GitHub did not produce what the control plane needed.
///
/// Three failures rather than one, because they are three different things
/// to tell the user: a rejected authorization code is theirs to fix, a
/// status GitHub answered with is a fact about GitHub, and a transport
/// failure is the only one whose text is flyco's own internals. See
/// [`From<GithubError> for ApiError`](crate::error::ApiError) for the
/// problem documents they become.
#[derive(Debug, thiserror::Error)]
pub enum GithubError {
    /// The request never completed, or the response was not the expected JSON.
    #[error("the {call} request to GitHub failed: {message}")]
    Transport {
        /// The call that never got an answer flyco could read.
        call: GithubCall,
        /// What the HTTP client or the deserializer said.
        message: String,
    },
    /// GitHub answered the code exchange with an OAuth error document.
    #[error("GitHub rejected the authorization code: {code} ({description})")]
    Rejected {
        /// GitHub's machine-readable error code.
        code: String,
        /// GitHub's human-readable explanation.
        description: String,
    },
    /// GitHub answered with a non-success status.
    #[error("GitHub answered HTTP {status} to the {call} request: {reason}")]
    Status {
        /// The call GitHub refused.
        call: GithubCall,
        /// The status it refused with.
        status: u16,
        /// What GitHub's error document said, so that a stored token GitHub
        /// no longer accepts ("Bad credentials") can be told apart from a
        /// scope or SSO refusal in the retained logs.
        reason: String,
    },
}

/// The two GitHub calls the OAuth callback makes.
///
/// Kept behind a trait because the happy path is otherwise untestable: the
/// callback handler cannot be exercised without standing in for `github.com`.
pub trait GithubOauth: Send + Sync + Clone + 'static {
    /// Exchanges an authorization code for a user grant.
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
    ) -> impl Future<Output = Result<GithubGrant, GithubError>> + Send;

    /// Exchanges a grant's refresh token for its next token pair.
    ///
    /// # Errors
    ///
    /// Returns [`GithubError`] if the call fails. A
    /// [`GithubError::Rejected`] means the grant is dead — GitHub will not
    /// renew it — and the user has to authorize again; transports and
    /// statuses are worth retrying instead.
    fn refresh(
        &self,
        client_id: &str,
        client_secret: &str,
        refresh_token: &str,
    ) -> impl Future<Output = Result<GithubGrant, GithubError>> + Send;

    /// Reads the account a token belongs to, and what the token may do.
    ///
    /// # Errors
    ///
    /// Returns [`GithubError`] if the request fails or the token is invalid.
    fn current_user(
        &self,
        token: &GithubToken,
    ) -> impl Future<Output = Result<GithubIdentity, GithubError>> + Send;

    /// Lists the repositories a token can see, most recently pushed first.
    ///
    /// # Errors
    ///
    /// Returns [`GithubError`] if the request fails or the token is invalid.
    fn list_repos(
        &self,
        token: &GithubToken,
    ) -> impl Future<Output = Result<Vec<RepoSummary>, GithubError>> + Send;

    /// Reads one repository, which is where its default branch comes from.
    ///
    /// # Errors
    ///
    /// Returns [`GithubError`] if the request fails, the token is invalid,
    /// or the repository is not one this token can see.
    fn get_repo(
        &self,
        token: &GithubToken,
        slug: &RepoSlug,
    ) -> impl Future<Output = Result<RepoSummary, GithubError>> + Send;

    /// Lists one page of a repository's branches.
    ///
    /// Pages count from one, as GitHub's own do.
    ///
    /// # Errors
    ///
    /// Returns [`GithubError`] if the request fails, the token is invalid,
    /// or the repository is not one this token can see.
    fn list_branches(
        &self,
        token: &GithubToken,
        slug: &RepoSlug,
        page: u32,
    ) -> impl Future<Output = Result<BranchListing, GithubError>> + Send;
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

/// One branch as GitHub's REST API reports it.
#[derive(Debug, Deserialize)]
struct GithubBranch {
    name: String,
}

/// Turns a page of GitHub branches into the names flyco can act on.
///
/// A name flyco's own [`BranchName`] refuses is skipped rather than passed
/// through: it could not be checked out, so offering it in a picker would be
/// offering a session that fails.
fn branch_listing(branches: Vec<GithubBranch>) -> BranchListing {
    let has_more = u32::try_from(branches.len()).is_ok_and(|len| len >= BRANCHES_PER_PAGE);
    BranchListing {
        names: branches
            .into_iter()
            .filter_map(|branch| branch.name.parse().ok())
            .collect(),
        has_more,
    }
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
            default_branch: self.default_branch.parse().ok()?,
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
    #[cfg(not(target_arch = "wasm32"))]
    Live(ZenwaveGithub),
    /// Talks to `api.github.com` through `WorkerGlobalScope.fetch`.
    #[cfg(target_arch = "wasm32")]
    Live(WorkerGithub),
    /// Answers from fixtures, for tests.
    #[cfg(test)]
    Fake(crate::testing::TestGithub),
}

impl Default for GithubClient {
    fn default() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        {
            Self::Live(ZenwaveGithub::new())
        }
        #[cfg(target_arch = "wasm32")]
        {
            Self::Live(WorkerGithub::new())
        }
    }
}

/// Forwards to whichever client this is.
///
/// Written out rather than macro-generated: five methods is less code than
/// the macro that would write them.
impl GithubOauth for GithubClient {
    async fn exchange_code(
        &self,
        client_id: &str,
        client_secret: &str,
        code: &str,
        redirect_uri: &str,
    ) -> Result<GithubGrant, GithubError> {
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

    async fn refresh(
        &self,
        client_id: &str,
        client_secret: &str,
        refresh_token: &str,
    ) -> Result<GithubGrant, GithubError> {
        match self {
            Self::Live(client) => {
                client
                    .refresh(client_id, client_secret, refresh_token)
                    .await
            }
            #[cfg(test)]
            Self::Fake(client) => {
                client
                    .refresh(client_id, client_secret, refresh_token)
                    .await
            }
        }
    }

    async fn current_user(&self, token: &GithubToken) -> Result<GithubIdentity, GithubError> {
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

    async fn get_repo(
        &self,
        token: &GithubToken,
        slug: &RepoSlug,
    ) -> Result<RepoSummary, GithubError> {
        match self {
            Self::Live(client) => client.get_repo(token, slug).await,
            #[cfg(test)]
            Self::Fake(client) => client.get_repo(token, slug).await,
        }
    }

    async fn list_branches(
        &self,
        token: &GithubToken,
        slug: &RepoSlug,
        page: u32,
    ) -> Result<BranchListing, GithubError> {
        match self {
            Self::Live(client) => client.list_branches(token, slug, page).await,
            #[cfg(test)]
            Self::Fake(client) => client.list_branches(token, slug, page).await,
        }
    }
}

/// The native production [`GithubOauth`], speaking HTTP through zenwave.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, Default)]
pub struct ZenwaveGithub;

#[cfg(not(target_arch = "wasm32"))]
impl ZenwaveGithub {
    /// Creates the client.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

/// The `map_err` a call site hands its transport failures to.
///
/// Returns the closure rather than the error so every `?` on the way to one
/// GitHub endpoint names its call once: `.map_err(transport(call))`.
fn transport<E: core::fmt::Display>(call: GithubCall) -> impl Fn(E) -> GithubError {
    move |error| GithubError::Transport {
        call,
        message: error.to_string(),
    }
}

/// The longest a refusal reason taken from a non-JSON body may be.
const REASON_MAX_CHARS: usize = 200;

/// GitHub's error document: every refusal carries a `message`.
#[derive(Deserialize)]
struct RefusalBody {
    message: String,
}

/// The sentence a refused call is reported with: GitHub's own `message`
/// when the body is its error document, else the body's first line, else a
/// note that it sent none.
fn refusal_reason(body: &str) -> String {
    if let Ok(refusal) = serde_json::from_str::<RefusalBody>(body) {
        return refusal.message;
    }
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map_or_else(
            || "no error document".to_owned(),
            |line| line.chars().take(REASON_MAX_CHARS).collect(),
        )
}

/// Reads a native JSON body, but only after the status line says the call
/// worked — otherwise a GitHub outage page would surface as a deserialization
/// error.
#[cfg(not(target_arch = "wasm32"))]
async fn json_body<T: serde::de::DeserializeOwned>(
    call: GithubCall,
    response: Response,
) -> Result<T, GithubError> {
    let status = response.status();
    if !status.is_success() {
        let body = response.into_string().await.map_err(transport(call))?;
        return Err(GithubError::Status {
            call,
            status: status.as_u16(),
            reason: refusal_reason(&body),
        });
    }
    response.into_json::<T>().await.map_err(transport(call))
}

/// One authenticated `GET` against `api.github.com`.
///
/// Four call sites want the same three headers and the same bearer, and a
/// fifth copy of them is a fifth place to forget the `User-Agent` GitHub
/// rejects a request without.
#[cfg(not(target_arch = "wasm32"))]
async fn authorized_get(
    call: GithubCall,
    url: &str,
    token: &GithubToken,
) -> Result<Response, GithubError> {
    let mut client = zenwave::client();
    client
        .get(url)
        .map_err(transport(call))?
        .header("Accept", "application/vnd.github+json")
        .map_err(transport(call))?
        .header("User-Agent", USER_AGENT)
        .map_err(transport(call))?
        .bearer_auth(token.access_token.clone())
        .await
        .map_err(transport(call))
}

#[cfg(not(target_arch = "wasm32"))]
impl GithubOauth for ZenwaveGithub {
    async fn exchange_code(
        &self,
        client_id: &str,
        client_secret: &str,
        code: &str,
        redirect_uri: &str,
    ) -> Result<GithubGrant, GithubError> {
        let call = GithubCall::TokenExchange;
        let mut client = zenwave::client();
        let response = client
            .post(ACCESS_TOKEN_URL)
            .map_err(transport(call))?
            .header("Accept", "application/json")
            .map_err(transport(call))?
            .header("User-Agent", USER_AGENT)
            .map_err(transport(call))?
            .json_body(&ExchangeRequest {
                client_id,
                client_secret,
                code,
                redirect_uri,
            })
            .map_err(transport(call))?
            .await
            .map_err(transport(call))?;

        token_response(json_body::<TokenResponse>(call, response).await?)
    }

    async fn refresh(
        &self,
        client_id: &str,
        client_secret: &str,
        refresh_token: &str,
    ) -> Result<GithubGrant, GithubError> {
        let call = GithubCall::TokenRefresh;
        let mut client = zenwave::client();
        let response = client
            .post(ACCESS_TOKEN_URL)
            .map_err(transport(call))?
            .header("Accept", "application/json")
            .map_err(transport(call))?
            .header("User-Agent", USER_AGENT)
            .map_err(transport(call))?
            .json_body(&RefreshRequest {
                client_id,
                client_secret,
                grant_type: "refresh_token",
                refresh_token,
            })
            .map_err(transport(call))?
            .await
            .map_err(transport(call))?;

        token_response(json_body::<TokenResponse>(call, response).await?)
    }

    async fn current_user(&self, token: &GithubToken) -> Result<GithubIdentity, GithubError> {
        let call = GithubCall::UserProfile;
        let response = authorized_get(call, USER_URL, token).await?;
        // Read before the body is consumed: what the token may do is in a
        // header, and `into_json` takes the whole response.
        let scopes = response
            .headers()
            .get(SCOPES_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(parse_scopes);

        Ok(GithubIdentity {
            user: json_body::<GithubUser>(call, response).await?,
            scopes,
        })
    }

    async fn list_repos(&self, token: &GithubToken) -> Result<Vec<RepoSummary>, GithubError> {
        let call = GithubCall::Repositories;
        let repos =
            json_body::<Vec<GithubRepo>>(call, authorized_get(call, REPOS_URL, token).await?)
                .await?;
        Ok(repos
            .into_iter()
            .filter_map(GithubRepo::into_summary)
            .collect())
    }

    async fn get_repo(
        &self,
        token: &GithubToken,
        slug: &RepoSlug,
    ) -> Result<RepoSummary, GithubError> {
        let call = GithubCall::Repository;
        let repo =
            json_body::<GithubRepo>(call, authorized_get(call, &repo_url(slug), token).await?)
                .await?;
        repo.into_summary().ok_or_else(|| unusable_repo(slug))
    }

    async fn list_branches(
        &self,
        token: &GithubToken,
        slug: &RepoSlug,
        page: u32,
    ) -> Result<BranchListing, GithubError> {
        let call = GithubCall::Branches;
        let branches = json_body::<Vec<GithubBranch>>(
            call,
            authorized_get(call, &branches_url(slug, page), token).await?,
        )
        .await?;
        Ok(branch_listing(branches))
    }
}

/// A repository GitHub described in terms flyco cannot act on.
///
/// A transport failure rather than a status: the call succeeded, and what
/// went wrong is that the document does not answer the question — which is
/// the same "flyco could not read GitHub's answer" the deserializer
/// produces, and equally none of the caller's business.
fn unusable_repo(slug: &RepoSlug) -> GithubError {
    GithubError::Transport {
        call: GithubCall::Repository,
        message: format!("GitHub described {slug} unusably"),
    }
}

fn token_response(response: TokenResponse) -> Result<GithubGrant, GithubError> {
    match response {
        TokenResponse::Token(body) => Ok(body.into_grant()),
        TokenResponse::Failure(failure) => Err(GithubError::Rejected {
            description: failure
                .error_description
                .unwrap_or_else(|| "no description".to_owned()),
            code: failure.error,
        }),
    }
}

/// The Cloudflare production [`GithubOauth`], speaking HTTP through
/// `WorkerGlobalScope.fetch`.
#[cfg(target_arch = "wasm32")]
#[derive(Debug, Clone, Copy, Default)]
pub struct WorkerGithub;

#[cfg(target_arch = "wasm32")]
impl WorkerGithub {
    /// Creates the client.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

/// Reads a Worker JSON body after validating the HTTP status.
#[cfg(target_arch = "wasm32")]
async fn worker_json_body<T: serde::de::DeserializeOwned>(
    call: GithubCall,
    mut response: skyzen_cloudflare::worker::Response,
) -> Result<T, GithubError> {
    let status = response.status_code();
    if !(200..300).contains(&status) {
        let body = response.text().into_send().await.map_err(transport(call))?;
        return Err(GithubError::Status {
            call,
            status,
            reason: refusal_reason(&body),
        });
    }
    response
        .json::<T>()
        .into_send()
        .await
        .map_err(transport(call))
}

#[cfg(target_arch = "wasm32")]
async fn worker_fetch(
    call: GithubCall,
    request: skyzen_cloudflare::worker::Request,
) -> Result<skyzen_cloudflare::worker::Response, GithubError> {
    skyzen_cloudflare::worker::Fetch::Request(request)
        .send()
        .into_send()
        .await
        .map_err(transport(call))
}

/// One authenticated `GET` against `api.github.com`, through the Worker's
/// own `fetch`.
///
/// The counterpart of the native [`authorized_get`], and here for the same
/// reason: four call sites, one set of headers.
#[cfg(target_arch = "wasm32")]
async fn worker_authorized_get(
    call: GithubCall,
    url: &str,
    token: &GithubToken,
) -> Result<skyzen_cloudflare::worker::Response, GithubError> {
    let authorization = format!("Bearer {}", token.access_token);
    let request = skyzen_cloudflare::bare_request(
        skyzen_cloudflare::worker::Method::Get,
        url,
        &[
            ("Accept", "application/vnd.github+json"),
            ("User-Agent", USER_AGENT),
            ("Authorization", authorization.as_str()),
        ],
        None,
    )
    .map_err(transport(call))?;

    worker_fetch(call, request).await
}

#[cfg(target_arch = "wasm32")]
impl GithubOauth for WorkerGithub {
    async fn exchange_code(
        &self,
        client_id: &str,
        client_secret: &str,
        code: &str,
        redirect_uri: &str,
    ) -> Result<GithubGrant, GithubError> {
        let call = GithubCall::TokenExchange;
        let request = skyzen_cloudflare::json_request(
            skyzen_cloudflare::worker::Method::Post,
            ACCESS_TOKEN_URL,
            &ExchangeRequest {
                client_id,
                client_secret,
                code,
                redirect_uri,
            },
            &[("Accept", "application/json"), ("User-Agent", USER_AGENT)],
        )
        .map_err(transport(call))?;
        let response = worker_fetch(call, request).await?;

        token_response(worker_json_body::<TokenResponse>(call, response).await?)
    }

    async fn refresh(
        &self,
        client_id: &str,
        client_secret: &str,
        refresh_token: &str,
    ) -> Result<GithubGrant, GithubError> {
        let call = GithubCall::TokenRefresh;
        let request = skyzen_cloudflare::json_request(
            skyzen_cloudflare::worker::Method::Post,
            ACCESS_TOKEN_URL,
            &RefreshRequest {
                client_id,
                client_secret,
                grant_type: "refresh_token",
                refresh_token,
            },
            &[("Accept", "application/json"), ("User-Agent", USER_AGENT)],
        )
        .map_err(transport(call))?;
        let response = worker_fetch(call, request).await?;

        token_response(worker_json_body::<TokenResponse>(call, response).await?)
    }

    async fn current_user(&self, token: &GithubToken) -> Result<GithubIdentity, GithubError> {
        let call = GithubCall::UserProfile;
        let response = worker_authorized_get(call, USER_URL, token).await?;
        // Read before the body is consumed, exactly as natively: what the
        // token may do is a header, and reading the JSON takes the response.
        let scopes = response
            .headers()
            .get(SCOPES_HEADER)
            .ok()
            .flatten()
            .map(|value| parse_scopes(&value));

        Ok(GithubIdentity {
            user: worker_json_body::<GithubUser>(call, response).await?,
            scopes,
        })
    }

    async fn list_repos(&self, token: &GithubToken) -> Result<Vec<RepoSummary>, GithubError> {
        let call = GithubCall::Repositories;
        let repos = worker_json_body::<Vec<GithubRepo>>(
            call,
            worker_authorized_get(call, REPOS_URL, token).await?,
        )
        .await?;
        Ok(repos
            .into_iter()
            .filter_map(GithubRepo::into_summary)
            .collect())
    }

    async fn get_repo(
        &self,
        token: &GithubToken,
        slug: &RepoSlug,
    ) -> Result<RepoSummary, GithubError> {
        let call = GithubCall::Repository;
        let repo = worker_json_body::<GithubRepo>(
            call,
            worker_authorized_get(call, &repo_url(slug), token).await?,
        )
        .await?;
        repo.into_summary().ok_or_else(|| unusable_repo(slug))
    }

    async fn list_branches(
        &self,
        token: &GithubToken,
        slug: &RepoSlug,
        page: u32,
    ) -> Result<BranchListing, GithubError> {
        let call = GithubCall::Branches;
        let branches = worker_json_body::<Vec<GithubBranch>>(
            call,
            worker_authorized_get(call, &branches_url(slug, page), token).await?,
        )
        .await?;
        Ok(branch_listing(branches))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BRANCHES_PER_PAGE, GithubBranch, GithubCall, GithubError, GithubIdentity, GithubOauthError,
        GithubUser, GrantBody, TokenResponse, branch_listing, branches_url, parse_scopes,
        refusal_reason, repo_url, token_response, transport, unusable_repo,
    };

    fn identity(scopes: Option<Vec<String>>) -> GithubIdentity {
        GithubIdentity {
            user: GithubUser {
                id: 4_242,
                login: "lexoliu".to_owned(),
                name: Some("Lexo Liu".to_owned()),
                plan: None,
            },
            scopes,
        }
    }

    #[test]
    fn a_commit_identity_prefers_the_display_name_and_the_noreply_address() {
        let user = identity(None).user;
        assert_eq!(user.commit_name(), "Lexo Liu");
        assert_eq!(user.commit_email(), "4242+lexoliu@users.noreply.github.com");
    }

    #[test]
    fn an_account_with_no_display_name_commits_under_its_login() {
        let mut user = identity(None).user;
        user.name = None;
        assert_eq!(user.commit_name(), "lexoliu");
        user.name = Some("   ".to_owned());
        assert_eq!(
            user.commit_name(),
            "lexoliu",
            "a blank display name is not a name"
        );
    }

    #[test]
    fn the_scopes_header_is_read_the_way_github_writes_it() {
        assert_eq!(
            parse_scopes("repo, read:org, "),
            vec!["repo".to_owned(), "read:org".to_owned()]
        );
        assert!(parse_scopes("").is_empty());
    }

    #[test]
    fn only_a_token_that_says_repo_can_open_a_session() {
        assert!(identity(Some(vec!["repo".to_owned()])).grants_repo_scope());
        assert!(
            identity(Some(vec!["read:org".to_owned(), "Repo".to_owned()])).grants_repo_scope(),
            "GitHub's scope names are not case-sensitive"
        );
        assert!(
            !identity(Some(vec!["public_repo".to_owned()])).grants_repo_scope(),
            "`public_repo` cannot reach a private repository, which is what a session needs"
        );
        assert!(!identity(Some(Vec::new())).grants_repo_scope());
        assert!(
            !identity(None).grants_repo_scope(),
            "a credential whose scopes GitHub did not report is one flyco cannot vouch for"
        );
    }

    #[test]
    fn a_full_page_of_branches_promises_another_one() {
        let full: Vec<GithubBranch> = (0..BRANCHES_PER_PAGE)
            .map(|index| GithubBranch {
                name: format!("feat/{index}"),
            })
            .collect();
        assert!(branch_listing(full).has_more);

        let short = vec![GithubBranch {
            name: "main".to_owned(),
        }];
        let listing = branch_listing(short);
        assert!(!listing.has_more);
        assert_eq!(listing.names.len(), 1);
    }

    #[test]
    fn a_branch_flyco_could_not_check_out_is_not_offered() {
        let listing = branch_listing(vec![
            GithubBranch {
                name: "main".to_owned(),
            },
            GithubBranch {
                name: "broken branch".to_owned(),
            },
        ]);
        assert_eq!(listing.names.len(), 1);
        assert_eq!(listing.names[0].as_str(), "main");
    }

    #[test]
    fn the_repository_urls_address_githubs_own_routes() {
        let slug: flyco_core::RepoSlug = "lexoliu/flyco".parse().expect("a valid slug");
        assert_eq!(
            repo_url(&slug),
            "https://api.github.com/repos/lexoliu/flyco"
        );
        assert_eq!(
            branches_url(&slug, 2),
            "https://api.github.com/repos/lexoliu/flyco/branches?per_page=100&page=2"
        );
    }

    #[test]
    fn a_token_response_yields_the_grant() {
        let grant = token_response(TokenResponse::Token(GrantBody {
            access_token: "github-test-token".to_owned(),
            refresh_token: None,
            expires_in: None,
        }))
        .expect("token response");

        assert_eq!(grant.token.access_token, "github-test-token");
        assert!(grant.refresh_token.is_none());
        assert!(grant.expires_at_unix.is_none());
    }

    #[test]
    fn an_expiring_grant_pins_its_lifetime_to_receipt() {
        let before = crate::clock::now_unix();
        let grant = token_response(TokenResponse::Token(GrantBody {
            access_token: "github-test-token".to_owned(),
            refresh_token: Some("github-test-refresh".to_owned()),
            expires_in: Some(28_800),
        }))
        .expect("token response");

        assert_eq!(grant.refresh_token.as_deref(), Some("github-test-refresh"));
        let expires_at = grant
            .expires_at_unix
            .expect("an expiring grant states its end");
        assert!(expires_at >= before + 28_800);
        assert!(expires_at <= crate::clock::now_unix() + 28_800);
    }

    #[test]
    fn an_oauth_error_response_keeps_githubs_reason() {
        let error = token_response(TokenResponse::Failure(GithubOauthError {
            error: "bad_verification_code".to_owned(),
            error_description: Some("The code passed is incorrect or expired.".to_owned()),
        }))
        .expect_err("rejected code");

        assert!(matches!(
            error,
            GithubError::Rejected { code, description }
                if code == "bad_verification_code"
                    && description == "The code passed is incorrect or expired."
        ));
    }

    #[test]
    fn every_call_names_itself_the_way_a_sentence_reads_it() {
        for (call, prose) in [
            (GithubCall::TokenExchange, "authorization code exchange"),
            (GithubCall::TokenRefresh, "token refresh"),
            (GithubCall::UserProfile, "account profile"),
            (GithubCall::Repositories, "repository list"),
            (GithubCall::Repository, "repository lookup"),
            (GithubCall::Branches, "branch list"),
        ] {
            assert_eq!(call.to_string(), prose);
        }
    }

    #[test]
    fn a_refused_status_says_which_call_it_refused() {
        let error = GithubError::Status {
            call: GithubCall::UserProfile,
            status: 401,
            reason: "Bad credentials".to_owned(),
        };

        assert_eq!(
            error.to_string(),
            "GitHub answered HTTP 401 to the account profile request: Bad credentials"
        );
    }

    #[test]
    fn a_refusal_reason_is_githubs_message_or_the_bodys_first_line() {
        assert_eq!(
            refusal_reason(
                r#"{"message":"Bad credentials","documentation_url":"https://docs.github.com/rest"}"#
            ),
            "Bad credentials"
        );
        assert_eq!(
            refusal_reason("\n<html>outage</html>\n"),
            "<html>outage</html>"
        );
        assert_eq!(refusal_reason(""), "no error document");
    }

    #[test]
    fn a_transport_failure_carries_the_call_it_belongs_to() {
        let error = transport(GithubCall::Branches)("connection reset");

        assert!(matches!(
            &error,
            GithubError::Transport { call, message }
                if *call == GithubCall::Branches && message == "connection reset"
        ));
        assert_eq!(
            error.to_string(),
            "the branch list request to GitHub failed: connection reset"
        );
    }

    #[test]
    fn a_repository_flyco_cannot_read_is_a_transport_failure_of_the_lookup() {
        let slug: flyco_core::RepoSlug = "lexoliu/flyco".parse().expect("a valid slug");

        assert!(matches!(
            unusable_repo(&slug),
            GithubError::Transport { call, message }
                if call == GithubCall::Repository
                    && message == "GitHub described lexoliu/flyco unusably"
        ));
    }
}
