//! Cloudflare Turnstile: the proof a sign-in had a human in front of it.
//!
//! `POST /v1/auth/github/start` is the one public route whose whole job is
//! to mint account state — an OAuth `state` today, a user row at the end of
//! the callback — so it is the one route a registration bot wants. The login
//! page renders a Turnstile widget before it will offer sign-in, and this
//! module is what decides whether the token that widget produced is real:
//! `siteverify` answers the verdict, [`Verification::evaluate`] applies
//! flyco's own policy on top of it.
//!
//! Everything that leaves the Worker for `challenges.cloudflare.com` goes
//! through [`SiteVerify`]. Native builds use [`ZenwaveTurnstile`], Cloudflare
//! Workers use [`WorkerTurnstile`], and tests substitute their own — the
//! same shape [`crate::github`] uses, for the same reason.

use core::future::Future;

use serde::{Deserialize, Serialize};
#[cfg(target_arch = "wasm32")]
use skyzen_cloudflare::worker::send::IntoSendFuture as _;
#[cfg(not(target_arch = "wasm32"))]
use zenwave::{Client as _, ResponseExt as _};

use crate::error::ApiError;

/// Cloudflare's token-verification endpoint.
const SITE_VERIFY_URL: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";

/// The `data-action` the login page renders its widget with.
///
/// Siteverify echoes it back on a pass. Checking it is what keeps a token
/// minted by some other widget on the same sitekey from starting a sign-in.
pub const EXPECTED_ACTION: &str = "login";

/// Header Cloudflare sets on every edge request, carrying the real
/// visitor's address — forwarded to siteverify so its verdict sees the
/// browser, not the Worker.
const CONNECTING_IP_HEADER: &str = "cf-connecting-ip";

/// Reads the visitor's address out of a request's headers for siteverify.
///
/// Kept next to the constant so the one name Cloudflare chose lives in one
/// place.
#[must_use]
pub fn remote_ip(headers: &crate::extract::Headers) -> Option<&str> {
    headers.get(CONNECTING_IP_HEADER)
}

/// What siteverify decided about one token.
///
/// The wire document maps one-to-one — `success` is the verdict Cloudflare
/// made, and `action`, `hostname` and `cdata` are only echoed when the
/// widget that minted the token set them, which is why they are checked
/// rather than assumed.
#[derive(Debug, Clone, Deserialize)]
pub struct Verification {
    /// Whether Cloudflare believes the visitor was human.
    pub success: bool,
    /// The `data-action` the widget was rendered with.
    #[serde(default)]
    pub action: Option<String>,
    /// The hostname the token was issued on.
    #[serde(default)]
    pub hostname: Option<String>,
    /// Cloudflare's own refusal codes, when it refused.
    #[serde(default, rename = "error-codes")]
    pub error_codes: Vec<String>,
}

impl Verification {
    /// Decides whether this siteverify answer lets a sign-in proceed.
    ///
    /// Cloudflare's `success` is only the first of three checks. The token
    /// must also have been minted by *this* widget — siteverify echoes the
    /// page's `data-action` — and on *this* deployment's hostname, or a
    /// token a bot solved anywhere the public sitekey is embedded would
    /// open sign-in here. A deployment's own hostnames are configured,
    /// never derived from the request: a bot controls `Host` too.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::TurnstileRefused`] when the verdict does not
    /// clear all three checks.
    pub fn evaluate(&self, hostnames: &[String]) -> Result<(), ApiError> {
        if !self.success {
            let codes = if self.error_codes.is_empty() {
                "no reason given".to_owned()
            } else {
                self.error_codes.join(", ")
            };
            return Err(ApiError::TurnstileRefused {
                reason: format!("Cloudflare did not pass the check: {codes}"),
            });
        }
        if self.action.as_deref() != Some(EXPECTED_ACTION) {
            return Err(ApiError::TurnstileRefused {
                reason: "the check was not minted for sign-in".to_owned(),
            });
        }
        let issued_here = self
            .hostname
            .as_ref()
            .is_some_and(|hostname| hostnames.iter().any(|allowed| allowed == hostname));
        if !issued_here {
            return Err(ApiError::TurnstileRefused {
                reason: "the check was minted on a different site".to_owned(),
            });
        }
        Ok(())
    }
}

