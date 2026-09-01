//! Inbound GitHub webhooks.
//!
//! This route carries no flyco credential and is reachable by anyone who
//! knows the URL, so the *only* thing that separates a real GitHub delivery
//! from a forgery is the `X-Hub-Signature-256` HMAC over the raw body. That
//! makes the order of operations part of the security boundary rather than a
//! matter of style: the signature is verified against the exact bytes
//! received, with a constant-time comparison, **before** the body is parsed,
//! before it is logged, and before anything is dispatched.
//!
//! [`accept`] is written in that order and nothing else in this module reads
//! the body, so there is no reachable path in which an unverified payload is
//! looked at. The signing secret is required deployment configuration, so a
//! Worker can never start with this route present but unusable.
//!
//! # What a verified delivery does
//!
//! A `check_run` or `workflow_run` that finished in a failing conclusion is
//! the CI-autofix trigger: every *active* session on that repository is woken
//! with a goal-shaped message, through the same
//! [`ControlToDaemon`](flyco_core::ControlToDaemon) path a user message takes.
//! There is one command channel to a session's daemon and this is it.
//!
//! Every other outcome is `204`. An event flyco does not act on, a passing
//! run, a repository nobody is working on — none of them is an error, and
//! answering with one would only make GitHub redeliver it. GitHub retries
//! anything that is not a success.

use askama::Template;
use flyco_core::{ControlToDaemon, RepoSlug, SessionId, SessionState};
use hmac::{Hmac, Mac as _};
use serde::Deserialize;
use sha2::Sha256;
use skyzen::routing::{CreateRouteNode, Route, RouteNode, Routes as _};
use skyzen::sql;
use skyzen::utils::{Bytes, State};
use skyzen_services::Db;

use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::extract::Headers;
use crate::problem::Outcome;
use crate::respond::NoContent;
use crate::rooms::Rooms;

/// Header GitHub signs each delivery with, per its webhook documentation.
pub const SIGNATURE_HEADER: &str = "x-hub-signature-256";

/// Header naming which event a delivery carries.
pub const EVENT_HEADER: &str = "x-github-event";

/// Scheme prefix of a signature value: `sha256=<hex>`.
const SIGNATURE_PREFIX: &str = "sha256=";

/// Byte length of an HMAC-SHA256 tag.
const SIGNATURE_LEN: usize = 32;

/// The event GitHub sends to prove a hook is wired up.
const PING_EVENT: &str = "ping";

/// The event a single check reports through.
const CHECK_RUN_EVENT: &str = "check_run";

/// The event a whole Actions workflow reports through.
const WORKFLOW_RUN_EVENT: &str = "workflow_run";

/// Conclusions that mean the run failed and is worth waking a session for.
///
/// `cancelled`, `neutral`, `skipped` and `stale` are deliberately absent:
/// none of them says anything went wrong, and an agent woken by one would
/// go looking for a fault that does not exist.
const FAILING_CONCLUSIONS: [&str; 3] = ["failure", "timed_out", "action_required"];

/// The repository half of every delivery flyco reads.
#[derive(Debug, Deserialize)]
struct Repository {
    /// `owner/name`, which is exactly what `sessions.repo` holds.
    full_name: String,
}

/// The check suite a check run belongs to.
///
/// Read for one field: `check_run` carries the branch here, while
/// `workflow_run` carries it on the run itself.
#[derive(Debug, Default, Deserialize)]
struct CheckSuite {
    #[serde(default)]
    head_branch: Option<String>,
}

/// The half of a completed run flyco tells an agent about.
#[derive(Debug, Deserialize)]
struct Run {
    /// The run's name, as GitHub shows it.
    name: String,
    /// How it finished. `null` while it is still going.
    ///
    /// Kept as GitHub's own token rather than an enum: the set is the
    /// vendor's to extend, and a conclusion flyco has never heard of must
    /// read as "not a failure" instead of failing the delivery.
    #[serde(default)]
    conclusion: Option<String>,
    /// Where a human reads the failure.
    #[serde(default)]
    html_url: Option<String>,
    /// Branch the run tested, as `workflow_run` reports it.
    #[serde(default)]
    head_branch: Option<String>,
    /// Branch the run tested, as `check_run` reports it.
    #[serde(default)]
    check_suite: Option<CheckSuite>,
}

