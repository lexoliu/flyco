//! `flyco login`, `flyco logout`, `flyco auth status`.
//!
//! Login is a browser-approval handshake: `POST /v1/cli-sessions` opens an
//! attempt (unauthenticated — the whole point is that the CLI holds no
//! credential yet), the user approves it on the PWA's `/cli/authorize`
//! page under their existing session, and the poll that follows collects
//! the minted `fk_` key exactly once.

use flyco_core::{CliSessionKey, CreateCliSession, CurrentUser};
use zenwave::ResponseExt as _;

use crate::client::Api;
use crate::{Exit, Failure, Outcome, creds, out};

/// The poll cadence while the user is deciding.
const POLL_EVERY: core::time::Duration = core::time::Duration::from_secs(2);

/// `flyco login`.
///
/// With `--token`, stores the given key directly — the headless path for
/// agents and SSH'd boxes. Without one, runs the browser handshake.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when the control plane refuses or
/// the credentials cannot be stored.
pub async fn login(api: &Api, token: Option<String>, mode: out::Mode) -> Outcome<()> {
    match token {
        Some(key) => login_with_token(api, key, mode).await,
        None => login_browser(api, mode).await,
    }
}

/// `flyco login --token …`: check the key names someone before saving it.
async fn login_with_token(api: &Api, key: String, mode: out::Mode) -> Outcome<()> {
    // The only way to know a key is good is to use it: authenticate as it
    // once, then save it if `me` answered.
    let probe = Api::new(api_base(api), Some(key.clone()));
    let me: CurrentUser = probe.get("/v1/me").await?;
    let credentials = creds::Credentials { key_id: None, key };
    let path = creds::store(&credentials)?;
    match mode {
        out::Mode::Json => out::emit(&serde_json::json!({
            "login": me.login,
            "credentials": path.display().to_string(),
        })),
        out::Mode::Human => out::print(&format!(
            "Signed in as {} — key stored at {}",
            me.login,
            path.display()
        )),
    }
}

/// The browser handshake.
async fn login_browser(api: &Api, mode: out::Mode) -> Outcome<()> {
    let hostname = rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned();
    let session: flyco_core::CliSession = api
        .post(
            "/v1/cli-sessions",
            &CreateCliSession {
                hostname: Some(hostname),
            },
        )
        .await?;

    match mode {
        // The attempt itself is the answer an agent needs: the URL to open
        // and the poll token are the whole document.
        out::Mode::Json => out::emit(&session)?,
        out::Mode::Human => {
            out::print("Open this page to approve the sign-in:")?;
            out::print(&format!("  {}", session.authorize_url))?;
            if open::that(&session.authorize_url).is_ok() {
                out::print("(opened in your browser — waiting for approval)")?;
            }
        }
    }

    let deadline = std::time::Instant::now()
        + core::time::Duration::from_secs(session.expires_at_unix.saturating_sub(now_unix()) + 30);
    let poll = format!("/v1/cli-sessions/{}?s={}", session.id, session.poll_token);
    let key: CliSessionKey = loop {
        let response = api.poll(&poll).await?;
        match response.status().as_u16() {
            200 => {
                break response
                    .into_json::<CliSessionKey>()
                    .await
                    .map_err(|error| {
                        Failure::transport(format!("the approval answered badly: {error}"))
                    })?;
            }
            202 => {}
            403 => {
                return Err(Failure::problem(
                    Exit::Auth,
                    "the sign-in was denied".to_owned(),
                ));
            }
            404 | 410 => {
                return Err(Failure::problem(
                    Exit::NotFoundOrConflict,
                    "the sign-in attempt expired or was already collected".to_owned(),
                ));
            }
            status => {
                return Err(Failure::problem(
                    Exit::Problem,
                    format!("the sign-in poll answered HTTP {status}"),
                ));
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(Failure::problem(
                Exit::Timeout,
                "the sign-in was not approved in time".to_owned(),
            ));
        }
        tokio::time::sleep(POLL_EVERY).await;
    };

    let credentials = creds::Credentials {
        key_id: Some(key.key_id),
        key: key.key,
    };
    let path = creds::store(&credentials)?;
    match mode {
        out::Mode::Json => out::emit(&serde_json::json!({
            "key_id": credentials.key_id,
            "credentials": path.display().to_string(),
        })),
        out::Mode::Human => out::print(&format!("Signed in — key stored at {}", path.display())),
    }
}

/// `flyco logout`: revoke the stored key, then drop the file.
///
/// Revocation comes first so a failed `DELETE` keeps the file — the
/// credential still works and a retry can still revoke it.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when the control plane refuses or
/// the credentials cannot be stored.
pub async fn logout(api: &Api, mode: out::Mode) -> Outcome<()> {
    let Some(credentials) = creds::resolve() else {
        return match mode {
            out::Mode::Json => out::emit(&serde_json::json!({ "logged_in": false })),
            out::Mode::Human => out::print("Not signed in."),
        };
    };
    if let Some(key_id) = credentials.key_id {
        api.delete(&format!("/v1/api-keys/{key_id}")).await?;
    }
    creds::remove();
    match mode {
        out::Mode::Json => out::emit(&serde_json::json!({ "logged_in": false })),
        out::Mode::Human => out::print("Signed out."),
    }
}

/// `flyco auth status`: who the credential belongs to.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when the control plane refuses or
/// the credentials cannot be stored.
pub async fn status(api: &Api, mode: out::Mode) -> Outcome<()> {
    let me: CurrentUser = api.get("/v1/me").await?;
    match mode {
        out::Mode::Json => out::emit(&me),
        out::Mode::Human => out::print(&format!(
            "{} ({} sessions at once)",
            me.login, me.session_cap
        )),
    }
}

/// The base URL the configured client points at, for the `--token` probe.
fn api_base(api: &Api) -> url::Url {
    api.base()
}

/// Seconds since the epoch, for the poll's deadline.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}