/// The JSON posted to siteverify.
#[derive(Serialize)]
struct SiteverifyRequest<'a> {
    secret: &'a str,
    response: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    remoteip: Option<&'a str>,
}

/// Why a siteverify call itself failed.
///
/// A token Cloudflare examined and refused is not one of these — that is a
/// [`Verification`] whose `success` is false, which [`evaluate`] turns into
/// the refusal the caller sees. This is the plumbing breaking: the network,
/// a non-2xx status, or a body that is not siteverify's document.
#[derive(Debug, Clone, thiserror::Error)]
pub enum TurnstileError {
    /// The request never produced a readable answer.
    #[error("{0}")]
    Transport(String),
    /// siteverify answered with a non-2xx status.
    #[error("siteverify answered {status}: {reason}")]
    Status {
        /// The HTTP status it answered with.
        status: u16,
        /// What the body said, when it said anything.
        reason: String,
    },
}

/// The one call flyco makes to Cloudflare to judge a Turnstile token.
///
/// Behind a trait for the same reason [`crate::github::GithubOauth`] is:
/// the sign-in handler cannot be exercised without standing in for
/// `challenges.cloudflare.com`.
pub trait SiteVerify: Send + Sync + Clone + 'static {
    /// Asks Cloudflare what it thinks of a widget token.
    ///
    /// `remote_ip` is the visitor's real address when the platform knows
    /// it — the verdict is sharper with it than with the Worker's own.
    ///
    /// # Errors
    ///
    /// Returns [`TurnstileError`] when the call itself fails. A refusal is
    /// *not* an error — it arrives as a [`Verification`] with
    /// `success: false`, so an outage and a bot can never be confused.
    fn verify(
        &self,
        secret: &str,
        token: &str,
        remote_ip: Option<&str>,
    ) -> impl Future<Output = Result<Verification, TurnstileError>> + Send;
}

/// The Turnstile client the router actually carries.
///
/// Concrete for the same reason [`crate::github::GithubClient`] is:
/// `#[skyzen::openapi]` emits items naming every argument type, so a
/// generic handler's operation id would carry the substituted type — not a
/// name a generated client can be written against.
#[derive(Debug, Clone)]
pub enum TurnstileClient {
    /// Talks to `challenges.cloudflare.com`.
    #[cfg(not(target_arch = "wasm32"))]
    Live(ZenwaveTurnstile),
    /// Talks to `challenges.cloudflare.com` through `WorkerGlobalScope.fetch`.
    #[cfg(target_arch = "wasm32")]
    Live(WorkerTurnstile),
    /// Answers from fixtures, for tests.
    #[cfg(test)]
    Fake(crate::testing::TestTurnstile),
}

impl Default for TurnstileClient {
    fn default() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        {
            Self::Live(ZenwaveTurnstile::new())
        }
        #[cfg(target_arch = "wasm32")]
        {
            Self::Live(WorkerTurnstile::new())
        }
    }
}

impl SiteVerify for TurnstileClient {
    async fn verify(
        &self,
        secret: &str,
        token: &str,
        remote_ip: Option<&str>,
    ) -> Result<Verification, TurnstileError> {
        match self {
            Self::Live(client) => client.verify(secret, token, remote_ip).await,
            #[cfg(test)]
            Self::Fake(client) => client.verify(secret, token, remote_ip).await,
        }
    }
}

/// The first non-empty line of a non-2xx body — what the status refusal is
/// reported with.
fn refusal_reason(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map_or_else(|| "no error document".to_owned(), str::to_owned)
}

/// The native production [`SiteVerify`], speaking HTTP through zenwave.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, Default)]
pub struct ZenwaveTurnstile;

#[cfg(not(target_arch = "wasm32"))]
impl ZenwaveTurnstile {
    /// Creates the client.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn transport(error: impl core::fmt::Display) -> TurnstileError {
    TurnstileError::Transport(error.to_string())
}

/// Reads a native siteverify body, but only after the status line says the
/// call worked.
#[cfg(not(target_arch = "wasm32"))]
async fn read_answer(response: zenwave::Response) -> Result<Verification, TurnstileError> {
    let status = response.status();
    if !status.is_success() {
        let body = response.into_string().await.map_err(transport)?;
        return Err(TurnstileError::Status {
            status: status.as_u16(),
            reason: refusal_reason(&body),
        });
    }
    response
        .into_json::<Verification>()
        .await
        .map_err(transport)
}

