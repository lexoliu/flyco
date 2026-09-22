//! The Anthropic side of the Claude Code OAuth flow.
//!
//! Flyco runs exactly the flow the Claude CLI runs, in the browser: the
//! control plane mints a PKCE verifier and a `state`, the user approves the
//! grant at `claude.ai`, Anthropic shows them a `CODE#STATE` string, and the
//! control plane redeems it at `console.anthropic.com` for an access token,
//! a refresh token, and a lifetime. There is no redirect back to flyco —
//! the redirect URI is Anthropic's own console page, which is what makes
//! the pasted code the transport.
//!
//! Everything that leaves the control plane for Anthropic goes through an
//! [`HttpTransport`](flyco_provider::HttpTransport), which is zenwave in
//! production and a table of recorded exchanges under test. That is the only
//! way to pin what actually goes on the wire — the exact URL, the exact JSON
//! body — and both grants below are the same endpoint, so there is one
//! request type and one method rather than two of each. The plan read
//! below goes out the same way, and to a different host: the grant is
//! redeemed at `console.anthropic.com` and the plan is read from
//! `api.anthropic.com`.

use core::future::Future;

use flyco_core::UsageWindow;
use flyco_provider::http::{HttpRequest, HttpResponse, Method};
use flyco_provider::{HttpError, HttpTransport, LiveTransport};
use serde::{Deserialize, Serialize};
use url::Url;

/// Where the browser approves the grant.
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";

/// Where an authorization code and a refresh token are both redeemed.
const TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";

/// Where a subscription's rolling plan windows are read.
///
/// The endpoint the Claude CLI reads its own `/usage` screen from, and it
/// answers for whoever the bearer token belongs to — there is no account
/// parameter, which is why the token has to be the account's own.
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

/// The beta gate an OAuth-authenticated call to `api.anthropic.com` needs.
const OAUTH_BETA: &str = "oauth-2025-04-20";

/// The API version every call to `api.anthropic.com` states.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Minutes in the two window lengths Anthropic's plan is measured in.
const FIVE_HOUR_MINUTES: u32 = 5 * 60;
const SEVEN_DAY_MINUTES: u32 = 7 * 24 * 60;

/// The redirect URI the grant is bound to.
///
/// Anthropic's own console page, which renders the `CODE#STATE` string for
/// the user to copy. Flyco never receives a redirect, which is why the flow
/// works from a Worker with no callback route of its own.
pub const REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";

/// Scopes a flyco-run session needs: the subscription's inference, the
/// account's profile, and the API-key grant the harness itself uses.
pub const SCOPE: &str = "org:create_api_key user:profile user:inference";

/// Separator Anthropic puts between the code and the state it echoes.
const CODE_STATE_SEPARATOR: char = '#';

/// The URL the browser opens to approve the grant.
///
/// # Panics
///
/// Panics if [`AUTHORIZE_URL`] is not an absolute URL, which would mean this
/// module's own constant was edited into something that is not one.
#[must_use]
pub fn authorize_url(client_id: &str, challenge: &str, state: &str) -> Url {
    let mut url = Url::parse(AUTHORIZE_URL).expect("the Claude authorize URL is absolute");
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    url
}

/// Splits what the user pasted into the code and the state beside it.
///
/// Anthropic shows `CODE#STATE`; a user who copied only the first half has
/// still supplied everything the exchange needs, because the control plane
/// holds the state it minted. The second half, when present, is checked
/// against that state rather than trusted.
#[must_use]
pub fn split_pasted_code(pasted: &str) -> (&str, Option<&str>) {
    let trimmed = pasted.trim();
    match trimmed.split_once(CODE_STATE_SEPARATOR) {
        Some((code, state)) => (code.trim(), Some(state.trim())),
        None => (trimmed, None),
    }
}

/// One request to Anthropic's token endpoint.
///
/// Both grants are the same endpoint with the same media type, so the tag
/// is the `grant_type` field itself and serde writes it.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "grant_type", rename_all = "snake_case")]
pub enum TokenRequest<'a> {
    /// Redeems the code the user pasted.
    AuthorizationCode {
        /// The code half of what Anthropic showed the user.
        code: &'a str,
        /// The `state` this control plane minted for the attempt.
        state: &'a str,
        /// The Claude Code OAuth client id this deployment presents.
        client_id: &'a str,
        /// Must equal the one the authorize URL carried.
        redirect_uri: &'a str,
        /// The PKCE verifier whose challenge was published.
        code_verifier: &'a str,
    },
    /// Exchanges a stored refresh token for a fresh pair.
    RefreshToken {
        /// The refresh token from a previous exchange.
        refresh_token: &'a str,
        /// The same client id the grant was issued to.
        client_id: &'a str,
    },
}

