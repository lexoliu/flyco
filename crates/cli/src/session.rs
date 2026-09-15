//! `flyco session …` — the agent path's primitives, one verb per route.
//!
//! Everything here is a faithful projection: the request bodies are the
//! API's own DTOs and the output is the API's own documents, so an agent
//! that knows `/v1` already knows this surface.

use std::io::{IsTerminal as _, Read as _};

use flyco_core::wire::ClientEvent;
use flyco_core::{
    ApprovalView, DecideApproval, DirectoryListing, EnvDocument, EnvEntry, FileContent,
    SendMessage, SessionDetail, SessionId, SessionSummary, ShellOutcome, ShellStream, UpdateEnv,
    UpdateSession, WorkdirDiff,
};

use crate::cli::SessionSpec;
use crate::client::Api;
use crate::follow::{Follow, Item};
use crate::{Exit, Failure, Outcome, out};

/// `session list` — `GET /v1/sessions`, optionally filtered client-side
/// (the route lists the whole account; `--state` is a read of the result).
///
/// # Errors
/// Returns [`Failure`](crate::Failure) on usage errors, when a request fails,
/// or when the answer does not decode.
pub async fn list(
    api: &Api,
    state: Option<flyco_core::SessionState>,
    mode: out::Mode,
) -> Outcome<()> {
    let sessions: Vec<SessionSummary> = api.get("/v1/sessions").await?;
    let sessions: Vec<SessionSummary> = sessions
        .into_iter()
        .filter(|session| state.is_none_or(|wanted| session.state == wanted))
        .collect();
    match mode {
        out::Mode::Json => out::emit(&sessions),
        out::Mode::Human => {
            let rows: Vec<Vec<String>> = sessions
                .iter()
                .map(|session| {
                    vec![
                        session.id.to_string(),
                        state_label(session),
                        repos_label(&session.repos),
                        session.title.clone(),
                    ]
                })
                .collect();
            out::print(&out::table(&["ID", "STATE", "REPO", "TITLE"], &rows))
        }
    }
}

/// The label a session's checkouts render as — the primary slug, plus
/// `+N` for the rest.
#[must_use]
pub fn repos_label(repos: &[flyco_core::SessionRepo]) -> String {
    let Some(first) = repos.first() else {
        return String::new();
    };
    match repos.len() - 1 {
        0 => first.slug.to_string(),
        extra => format!("{} +{extra}", first.slug),
    }
}

/// The state a list row shows — `paused · budget` rather than bare
/// `paused`, because the reason is the actionable half.
fn state_label(session: &SessionSummary) -> String {
    let state = serde_json::to_value(session.state)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    if let Some(reason) = session.paused_reason {
        return match reason {
            flyco_core::PausedReason::Budget => format!("{state} · budget"),
            flyco_core::PausedReason::UsageLimit => format!("{state} · plan window"),
        };
    }
    if let Some(reason) = session.interrupted_reason {
        let label = match reason {
            flyco_core::InterruptedReason::SpotReclaimed => "spot reclaimed",
            flyco_core::InterruptedReason::Suspended => "suspended",
            flyco_core::InterruptedReason::MachineLost => "machine lost",
        };
        return format!("{state} · {label}");
    }
    state
}