#[cfg(not(target_arch = "wasm32"))]
impl SiteVerify for ZenwaveTurnstile {
    async fn verify(
        &self,
        secret: &str,
        token: &str,
        remote_ip: Option<&str>,
    ) -> Result<Verification, TurnstileError> {
        let mut client = zenwave::client();
        let response = client
            .post(SITE_VERIFY_URL)
            .map_err(transport)?
            .header("Accept", "application/json")
            .map_err(transport)?
            .json_body(&SiteverifyRequest {
                secret,
                response: token,
                remoteip: remote_ip,
            })
            .map_err(transport)?
            .await
            .map_err(transport)?;
        read_answer(response).await
    }
}

/// The Cloudflare production [`SiteVerify`], speaking HTTP through
/// `WorkerGlobalScope.fetch`.
#[cfg(target_arch = "wasm32")]
#[derive(Debug, Clone, Copy, Default)]
pub struct WorkerTurnstile;

#[cfg(target_arch = "wasm32")]
impl WorkerTurnstile {
    /// Creates the client.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[cfg(target_arch = "wasm32")]
fn transport(error: impl core::fmt::Display) -> TurnstileError {
    TurnstileError::Transport(error.to_string())
}

/// Reads a Worker siteverify body, but only after the status line says the
/// call worked.
#[cfg(target_arch = "wasm32")]
async fn read_answer(
    mut response: skyzen_cloudflare::worker::Response,
) -> Result<Verification, TurnstileError> {
    let status = response.status_code();
    if !(200..300).contains(&status) {
        let body = response.text().into_send().await.map_err(transport)?;
        return Err(TurnstileError::Status {
            status,
            reason: refusal_reason(&body),
        });
    }
    response
        .json::<Verification>()
        .into_send()
        .await
        .map_err(transport)
}

#[cfg(target_arch = "wasm32")]
impl SiteVerify for WorkerTurnstile {
    async fn verify(
        &self,
        secret: &str,
        token: &str,
        remote_ip: Option<&str>,
    ) -> Result<Verification, TurnstileError> {
        let request = skyzen_cloudflare::json_request(
            skyzen_cloudflare::worker::Method::Post,
            SITE_VERIFY_URL,
            &SiteverifyRequest {
                secret,
                response: token,
                remoteip: remote_ip,
            },
            &[("Accept", "application/json")],
        )
        .map_err(transport)?;
        let response = skyzen_cloudflare::worker::Fetch::Request(request)
            .send()
            .into_send()
            .await
            .map_err(transport)?;
        read_answer(response).await
    }
}

#[cfg(test)]
mod tests {
    use super::{EXPECTED_ACTION, Verification};

    fn verification() -> Verification {
        Verification {
            success: true,
            action: Some(EXPECTED_ACTION.to_owned()),
            hostname: Some("dev.flyco.dev".to_owned()),
            error_codes: Vec::new(),
        }
    }

    fn hostnames() -> Vec<String> {
        vec!["dev.flyco.dev".to_owned(), "flyco.pages.dev".to_owned()]
    }

    #[test]
    fn a_pass_on_this_site_for_sign_in_is_accepted() {
        verification()
            .evaluate(&hostnames())
            .expect("the verification passes");
    }

    #[test]
    fn a_cloudflare_refusal_names_its_codes() {
        let mut verdict = verification();
        verdict.success = false;
        verdict.hostname = None;
        verdict.error_codes = vec!["timeout-or-duplicate".to_owned()];

        let error = verdict
            .evaluate(&hostnames())
            .expect_err("a refusal cannot pass");
        assert!(error.to_string().contains("timeout-or-duplicate"));
    }

    #[test]
    fn a_token_minted_for_another_action_is_refused() {
        let mut verdict = verification();
        verdict.action = Some("comment".to_owned());

        verdict
            .evaluate(&hostnames())
            .expect_err("another action cannot pass");
    }

    #[test]
    fn a_token_minted_elsewhere_is_refused() {
        let mut verdict = verification();
        verdict.hostname = Some("bots.example".to_owned());

        verdict
            .evaluate(&hostnames())
            .expect_err("another hostname cannot pass");
    }

    #[test]
    fn a_token_without_a_hostname_is_refused() {
        let mut verdict = verification();
        verdict.hostname = None;

        verdict
            .evaluate(&hostnames())
            .expect_err("no hostname cannot pass");
    }
}