impl Run {
    /// The branch this run tested, wherever the event put it.
    fn branch(&self) -> Option<String> {
        self.head_branch.clone().or_else(|| {
            self.check_suite
                .as_ref()
                .and_then(|suite| suite.head_branch.clone())
        })
    }

    /// Whether this run finished in a state worth waking a session for.
    fn failed(&self) -> bool {
        self.conclusion
            .as_deref()
            .is_some_and(|conclusion| FAILING_CONCLUSIONS.contains(&conclusion))
    }
}

/// A `check_run` delivery.
#[derive(Debug, Deserialize)]
struct CheckRunDelivery {
    repository: Repository,
    check_run: Run,
}

/// A `workflow_run` delivery.
#[derive(Debug, Deserialize)]
struct WorkflowRunDelivery {
    repository: Repository,
    workflow_run: Run,
}

/// The notice a woken session is given.
///
/// A rendered template rather than an assembled string: the message is
/// structured text an agent acts on, so its layout belongs in a file the
/// build checks and a dropped field is a compile error.
#[derive(Debug, Template)]
#[template(path = "github/ci_failure.txt", escape = "none")]
struct CiFailure {
    /// What kind of thing failed, in the words the notice uses.
    kind: &'static str,
    /// The run's name.
    name: String,
    /// The repository, `owner/name`.
    repo: String,
    /// GitHub's own conclusion token.
    conclusion: String,
    /// Branch the run tested, when the event named one.
    branch: Option<String>,
    /// Where a human reads the failure, when the event named it.
    url: Option<String>,
}

/// The session ids a repository's failure has to reach.
#[derive(Debug, skyzen::FromRow)]
struct ActiveSessionRow {
    id: SessionId,
}

/// Accepts a signed GitHub webhook delivery.
///
/// Answers `204`: GitHub only needs to know the delivery was accepted, and
/// what it triggers happens on the session relay rather than in this
/// response.
#[skyzen::openapi]
async fn receive_github_webhook(
    headers: Headers,
    body: Bytes,
    State(config): State<ApiConfig>,
    rooms: Rooms,
    db: Db,
) -> Outcome<NoContent> {
    accept(&headers, body.as_ref(), &config, &rooms, &db)
        .await
        .into()
}

/// Verifies a delivery and then, and only then, acts on it.
///
/// The three steps are in this order because the order *is* the security
/// property: an unverified body is never looked at, and the event header is
/// read after the bytes it describes have
/// been proven to come from GitHub.
async fn accept(
    headers: &Headers,
    body: &[u8],
    config: &ApiConfig,
    rooms: &Rooms,
    db: &Db,
) -> Result<NoContent, ApiError> {
    let secret = config.github_webhook_secret();
    verify(secret, headers.get(SIGNATURE_HEADER), body)?;

    let event = headers.get(EVENT_HEADER).unwrap_or_default();
    match event {
        PING_EVENT => {
            tracing::info!("GitHub pinged a flyco webhook");
            Ok(NoContent)
        }
        CHECK_RUN_EVENT => {
            let delivery: CheckRunDelivery = decode(body, event)?;
            dispatch(
                rooms,
                db,
                "a check run",
                &delivery.repository,
                &delivery.check_run,
            )
            .await
        }
        WORKFLOW_RUN_EVENT => {
            let delivery: WorkflowRunDelivery = decode(body, event)?;
            dispatch(
                rooms,
                db,
                "a workflow run",
                &delivery.repository,
                &delivery.workflow_run,
            )
            .await
        }
        other => {
            // Subscribing to more events than flyco acts on is normal, and
            // refusing one would make GitHub redeliver it forever.
            tracing::debug!(event = %other, "ignoring a GitHub event flyco does not act on");
            Ok(NoContent)
        }
    }
}