/// `session get` — `GET /v1/sessions/{id}`.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) on usage errors, when a request fails,
/// or when the answer does not decode.
pub async fn get(api: &Api, id: &str, mode: out::Mode) -> Outcome<()> {
    let session: SessionDetail = api.get(&format!("/v1/sessions/{id}")).await?;
    match mode {
        out::Mode::Json => out::emit(&session),
        out::Mode::Human => out::print(&format!(
            "{}  {}  {}\n{}\n{}",
            session.summary.id,
            state_label(&session.summary),
            repos_label(&session.summary.repos),
            session.summary.title,
            session
                .summary
                .repos
                .iter()
                .map(|repo| {
                    repo.branch.as_ref().map_or_else(
                        || repo.slug.to_string(),
                        |branch| format!("{}: {branch}", repo.slug),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )),
    }
}

/// `session create` — `POST /v1/sessions`, then the env PUT when `--env`
/// was given.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) on usage errors, when a request fails,
/// or when the answer does not decode.
pub async fn create(
    api: &Api,
    harness: flyco_core::HarnessKind,
    spec: &SessionSpec,
    mode: out::Mode,
) -> Outcome<SessionDetail> {
    let prompt = resolve_text(
        spec.prompt.clone(),
        spec.prompt_file.clone(),
        false,
        "--prompt",
    )?;
    let request = spec.to_request(harness, prompt)?;
    let headers: &[(&str, &str)] = match &spec.idempotency_key {
        Some(key) => &[("Idempotency-Key", key.as_str())],
        None => &[],
    };
    let session: SessionDetail = api
        .post_with_headers("/v1/sessions", &request, headers)
        .await?;
    set_env(api, &session.summary.id, &spec.env).await?;
    match mode {
        out::Mode::Json => out::emit(&session)?,
        out::Mode::Human => {
            out::print(&format!(
                "{}  {}",
                session.summary.id, session.summary.title
            ))?;
        }
    }
    Ok(session)
}

/// `session send` — `POST …/messages`.
///
/// The text comes from `-m`, `-f`, `--stdin`, or piped stdin — in that
/// order — and a non-TTY stdin is read even without the flag, so
/// `echo hi | flyco session send <id>` does the obvious thing.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) on usage errors, when a request fails,
/// or when the answer does not decode.
pub async fn send(
    api: &Api,
    id: &str,
    text: Option<String>,
    file: Option<String>,
    stdin_flag: bool,
) -> Outcome<()> {
    let text = resolve_text(text, file, stdin_flag, "--message or --file or --stdin")?;
    api.post::<SendMessage, serde_json::Value>(
        &format!("/v1/sessions/{id}/messages"),
        &SendMessage { text },
    )
    .await?;
    Ok(())
}

/// `session exec` — `!cmd` sugared: post the command, follow the session's
/// events, relay this run's output, exit with its status.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) on usage errors, when a request fails,
/// or when the answer does not decode.
pub async fn exec(api: &Api, id: &str, command: &[String]) -> Outcome<Exit> {
    let session: SessionId = id
        .parse()
        .map_err(|_| Failure::usage(format!("`{id}` is not a session id")))?;
    let text = command.join(" ");

    // Subscribe *before* posting: the run's identity arrives as a
    // `ShellCommand` echo on the same stream its output will.
    let mut follow = Follow::new(api, session);
    api.post::<flyco_core::RunShell, serde_json::Value>(
        &format!("/v1/sessions/{id}/shell"),
        &flyco_core::RunShell {
            command: text.clone(),
        },
    )
    .await?;

    let mut run = None;
    loop {
        let item = follow.next().await?;
        let Some(event) = item.event() else { continue };
        match event {
            ClientEvent::ShellCommand { run: ours, command } if command == text => {
                run = Some(ours);
            }
            ClientEvent::ShellOutput {
                run: seen,
                stream,
                data,
            } if Some(seen) == run => match stream {
                ShellStream::Stdout => out::raw_out(data.as_bytes())?,
                ShellStream::Stderr => out::raw_err(data.as_bytes())?,
            },
            ClientEvent::ShellExited {
                run: seen, outcome, ..
            } if Some(seen) == run => {
                return Ok(match outcome {
                    ShellOutcome::Exited { code } => exit_code(code),
                    ShellOutcome::TimedOut { .. } => Exit::Timeout,
                    ShellOutcome::Busy => Exit::NotFoundOrConflict,
                    // Nothing ran it — the daemon was gone.
                    ShellOutcome::Offline => Exit::Transport,
                    // Paused on budget, or mid-reclaim — the same "wait
                    // it out" a paused session asks for.
                    ShellOutcome::Refused => Exit::Paused,
                    ShellOutcome::Signalled
                    | ShellOutcome::Cancelled
                    | ShellOutcome::Failed { .. } => Exit::Failed,
                });
            }
            _ => {}
        }
    }
}

/// A remote exit code as a process exit — `ssh` semantics: the status is
/// the remote command's, verbatim.
pub(crate) fn exit_code(code: i32) -> Exit {
    match u8::try_from(code) {
        Ok(0) => Exit::Ok,
        Ok(code) => Exit::Remote(code),
        Err(_) => Exit::Failed,
    }
}

/// `session events` — the recorded stream, then the live tail under
/// `--follow`.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) on usage errors, when a request fails,
/// or when the answer does not decode.
pub async fn events(api: &Api, id: &str, after: u64, follow: bool) -> Outcome<()> {
    let session: SessionId = id
        .parse()
        .map_err(|_| Failure::usage(format!("`{id}` is not a session id")))?;
    let mut cursor = after;
    loop {
        let page = crate::follow::events(api, session, cursor).await?;
        for stored in &page.events {
            out::emit_line(stored)?;
            cursor = stored.seq;
        }
        if !page.more {
            break;
        }
    }
    if !follow {
        return Ok(());
    }
    let mut tail = Follow::new(api, session).after(cursor);
    loop {
        let item = tail.next().await?;
        match item {
            Item::Live(envelope) => out::emit_line(&envelope)?,
            Item::Replayed(stored) => out::emit_line(&stored)?,
        }
    }
}

/// `session wait` — block until one of the named conditions occurs.
///
/// Answers the matched condition's name; the exit code carries the same
/// fact to a script.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) on usage errors, when a request fails,
/// or when the answer does not decode.
pub async fn wait(
    api: &Api,
    id: &str,
    conditions: &[String],
    timeout: Option<core::time::Duration>,
    mode: out::Mode,
) -> Outcome<Exit> {
    let session: SessionId = id
        .parse()
        .map_err(|_| Failure::usage(format!("`{id}` is not a session id")))?;
    let wanted: Vec<Condition> = conditions
        .iter()
        .map(|name| Condition::parse(name))
        .collect::<Result<_, _>>()?;

    // A condition the session already satisfies wins without a stream:
    // `wait --for paused` on a paused session is `paused`, immediately.
    let detail: SessionDetail = api.get(&format!("/v1/sessions/{id}")).await?;
    if let Some(condition) = wanted.iter().find(|c| c.met_by(&detail)) {
        return matched(*condition, mode).map(|()| condition.exit());
    }
    if wanted.contains(&Condition::Approval) {
        let pending: Vec<ApprovalView> = api
            .get(&format!("/v1/approvals?session={id}&state=pending"))
            .await?;
        if !pending.is_empty() {
            return matched(Condition::Approval, mode).map(|()| Condition::Approval.exit());
        }
    }

    let mut follow = Follow::new(api, session);
    let watch = async {
        loop {
            let item = follow.next().await?;
            let Some(event) = item.event() else { continue };
            if let Some(condition) = wanted.iter().find(|c| c.met_by_event(&event)) {
                break Ok(*condition);
            }
        }
    };
    let condition = match timeout {
        Some(limit) => match tokio::time::timeout(limit, watch).await {
            Ok(condition) => condition?,
            Err(_elapsed) => {
                return Err(Failure::problem(
                    Exit::Timeout,
                    format!("no {conditions:?} within {}", humantime(limit)),
                ));
            }
        },
        None => watch.await?,
    };
    matched(condition, mode).map(|()| condition.exit())
}

/// One of the `--for` conditions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Condition {
    /// `SessionStateChanged{Active}` — the session is on a live machine.
    Active,
    /// A turn boundary: `TurnCompleted` means the agent is waiting.
    Idle,
    /// `ApprovalPending` — the agent needs a decision.
    Approval,
    /// `SessionStateChanged{Paused}`.
    Paused,
    /// The machine went away — `Interrupted`, or the daemon disconnected.
    Stopped,
    /// `SessionStateChanged{Failed}`, or a turn that failed outright.
    Failed,
    /// `SessionStateChanged{Archived}`.
    Archived,
}

impl Condition {
    fn parse(name: &str) -> Result<Self, Failure> {
        match name {
            "active" => Ok(Self::Active),
            "idle" => Ok(Self::Idle),
            "approval" => Ok(Self::Approval),
            "paused" => Ok(Self::Paused),
            "stopped" => Ok(Self::Stopped),
            "failed" => Ok(Self::Failed),
            "archived" => Ok(Self::Archived),
            other => Err(Failure::usage(format!(
                "`{other}` is not a condition — active, idle, approval, paused, stopped, failed, archived"
            ))),
        }
    }

    /// Whether the session's current row already satisfies it.
    pub(crate) fn met_by(self, detail: &SessionDetail) -> bool {
        use flyco_core::SessionState as S;
        match self {
            Self::Active => detail.summary.state == S::Active,
            Self::Idle => {
                matches!(detail.summary.state, S::Active)
                    && matches!(
                        detail.summary.activity,
                        flyco_core::SessionActivity::Idle | flyco_core::SessionActivity::NeedsInput
                    )
            }
            Self::Paused => detail.summary.state == S::Paused,
            Self::Stopped => detail.summary.state == S::Interrupted,
            Self::Failed => detail.summary.state == S::Failed,
            Self::Archived => detail.summary.state == S::Archived,
            // A pending approval is not on the session row — it is its own
            // table. `wait` reads it separately before following.
            Self::Approval => false,
        }
    }

    /// Whether a live event realizes it.
    pub(crate) const fn met_by_event(self, event: &ClientEvent) -> bool {
        use flyco_core::SessionState as S;
        match self {
            Self::Active => matches!(event, ClientEvent::SessionStateChanged { state: S::Active }),
            Self::Idle => matches!(
                event,
                ClientEvent::Harness {
                    event: flyco_core::HarnessEvent::TurnCompleted { .. }
                }
            ),
            Self::Approval => matches!(event, ClientEvent::ApprovalPending { .. }),
            Self::Paused => matches!(event, ClientEvent::SessionStateChanged { state: S::Paused }),
            Self::Stopped => matches!(
                event,
                ClientEvent::SessionStateChanged {
                    state: S::Interrupted
                } | ClientEvent::MachineConnection { connected: false }
            ),
            Self::Failed => matches!(
                event,
                ClientEvent::SessionStateChanged { state: S::Failed }
                    | ClientEvent::Harness {
                        event: flyco_core::HarnessEvent::TurnFailed { .. }
                    }
            ),
            Self::Archived => matches!(
                event,
                ClientEvent::SessionStateChanged { state: S::Archived }
            ),
        }
    }

    /// The name a match reports.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Idle => "idle",
            Self::Approval => "approval",
            Self::Paused => "paused",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
            Self::Archived => "archived",
        }
    }

    /// The code a match exits with.
    #[must_use]
    pub const fn exit(self) -> Exit {
        match self {
            Self::Active | Self::Idle | Self::Archived => Exit::Ok,
            Self::Approval => Exit::ApprovalPending,
            Self::Paused => Exit::Paused,
            Self::Stopped | Self::Failed => Exit::Failed,
        }
    }
}