/// The account Anthropic says the grant belongs to.
///
/// Only the address is read: it is what a linked card is labelled with, so
/// two Claude accounts are told apart by the person's own name for them.
#[derive(Debug, Clone, Deserialize)]
pub struct Account {
    /// The account's email address, when Anthropic states one.
    pub email_address: Option<String>,
}

/// What Anthropic hands back for either grant.
#[derive(Clone, Deserialize)]
pub struct TokenSet {
    /// The bearer token the harness runs under.
    pub access_token: String,
    /// Redeemed for the next pair before this one expires.
    pub refresh_token: String,
    /// Lifetime of [`access_token`](Self::access_token), in seconds.
    pub expires_in: u64,
    /// The account the grant belongs to, when Anthropic names one.
    #[serde(default)]
    pub account: Option<Account>,
}

impl core::fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TokenSet")
            .field("expires_in", &self.expires_in)
            .finish_non_exhaustive()
    }
}

impl TokenSet {
    /// When the access token stops working, as a Unix timestamp.
    #[must_use]
    pub const fn expires_at_unix(&self, now: u64) -> u64 {
        now.saturating_add(self.expires_in)
    }

    /// The address to label the linked account with, if Anthropic named one.
    #[must_use]
    pub fn email_address(&self) -> Option<&str> {
        self.account
            .as_ref()
            .and_then(|account| account.email_address.as_deref())
    }
}

/// The OAuth error document a refusal carries.
#[derive(Debug, Clone, Deserialize)]
struct OauthError {
    error: String,
    error_description: Option<String>,
}

/// Why a call to Anthropic did not produce a token.
#[derive(Debug, thiserror::Error)]
pub enum AnthropicError {
    /// The request never completed, or the response was not the expected
    /// JSON.
    #[error("Anthropic request failed: {0}")]
    Transport(String),
    /// Anthropic refused the grant and said why.
    #[error("{code}: {description}")]
    Rejected {
        /// Anthropic's machine-readable error code.
        code: String,
        /// Anthropic's human-readable explanation.
        description: String,
    },
    /// Anthropic answered with a status flyco cannot interpret.
    #[error("Anthropic responded with HTTP {0}")]
    Status(u16),
}

impl From<HttpError> for AnthropicError {
    fn from(error: HttpError) -> Self {
        Self::Transport(error.to_string())
    }
}

/// Reads a token response, turning a refusal into [`AnthropicError`].
fn token_set(response: &HttpResponse) -> Result<TokenSet, AnthropicError> {
    if response.is_success() {
        return response
            .json::<TokenSet>()
            .map_err(|error| AnthropicError::Transport(error.to_string()));
    }

    response.json::<OauthError>().map_or_else(
        |_| Err(AnthropicError::Status(response.status)),
        |failure| {
            Err(AnthropicError::Rejected {
                description: failure
                    .error_description
                    .unwrap_or_else(|| "no description".to_owned()),
                code: failure.error,
            })
        },
    )
}

/// The one call the two OAuth routes and the refresh-on-use path make.
///
/// Behind a trait for the same reason the GitHub client is: the happy path
/// is otherwise untestable, because a route handler cannot be exercised
/// without standing in for `console.anthropic.com`.
pub trait ClaudeOauth: Send + Sync + Clone + 'static {
    /// Redeems an authorization code or a refresh token.
    ///
    /// # Errors
    ///
    /// Returns [`AnthropicError`] if the exchange fails or Anthropic
    /// refuses the grant.
    fn exchange(
        &self,
        request: TokenRequest<'_>,
    ) -> impl Future<Output = Result<TokenSet, AnthropicError>> + Send;

    /// Reads the plan the account's subscription is metered against.
    ///
    /// Live on every call and never cached: a window moves whether or not
    /// flyco is looking, and the reading that is a second old is the only
    /// one worth drawing.
    ///
    /// # Errors
    ///
    /// Returns [`AnthropicError`] if the read fails or Anthropic refuses
    /// the token.
    fn usage(
        &self,
        access_token: &str,
    ) -> impl Future<Output = Result<PlanUsageDocument, AnthropicError>> + Send;
}