/// Checks `X-Hub-Signature-256` against the raw body, in constant time.
///
/// The comparison is [`Mac::verify_slice`], which is constant-time by
/// construction — a byte-by-byte `==` on a tag leaks, through timing, how
/// much of a guess was right, and that is enough to forge one.
///
/// A missing signature and a wrong one are the same refusal on purpose:
/// telling a forger which half they got wrong is a free oracle, and GitHub
/// can act on neither answer.
fn verify(secret: &str, presented: Option<&str>, body: &[u8]) -> Result<(), ApiError> {
    let digits = presented
        .and_then(|value| value.strip_prefix(SIGNATURE_PREFIX))
        .ok_or(ApiError::WebhookUnverified)?;

    // `decode_to_slice` insists on exactly the tag's length, so a signature
    // of the wrong size is refused here rather than compared short.
    let mut presented_tag = [0_u8; SIGNATURE_LEN];
    hex::decode_to_slice(digits, &mut presented_tag).map_err(|_| ApiError::WebhookUnverified)?;

    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|_| ApiError::CorruptRecord("the webhook secret is not a usable HMAC key"))?;
    mac.update(body);
    mac.verify_slice(&presented_tag)
        .map_err(|_| ApiError::WebhookUnverified)
}

/// Reads a verified body as the document its event header claims.
fn decode<T: for<'de> Deserialize<'de>>(body: &[u8], event: &str) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|_| ApiError::WebhookMalformed {
        event: event.to_owned(),
    })
}

/// Wakes every active session on the repository a failing run belongs to.
///
/// Every one of them, not the first: two sessions may legitimately be open
/// on one repository, and picking one arbitrarily would leave the other
/// working against a tree CI has already rejected. A repository nobody is
/// working on matches nothing, which is a `204` — GitHub delivers every
/// hook to every subscriber and most of them are about no session at all.
async fn dispatch(
    rooms: &Rooms,
    db: &Db,
    kind: &'static str,
    repository: &Repository,
    run: &Run,
) -> Result<NoContent, ApiError> {
    if !run.failed() {
        return Ok(NoContent);
    }

    let repo: RepoSlug = repository
        .full_name
        .parse()
        .map_err(|_| ApiError::WebhookMalformed {
            event: kind.to_owned(),
        })?;

    let sessions: Vec<ActiveSessionRow> = sql!(
        db,
        "SELECT id FROM sessions WHERE repo = {&repo} AND state = {SessionState::Active} \
         ORDER BY last_active_unix DESC, id"
    )
    .fetch_all()
    .await?;
    if sessions.is_empty() {
        tracing::debug!(%repo, "a CI failure matched no active session");
        return Ok(NoContent);
    }

    let notice = CiFailure {
        kind,
        name: run.name.clone(),
        repo: repo.as_str().to_owned(),
        conclusion: run.conclusion.clone().unwrap_or_default(),
        branch: run.branch(),
        url: run.html_url.clone(),
    }
    .render()
    .map_err(|_| ApiError::CorruptRecord("the CI failure notice did not render"))?;

    let command = ControlToDaemon::UserMessage { text: notice };
    let mut delivered = 0_usize;
    let mut last_failure = None;
    for session in &sessions {
        match rooms.command(session.id, &command).await {
            Ok(()) => delivered += 1,
            Err(error) => {
                tracing::warn!(session = %session.id, %error, "a CI failure did not reach its room");
                last_failure = Some(error);
            }
        }
    }

    // A delivery that reached nobody is worth retrying, and GitHub retries
    // anything that is not a success. One that reached some of its sessions
    // is not: a redelivery would wake the rest a second time.
    if let (0, Some(error)) = (delivered, last_failure) {
        return Err(error);
    }

    tracing::info!(%repo, sessions = delivered, "woke sessions on a CI failure");
    Ok(NoContent)
}

/// The webhook route, which authenticates itself by signature.
pub fn routes() -> Vec<RouteNode> {
    Route::new(("/v1/webhooks/github".post(receive_github_webhook),)).into_route_nodes()
}