fn matched(condition: Condition, mode: out::Mode) -> Outcome<()> {
    match mode {
        out::Mode::Json => out::emit(&serde_json::json!({
            "condition": condition.name(),
        })),
        out::Mode::Human => out::print(condition.name()),
    }
}

/// `session set` — `PATCH /v1/sessions/{id}`.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) on usage errors, when a request fails,
/// or when the answer does not decode.
pub async fn set(api: &Api, id: &str, body: UpdateSession, mode: out::Mode) -> Outcome<()> {
    if body == UpdateSession::default() {
        return Err(Failure::usage(
            "`set` needs at least one of --title, --budget, --model, --permission-mode",
        ));
    }
    let session: SessionDetail = api.patch(&format!("/v1/sessions/{id}"), &body).await?;
    match mode {
        out::Mode::Json => out::emit(&session),
        out::Mode::Human => out::print(&format!(
            "{}  {}",
            session.summary.id, session.summary.title
        )),
    }
}

/// `session stop` — `POST …/machine/stop`.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) on usage errors, when a request fails,
/// or when the answer does not decode.
pub async fn stop(api: &Api, id: &str) -> Outcome<()> {
    api.post_empty(&format!("/v1/sessions/{id}/machine/stop"))
        .await
}

/// `session resume` — `POST …/resume`; the session is re-provisioned.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) on usage errors, when a request fails,
/// or when the answer does not decode.
pub async fn resume(api: &Api, id: &str, mode: out::Mode) -> Outcome<SessionDetail> {
    let session: SessionDetail = api
        .post::<serde_json::Value, SessionDetail>(
            &format!("/v1/sessions/{id}/resume"),
            &serde_json::json!({}),
        )
        .await?;
    match mode {
        out::Mode::Json => out::emit(&session)?,
        out::Mode::Human => {
            out::print(&format!(
                "{}  {}",
                session.summary.id,
                state_label(&session.summary)
            ))?;
        }
    }
    Ok(session)
}