/// The production client, speaking HTTP through zenwave — hyper natively,
/// Fetch inside the Worker.
#[derive(Debug, Clone, Copy, Default)]
pub struct ZenwaveClaude {
    transport: LiveTransport,
}

impl ZenwaveClaude {
    /// Creates the client.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            transport: LiveTransport::new(),
        }
    }
}

/// Describes one token exchange as an [`HttpRequest`].
///
/// Shared by every transport so a recorded exchange is the same bytes the
/// deployed control plane sends.
///
/// # Errors
///
/// Returns [`HttpError::Encoding`] if the body does not serialize, which
/// would mean this module's own request type is malformed.
pub fn token_request(request: TokenRequest<'_>) -> Result<HttpRequest, HttpError> {
    HttpRequest::new(Method::Post, TOKEN_URL)
        .header("accept", "application/json")
        .json_body(&request)
}

/// Performs one exchange over any transport.
///
/// # Errors
///
/// Returns [`AnthropicError`] if the request cannot be built or sent, or if
/// Anthropic refuses the grant.
pub async fn exchange_over<T: HttpTransport>(
    transport: &T,
    request: TokenRequest<'_>,
) -> Result<TokenSet, AnthropicError> {
    let response = transport.send(token_request(request)?).await?;
    token_set(&response)
}

/// One rolling plan window as `/api/oauth/usage` states it.
///
/// Both members are nullable on the wire and mean different things when
/// they are: a `utilization` of `null` is a bucket this plan does not
/// have, and a `resets_at` of `null` is a window with no stated turnover.
#[derive(Debug, Clone, Deserialize)]
struct UsageReading {
    /// How much of the window is spent, 0–100.
    utilization: Option<f64>,
    /// When the window turns over, ISO 8601.
    resets_at: Option<String>,
}

impl UsageReading {
    /// This reading as the window flyco states limits in, or `None` when
    /// the plan does not have it or it has already turned over.
    ///
    /// A window whose reset is in the past is not a window: the vendor is
    /// describing an interval that has ended, and drawing it would say a
    /// plan is spent when the thing it was spent against is over. That is
    /// the reading `usage_json` used to serve for days on end.
    fn window(&self, minutes: Option<u32>, scope: Option<&str>, now: u64) -> Option<UsageWindow> {
        let utilization = self.utilization?;
        let resets_at_unix = self.resets_at.as_deref().and_then(parse_instant);
        if resets_at_unix.is_some_and(|at| at <= i64::try_from(now).unwrap_or(i64::MAX)) {
            return None;
        }
        Some(UsageWindow::new(
            minutes,
            scope,
            percent(utilization),
            resets_at_unix,
        ))
    }
}

/// A vendor percentage as the whole number flyco draws.
///
/// Rounded *down* below a hundred, the way the Claude CLI prints it, so
/// that a ring reads full only when the vendor said the window is full —
/// and so that flyco and the CLI never disagree by a point.
fn percent(utilization: f64) -> u8 {
    if utilization >= 100.0 {
        return 100;
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to the range a percentage has before the cast"
    )]
    let whole = utilization.clamp(0.0, 100.0).floor() as u8;
    whole
}

/// An ISO 8601 instant as a Unix second, or `None` if it will not parse.
fn parse_instant(text: &str) -> Option<i64> {
    time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(time::OffsetDateTime::unix_timestamp)
}

/// The plan document `/api/oauth/usage` answers with.
///
/// Every member is optional because which buckets exist is the plan's
/// business: a Pro subscription has no per-model weekly window, and a plan
/// Anthropic introduces tomorrow is simply a key flyco does not read yet.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PlanUsageDocument {
    /// The session window every plan has.
    five_hour: Option<UsageReading>,
    /// The week across every model.
    seven_day: Option<UsageReading>,
    /// The week's Opus bucket, on the plans that meter Opus separately.
    seven_day_opus: Option<UsageReading>,
    /// The week's Sonnet bucket, on the plans that meter Sonnet separately.
    seven_day_sonnet: Option<UsageReading>,
}