/// `session interrupt` — `POST …/interrupt`.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) on usage errors, when a request fails,
/// or when the answer does not decode.
pub async fn interrupt(api: &Api, id: &str) -> Outcome<()> {
    api.post_empty(&format!("/v1/sessions/{id}/interrupt"))
        .await
}

/// `session archive` — `POST …/archive`.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) on usage errors, when a request fails,
/// or when the answer does not decode.
pub async fn archive(
    api: &Api,
    id: &str,
    discard_uncommitted: bool,
    mode: out::Mode,
) -> Outcome<()> {
    let path = if discard_uncommitted {
        format!("/v1/sessions/{id}/archive?discard_uncommitted=true")
    } else {
        format!("/v1/sessions/{id}/archive")
    };
    let session: SessionDetail = api
        .post::<serde_json::Value, SessionDetail>(&path, &serde_json::json!({}))
        .await?;
    match mode {
        out::Mode::Json => out::emit(&session),
        out::Mode::Human => out::print(&format!("{}  archived", session.summary.id)),
    }
}

/// `session approvals` — `GET /v1/approvals?session=`.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) on usage errors, when a request fails,
/// or when the answer does not decode.
pub async fn approvals(api: &Api, id: &str, pending: bool, mode: out::Mode) -> Outcome<()> {
    let mut path = format!("/v1/approvals?session={id}");
    if pending {
        path.push_str("&state=pending");
    }
    let approvals: Vec<ApprovalView> = api.get(&path).await?;
    match mode {
        out::Mode::Json => out::emit(&approvals),
        out::Mode::Human => {
            let rows: Vec<Vec<String>> = approvals
                .iter()
                .map(|approval| {
                    vec![
                        approval.id.to_string(),
                        format!("{:?}", approval.state).to_lowercase(),
                        approval_summary(approval),
                    ]
                })
                .collect();
            out::print(&out::table(&["ID", "STATE", "WHAT"], &rows))
        }
    }
}

/// `session approve` — `POST /v1/approvals/{id}/decision`.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) on usage errors, when a request fails,
/// or when the answer does not decode.
pub async fn approve(api: &Api, approval: &str, allow: bool, mode: out::Mode) -> Outcome<()> {
    let decision = if allow {
        flyco_core::wire::ApprovalDecision::Approved
    } else {
        flyco_core::wire::ApprovalDecision::Denied
    };
    let decided: ApprovalView = api
        .post(
            &format!("/v1/approvals/{approval}/decision"),
            &DecideApproval { decision },
        )
        .await?;
    match mode {
        out::Mode::Json => out::emit(&decided),
        out::Mode::Human => out::print(&format!("{}  {:?}", decided.id, decided.state)),
    }
}

/// `session diff` — `GET …/diff`.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) on usage errors, when a request fails,
/// or when the answer does not decode.
pub async fn diff(api: &Api, id: &str, mode: out::Mode) -> Outcome<()> {
    let diff: WorkdirDiff = api.get(&format!("/v1/sessions/{id}/diff")).await?;
    match mode {
        out::Mode::Json => out::emit(&diff),
        out::Mode::Human => {
            let mut text = String::new();
            for file in &diff.files {
                if let Some(patch) = &file.patch {
                    text.push_str(patch);
                    if !patch.ends_with('\n') {
                        text.push('\n');
                    }
                }
            }
            if diff.truncated {
                text.push_str("(diff truncated for size)\n");
            }
            out::print(text.trim_end_matches('\n'))
        }
    }
}