impl PlanUsageDocument {
    /// Every window the plan currently has, in reading order.
    ///
    /// `now` is a parameter rather than the clock so that one answer is
    /// judged against one instant — and so the expiry rule above is
    /// testable without waiting for a window to turn over.
    #[must_use]
    pub fn windows(&self, now: u64) -> Vec<UsageWindow> {
        [
            (&self.five_hour, Some(FIVE_HOUR_MINUTES), None),
            (&self.seven_day, Some(SEVEN_DAY_MINUTES), None),
            (&self.seven_day_opus, Some(SEVEN_DAY_MINUTES), Some("Opus")),
            (
                &self.seven_day_sonnet,
                Some(SEVEN_DAY_MINUTES),
                Some("Sonnet"),
            ),
        ]
        .into_iter()
        .filter_map(|(reading, minutes, scope)| reading.as_ref()?.window(minutes, scope, now))
        .collect()
    }
}

/// Describes the plan read as an [`HttpRequest`].
///
/// The three headers are the ones `api.anthropic.com` requires of an
/// OAuth-authenticated caller; without the beta gate it answers `401` for
/// a token that is perfectly good.
#[must_use]
pub fn usage_request(access_token: &str) -> HttpRequest {
    HttpRequest::new(Method::Get, USAGE_URL)
        .header("accept", "application/json")
        .header("authorization", format!("Bearer {access_token}"))
        .header("anthropic-beta", OAUTH_BETA)
        .header("anthropic-version", ANTHROPIC_VERSION)
}

/// Reads the plan over any transport.
///
/// # Errors
///
/// Returns [`AnthropicError`] if the request cannot be sent, if Anthropic
/// refuses the token, or if the document does not parse.
pub async fn usage_over<T: HttpTransport>(
    transport: &T,
    access_token: &str,
) -> Result<PlanUsageDocument, AnthropicError> {
    let response = transport.send(usage_request(access_token)).await?;
    if !response.is_success() {
        return Err(AnthropicError::Status(response.status));
    }
    response
        .json::<PlanUsageDocument>()
        .map_err(|error| AnthropicError::Transport(error.to_string()))
}

impl ClaudeOauth for ZenwaveClaude {
    async fn exchange(&self, request: TokenRequest<'_>) -> Result<TokenSet, AnthropicError> {
        exchange_over(&self.transport, request).await
    }

    async fn usage(&self, access_token: &str) -> Result<PlanUsageDocument, AnthropicError> {
        usage_over(&self.transport, access_token).await
    }
}

/// The Claude client the router and the provisioning consumer carry.
///
/// An enum rather than a type parameter for the same reason
/// [`GithubClient`](crate::github::GithubClient) is one: `#[skyzen::openapi]`
/// cannot annotate a generic handler, and one concrete type keeps every
/// operation id stable.
#[derive(Debug, Clone)]
pub enum ClaudeClient {
    /// Talks to `console.anthropic.com`.
    Live(ZenwaveClaude),
    /// Answers from fixtures, for tests.
    #[cfg(test)]
    Fake(crate::testing::TestClaude),
}

impl Default for ClaudeClient {
    fn default() -> Self {
        Self::Live(ZenwaveClaude::new())
    }
}

impl ClaudeOauth for ClaudeClient {
    async fn exchange(&self, request: TokenRequest<'_>) -> Result<TokenSet, AnthropicError> {
        match self {
            Self::Live(client) => client.exchange(request).await,
            #[cfg(test)]
            Self::Fake(client) => client.exchange(request).await,
        }
    }