/// `session files` — `GET …/files?path=`.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) on usage errors, when a request fails,
/// or when the answer does not decode.
pub async fn files(api: &Api, id: &str, path: Option<&str>, mode: out::Mode) -> Outcome<()> {
    let listing: DirectoryListing = api
        .get(&format!(
            "/v1/sessions/{id}/files?path={}",
            urlencoding(path.unwrap_or_default())
        ))
        .await?;
    match mode {
        out::Mode::Json => out::emit(&listing),
        out::Mode::Human => {
            let rows: Vec<Vec<String>> = listing
                .entries
                .iter()
                .map(|entry| {
                    vec![
                        match entry.kind {
                            flyco_core::EntryKind::Directory => "dir".to_owned(),
                            flyco_core::EntryKind::File => "file".to_owned(),
                        },
                        entry
                            .size_bytes
                            .map_or_else(|| "-".to_owned(), |size| size.to_string()),
                        entry.name.clone(),
                    ]
                })
                .collect();
            out::print(&out::table(&["KIND", "SIZE", "NAME"], &rows))
        }
    }
}

/// `session read` — `GET …/files/content?path=`.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) on usage errors, when a request fails,
/// or when the answer does not decode.
pub async fn read(api: &Api, id: &str, path: &str, mode: out::Mode) -> Outcome<()> {
    let file: FileContent = api
        .get(&format!(
            "/v1/sessions/{id}/files/content?path={}",
            urlencoding(path)
        ))
        .await?;
    match mode {
        out::Mode::Json => out::emit(&file),
        out::Mode::Human => out::raw_out(file.text.as_bytes()),
    }
}

/// `session env` — `GET …/env`, or `PUT` after `--set`.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) on usage errors, when a request fails,
/// or when the answer does not decode.
pub async fn env(api: &Api, id: &str, set: &[String], mode: out::Mode) -> Outcome<()> {
    if !set.is_empty() {
        let mut current: EnvDocument = api.get(&format!("/v1/sessions/{id}/env")).await?;
        merge_pairs(&mut current, set)?;
        let updated: EnvDocument = api
            .put(
                &format!("/v1/sessions/{id}/env"),
                &UpdateEnv {
                    entries: current.entries,
                },
            )
            .await?;
        return match mode {
            out::Mode::Json => out::emit(&updated),
            out::Mode::Human => out::print(&format!("{} variables set", updated.entries.len())),
        };
    }
    let document: EnvDocument = api.get(&format!("/v1/sessions/{id}/env")).await?;
    match mode {
        out::Mode::Json => out::emit(&document),
        out::Mode::Human => {
            let rows: Vec<Vec<String>> = document
                .entries
                .iter()
                .map(|entry| vec![entry.key.clone(), entry.value.clone()])
                .collect();
            out::print(&out::table(&["KEY", "VALUE"], &rows))
        }
    }
}