    async fn usage(&self, access_token: &str) -> Result<PlanUsageDocument, AnthropicError> {
        match self {
            Self::Live(client) => client.usage(access_token).await,
            #[cfg(test)]
            Self::Fake(client) => client.usage(access_token).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use flyco_provider::http::HttpResponse;
    use flyco_provider::testing::RecordedTransport;

    use super::{
        AnthropicError, PlanUsageDocument, REDIRECT_URI, SCOPE, TokenRequest, authorize_url,
        exchange_over, split_pasted_code, usage_over,
    };
    use crate::crypto::pkce;

    /// A token response as Anthropic returns one.
    const TOKEN_BODY: &str = include_str!("../fixtures/anthropic/token.json");

    /// A refusal as Anthropic returns one.
    const REFUSAL_BODY: &str = include_str!("../fixtures/anthropic/invalid_grant.json");

    fn query(url: &url::Url, name: &str) -> String {
        url.query_pairs()
            .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
            .unwrap_or_else(|| panic!("the authorize URL carries `{name}`"))
    }

    #[test]
    fn the_authorize_url_is_the_flow_the_claude_cli_runs() {
        let pkce = pkce().expect("mint a verifier");
        let url = authorize_url("client-id", &pkce.challenge, "the-state");

        assert_eq!(url.host_str(), Some("claude.ai"));
        assert_eq!(url.path(), "/oauth/authorize");
        assert_eq!(query(&url, "code"), "true");
        assert_eq!(query(&url, "client_id"), "client-id");
        assert_eq!(query(&url, "response_type"), "code");
        assert_eq!(query(&url, "redirect_uri"), REDIRECT_URI);
        assert_eq!(query(&url, "scope"), SCOPE);
        assert_eq!(query(&url, "code_challenge"), pkce.challenge);
        assert_eq!(query(&url, "code_challenge_method"), "S256");
        assert_eq!(query(&url, "state"), "the-state");
    }

    #[test]
    fn a_pasted_code_carries_its_state_or_stands_alone() {
        assert_eq!(
            split_pasted_code("the-code#the-state"),
            ("the-code", Some("the-state"))
        );
        assert_eq!(split_pasted_code("  the-code  "), ("the-code", None));
        assert_eq!(
            split_pasted_code(" the-code # the-state "),
            ("the-code", Some("the-state"))
        );
    }

    #[skyzen::test]
    async fn a_code_exchange_posts_exactly_what_anthropic_expects() {
        let transport = RecordedTransport::new(vec![HttpResponse::new(200, TOKEN_BODY)]);
        let tokens = exchange_over(
            &transport,
            TokenRequest::AuthorizationCode {
                code: "the-code",
                state: "the-state",
                client_id: "client-id",
                redirect_uri: REDIRECT_URI,
                code_verifier: "the-verifier",
            },
        )
        .await
        .expect("exchange the code");

        let request = transport.request(0);
        assert_eq!(request.method.as_str(), "POST");
        assert_eq!(request.url, "https://console.anthropic.com/v1/oauth/token");
        let body: serde_json::Value =
            serde_json::from_str(request.body_text().expect("UTF-8")).expect("a JSON body");
        assert_eq!(body["grant_type"], "authorization_code");
        assert_eq!(body["code"], "the-code");
        assert_eq!(body["state"], "the-state");
        assert_eq!(body["client_id"], "client-id");
        assert_eq!(body["redirect_uri"], REDIRECT_URI);
        assert_eq!(body["code_verifier"], "the-verifier");

        assert_eq!(tokens.access_token, "sk-ant-oat01-fixture-access");
        assert_eq!(tokens.refresh_token, "sk-ant-ort01-fixture-refresh");
        assert_eq!(tokens.expires_in, 28_800);
        assert_eq!(tokens.email_address(), Some("me@lexo.cool"));
        assert_eq!(tokens.expires_at_unix(1_000), 29_800);
    }

    #[skyzen::test]
    async fn a_refresh_sends_only_the_refresh_token_and_the_client_id() {
        let transport = RecordedTransport::new(vec![HttpResponse::new(200, TOKEN_BODY)]);
        exchange_over(
            &transport,
            TokenRequest::RefreshToken {
                refresh_token: "the-refresh-token",
                client_id: "client-id",
            },
        )
        .await
        .expect("refresh the grant");

        let body: serde_json::Value =
            serde_json::from_str(transport.request(0).body_text().expect("the body is UTF-8"))
                .expect("a JSON body");
        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["refresh_token"], "the-refresh-token");
        assert_eq!(body["client_id"], "client-id");
        assert!(body.get("code").is_none());
        assert!(body.get("code_verifier").is_none());
    }

    #[skyzen::test]
    async fn a_refusal_keeps_anthropics_own_reason() {
        let transport = RecordedTransport::new(vec![HttpResponse::new(400, REFUSAL_BODY)]);
        let error = exchange_over(
            &transport,
            TokenRequest::RefreshToken {
                refresh_token: "stale",
                client_id: "client-id",
            },
        )
        .await
        .expect_err("a refused grant");

        assert!(matches!(
            error,
            AnthropicError::Rejected { code, description }
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
                client_id: "client-id",
            },
        )
        .await
        .expect_err("an outage page");

        assert!(matches!(error, AnthropicError::Status(503)));
    }
    /// A plan document as `/api/oauth/usage` answers one.
    ///
    /// `resets_at` is stated relative to the reading below, because what
    /// the endpoint returns is always a window that is still open.
    fn plan_body(now: u64) -> String {
        serde_json::json!({
            "five_hour": { "utilization": 43.7, "resets_at": instant(now + 3_600) },
            "seven_day": { "utilization": 78.2, "resets_at": instant(now + 86_400) },
            "seven_day_opus": { "utilization": 100.0, "resets_at": instant(now + 86_400) },
            "seven_day_sonnet": { "utilization": null, "resets_at": null },
        })
        .to_string()
    }

    /// An instant as Anthropic states one.
    fn instant(unix: u64) -> String {
        time::OffsetDateTime::from_unix_timestamp(i64::try_from(unix).expect("a plausible clock"))
            .expect("a plausible clock")
            .format(&time::format_description::well_known::Rfc3339)
            .expect("RFC 3339 formats any instant")
    }

    #[skyzen::test]
    async fn a_plan_read_sends_the_bearer_token_and_the_beta_gate() {
        let now = 1_800_000_000;
        let transport = RecordedTransport::new(vec![HttpResponse::new(200, plan_body(now))]);
        let plan = usage_over(&transport, "sk-ant-oat01-the-account")
            .await
            .expect("read the plan");

        let request = transport.request(0);
        assert_eq!(request.method.as_str(), "GET");
        assert_eq!(request.url, "https://api.anthropic.com/api/oauth/usage");
        let header = |name: &str| {
            request
                .headers
                .iter()
                .find_map(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value.as_str()))
        };
        assert_eq!(
            header("authorization"),
            Some("Bearer sk-ant-oat01-the-account")
        );
        assert_eq!(header("anthropic-beta"), Some("oauth-2025-04-20"));
        assert_eq!(header("anthropic-version"), Some("2023-06-01"));

        // Floored, the way the CLI prints it, so a window reads full only
        // when the vendor said it is full. A bucket this plan does not
        // meter states no utilization and is no window at all.
        assert_eq!(
            plan.windows(now)
                .iter()
                .map(|window| (window.label.clone(), window.used_percent))
                .collect::<Vec<_>>(),
            [
                ("5-hour".to_owned(), 43),
                ("Weekly".to_owned(), 78),
                ("Weekly (Opus)".to_owned(), 100),
            ]
        );
    }

    #[skyzen::test]
    async fn a_window_whose_reset_has_passed_is_not_a_window() {
        // The whole of #381: a reading whose turnover is behind us
        // describes an interval that has ended, and drawing it is how a
        // plan at 43% rendered as a spent week that "resets now".
        let now = 1_800_000_000;
        let transport = RecordedTransport::new(vec![HttpResponse::new(200, plan_body(now))]);
        let plan = usage_over(&transport, "sk-ant-oat01-the-account")
            .await
            .expect("read the plan");

        assert_eq!(
            plan.windows(now + 86_401),
            [],
            "every window has turned over"
        );
        assert_eq!(
            plan.windows(now + 3_601)
                .iter()
                .map(|window| window.label.clone())
                .collect::<Vec<_>>(),
            ["Weekly".to_owned(), "Weekly (Opus)".to_owned()],
            "the five-hour window is over; the week is not"
        );
    }

    #[skyzen::test]
    async fn a_refused_plan_read_is_its_status() {
        let transport = RecordedTransport::new(vec![HttpResponse::new(401, "{}")]);
        let error = usage_over(&transport, "sk-ant-oat01-revoked")
            .await
            .expect_err("a refused token");
        assert!(matches!(error, AnthropicError::Status(401)));
    }

    #[test]
    fn a_plan_with_no_windows_at_all_reads_back_empty() {
        // Every member is optional: an account on a plan Anthropic meters
        // differently answers with keys this control plane does not read,
        // and that is a plan with nothing to draw rather than a failure.
        let plan: PlanUsageDocument =
            serde_json::from_str("{\"seven_day_oauth_apps\": {\"utilization\": 4.0}}")
                .expect("an unknown bucket is still a plan document");
        assert_eq!(plan.windows(1_800_000_000), []);
    }
}