/// `--env KEY=VALUE` pairs onto the session, when any were given.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) on usage errors, when a request fails,
/// or when the answer does not decode.
pub async fn set_env(api: &Api, id: &SessionId, env: &[String]) -> Outcome<()> {
    if env.is_empty() {
        return Ok(());
    }
    let mut document: EnvDocument = api.get(&format!("/v1/sessions/{id}/env")).await?;
    merge_pairs(&mut document, env)?;
    api.put::<UpdateEnv, serde_json::Value>(
        &format!("/v1/sessions/{id}/env"),
        &UpdateEnv {
            entries: document.entries,
        },
    )
    .await?;
    Ok(())
}

/// `KEY=VALUE` pairs merged into an env document, in place.
fn merge_pairs(document: &mut EnvDocument, pairs: &[String]) -> Outcome<()> {
    for pair in pairs {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| Failure::usage(format!("`{pair}` is not KEY=VALUE")))?;
        match document.entries.iter_mut().find(|entry| entry.key == key) {
            Some(entry) => value.clone_into(&mut entry.value),
            None => document.entries.push(EnvEntry {
                key: key.to_owned(),
                value: value.to_owned(),
            }),
        }
    }
    Ok(())
}

/// A `K=V` string into URL-safe query text.
fn urlencoding(text: &str) -> String {
    text.replace('%', "%25")
        .replace('&', "%26")
        .replace('=', "%3D")
        .replace(' ', "%20")
        .replace('?', "%3F")
        .replace('#', "%23")
}

/// The approval's payload, as one line.
fn approval_summary(approval: &ApprovalView) -> String {
    serde_json::to_string(&approval.payload).unwrap_or_default()
}

/// The spec's prompt flags resolved, when either was given.
///
/// `None` means "ask": the human path's picker asks, the agent path's
/// caller reads piped stdin or fails usage — which of those is the
/// caller's context to know.
#[must_use]
pub fn resolve_prompt(spec: &SessionSpec) -> Option<Outcome<String>> {
    if let Some(prompt) = &spec.prompt {
        return Some(Ok(prompt.clone()));
    }
    if let Some(path) = &spec.prompt_file {
        return Some(
            std::fs::read_to_string(path)
                .map_err(|error| Failure::usage(format!("cannot read {path}: {error}"))),
        );
    }
    None
}

/// What `-m`/`-f`/`-`/piped-stdin resolve to.
fn resolve_text(
    text: Option<String>,
    file: Option<String>,
    stdin_flag: bool,
    naming: &str,
) -> Outcome<String> {
    // `-` anywhere a text could come from means stdin — the same convention
    // `ssh`/`cat` use — so it forces the read even on a TTY.
    let stdin_named = text.as_deref() == Some("-") || file.as_deref() == Some("-");
    if let Some(text) = text
        && !stdin_named
    {
        return Ok(text);
    }
    if let Some(path) = file
        && !stdin_named
    {
        return std::fs::read_to_string(&path)
            .map_err(|error| Failure::usage(format!("cannot read {path}: {error}")));
    }
    if stdin_flag || stdin_named || !std::io::stdin().is_terminal() {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|error| Failure::usage(format!("cannot read stdin: {error}")))?;
        return Ok(text);
    }
    Err(Failure::usage(format!("a message needs {naming}")))
}

/// A duration as prose, for the timeout error.
fn humantime(duration: core::time::Duration) -> String {
    let seconds = duration.as_secs();
    match seconds {
        s if s >= 3600 => format!("{}h{}m", s / 3600, s % 3600 / 60),
        s if s >= 60 => format!("{}m{}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}
