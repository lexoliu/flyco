//! The human path: `flyco claude`, `flyco codex`, `flyco resume`, and the
//! bare command's harness picker.
//!
//! One orchestration for all of it: get the session onto a live machine,
//! ask for its harness TUI, and hand the terminal to [`crate::bridge`].
//! What differs is only how the session is named — created from a picker
//! chain for `claude`/`codex`, named by id or picked for `resume`.
//!
//! The path is TTY-only by definition: the bridge puts the local terminal
//! in raw mode, so a pipe has nothing to bridge. The agent surface —
//! `flyco run`, `flyco session …` — is the non-interactive one.

use std::io::IsTerminal as _;

use flyco_core::wire::ClientEvent;
use flyco_core::{
    CreateSession, HarnessKind, HarnessTui, MachineCatalog, MachineChoice, MachineDefault,
    RepoSummary, SessionDetail, SessionState, SessionSummary,
};

use crate::bridge::{self, Ended};
use crate::cli::SessionSpec;
use crate::client::Api;
use crate::follow::Follow;
use crate::session::exit_code;
use crate::{Exit, Failure, Outcome, out, pick};

/// `flyco claude` / `flyco codex`: create a session and bridge into its
/// harness TUI.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when a request fails, the session cannot
/// reach a machine, or the bridge fails.
pub async fn launch(
    api: &Api,
    harness: HarnessKind,
    repo_arg: Option<String>,
    spec: &SessionSpec,
    mode: out::Mode,
) -> Outcome<Exit> {
    require_terminal()?;
    let (request, env) = resolve(api, harness, repo_arg, spec).await?;
    let headers: &[(&str, &str)] = match &spec.idempotency_key {
        Some(key) => &[("Idempotency-Key", key.as_str())],
        None => &[],
    };
    let session: SessionDetail = api
        .post_with_headers("/v1/sessions", &request, headers)
        .await?;
    let id = session.summary.id;
    crate::session::set_env(api, &id, &env).await?;
    note(mode, &format!("session {id} — provisioning its machine"));
    attach(api, id, HarnessTui { resume: false }, mode).await
}

/// `flyco resume [<id>|--last]`: re-enter a session's TUI, putting it back
/// on a machine first when it was off one.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when a request fails, the session cannot
/// reach a machine, or the bridge fails.
pub async fn resume(api: &Api, id: Option<String>, last: bool, mode: out::Mode) -> Outcome<Exit> {
    require_terminal()?;
    let session = find_session(api, id, last).await?;
    match session.state {
        // Off its machine — interrupted, failed, archived: the resume
        // route rebuilds it through provisioning.
        state if state.is_resumable() => {
            note(mode, "putting the session back on a machine");
            crate::session::resume(api, &session.id.to_string(), mode).await?;
        }
        SessionState::Paused => {
            return Err(Failure::problem(
                Exit::Paused,
                "the session is paused — resolve the pause, then resume",
            ));
        }
        _ => {}
    }
    attach(api, session.id, HarnessTui { resume: true }, mode).await
}

/// Bare `flyco`: the harness picker, then the launch flow.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when a request fails, the session cannot
/// reach a machine, or the bridge fails.
pub async fn pick_harness(api: &Api, mode: out::Mode) -> Outcome<Exit> {
    require_terminal()?;
    let harnesses = [HarnessKind::ClaudeCode, HarnessKind::Codex];
    let chosen = pick::pick("Harness", &harnesses, |harness| match harness {
        HarnessKind::ClaudeCode => "Claude Code".to_owned(),
        HarnessKind::Codex => "Codex".to_owned(),
    })?;
    launch(api, harnesses[chosen], None, &SessionSpec::default(), mode).await
}

/// A TUI command with no terminal is a category error, not a missing
/// flag: point at the pipe surface rather than asking a pipe questions.
fn require_terminal() -> Outcome<()> {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        return Ok(());
    }
    Err(Failure::usage(
        "the TUI bridge needs a terminal — `flyco run` and `flyco session …` are the non-interactive surface",
    ))
}

/// The session `resume` names: explicit id, `--last`, or a picker over
/// the account's sessions.
async fn find_session(api: &Api, id: Option<String>, last: bool) -> Outcome<SessionSummary> {
    let sessions: Vec<SessionSummary> = api.get("/v1/sessions").await?;
    if let Some(id) = id {
        return sessions
            .into_iter()
            .find(|session| session.id.to_string() == id)
            .ok_or_else(|| Failure::problem(Exit::NotFoundOrConflict, format!("no session {id}")));
    }
    let resumable: Vec<SessionSummary> = sessions
        .into_iter()
        .filter(|session| session.state != SessionState::Archived)
        .collect();
    if resumable.is_empty() {
        return Err(Failure::problem(
            Exit::NotFoundOrConflict,
            "no sessions to resume",
        ));
    }
    if last {
        // The list answers newest-first.
        return Ok(resumable.into_iter().next().expect("non-empty"));
    }
    let chosen = pick::pick("Resume which session", &resumable, |session| {
        format!("{}  {}  {}", state_of(session), session.repo, session.title)
    })?;
    Ok(resumable.into_iter().nth(chosen).expect("picked in range"))
}

/// A session's state as the picker shows it.
fn state_of(session: &SessionSummary) -> String {
    serde_json::to_value(session.state)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The picker chain for a new session — every step a flag can bypass.
///
/// Order is the issue's: repo → branch → machine → spot → disk → budget →
/// env → the goal. Each answer comes from `spec` when the flag was given
/// and is asked for otherwise; nothing here ever runs off a TTY.
async fn resolve(
    api: &Api,
    harness: HarnessKind,
    repo_arg: Option<String>,
    spec: &SessionSpec,
) -> Outcome<(CreateSession, Vec<String>)> {
    let repo = match repo_arg.or_else(|| spec.repo.clone()) {
        Some(repo) => repo,
        None => pick_repo(api).await?,
    };
    let branch = match &spec.branch {
        Some(branch) => Some(branch.clone()),
        None => pick_branch(api, &repo).await?,
    };
    let machine = match spec_machine(spec)? {
        Some(machine) => Some(machine),
        None => Some(pick_machine(api, spec).await?),
    };
    let budget_limit = if let Some(budget) = spec.budget {
        budget
    } else {
        let text = pick::input("Budget for the session, in dollars", Some("10"))?;
        crate::cli::parse_usd(&text).map_err(Failure::usage)?
    };
    let mut env = spec.env.clone();
    if env.is_empty() {
        let text = pick::input(
            "Extra env (KEY=VALUE, comma-separated; empty for none)",
            None,
        )?;
        if !text.trim().is_empty() {
            env = text.split(',').map(|pair| pair.trim().to_owned()).collect();
        }
    }
    let prompt = match crate::session::resolve_prompt(spec) {
        Some(prompt) => prompt?,
        None => pick::input("What should it do", None)?,
    };
    Ok((
        CreateSession {
            prompt,
            harness,
            repo,
            branch,
            budget_limit,
            machine,
            spot: !spec.on_demand,
            model: spec.model.clone().map(|model| flyco_core::ModelChoice {
                model,
                effort: spec.effort.clone(),
            }),
            permission_mode: spec.permission_mode,
        },
        env,
    ))
}

/// `spec`'s machine flags as a `MachineChoice`, when all three were given.
fn spec_machine(spec: &SessionSpec) -> Outcome<Option<MachineChoice>> {
    match (&spec.machine, &spec.account, &spec.region) {
        (Some(machine_type), Some(account), Some(region)) => Ok(Some(MachineChoice {
            provider_account: account
                .parse()
                .map_err(|_| Failure::usage("`--account` is not an account id"))?,
            machine_type: machine_type.clone(),
            runtime: flyco_core::Runtime::Vm,
            region: region.clone(),
            spot: !spec.on_demand,
            disk_gib: spec.disk.unwrap_or(flyco_core::DEFAULT_DISK_GIB),
        })),
        (None, None, None) => Ok(None),
        _ => unreachable!("clap's requires= rules out a partial machine choice"),
    }
}

/// Repo picker over `GET /v1/github/repos`.
async fn pick_repo(api: &Api) -> Outcome<String> {
    let repos: Vec<RepoSummary> = api.get("/v1/github/repos").await?;
    if repos.is_empty() {
        return Err(Failure::problem(
            Exit::Problem,
            "no repositories — link a GitHub account in flyco first",
        ));
    }
    let chosen = pick::pick("Repository", &repos, |repo| {
        format!(
            "{}{}{}",
            repo.slug,
            if repo.private { " (private)" } else { "" },
            repo.description
                .as_ref()
                .map_or(String::new(), |d| format!(" — {d}")),
        )
    })?;
    Ok(repos[chosen].slug.to_string())
}

/// Branch picker; the repository's default is the preselected row.
///
/// `None` means "the default" — the request omits the branch and the
/// control plane resolves it, so a picker answer of "the usual one" never
/// freezes a name the repo might change.
async fn pick_branch(api: &Api, repo: &str) -> Outcome<Option<String>> {
    let page: flyco_core::BranchPage = api
        .get(&format!("/v1/github/repos/{repo}/branches"))
        .await?;
    if page.branches.is_empty() {
        return Ok(None);
    }
    // "(default)" first: choosing it omits the branch rather than naming
    // it, so a session follows the repo's default forever.
    let mut names: Vec<String> = vec!["(default)".to_owned()];
    names.extend(page.branches.iter().map(|branch| branch.name.to_string()));
    let chosen = pick::pick("Branch", &names, std::clone::Clone::clone)?;
    Ok((chosen > 0).then(|| names[chosen].clone()))
}

/// Machine picker over `GET /v1/machines/catalog`, with
/// `GET /v1/machines/default`'s answer preselected.
///
/// Also answers the spot and disk questions, since they belong to the
/// machine choice: spot only exists where the entry offers it, and the
/// disk default is the platform's.
async fn pick_machine(api: &Api, spec: &SessionSpec) -> Outcome<MachineChoice> {
    let catalog: MachineCatalog = api.get("/v1/machines/catalog").await?;
    let default: Option<MachineDefault> = api.get("/v1/machines/default").await.ok();
    let entries: Vec<&flyco_core::MachineCatalogEntry> = catalog
        .entries
        .iter()
        .filter(|entry| entry.account.is_some())
        .collect();
    if entries.is_empty() {
        return Err(Failure::problem(
            Exit::Problem,
            "no machines in the catalog — link a provider account in flyco first",
        ));
    }
    // The suggested machine first, so Enter does the right thing.
    let is_default = |entry: &flyco_core::MachineCatalogEntry| {
        default.as_ref().is_some_and(|d| {
            entry.account == Some(d.choice.provider_account)
                && entry.machine_type == d.choice.machine_type
                && entry.region == d.choice.region
        })
    };
    let mut ordered: Vec<&flyco_core::MachineCatalogEntry> = Vec::new();
    ordered.extend(entries.iter().copied().filter(|entry| is_default(entry)));
    ordered.extend(entries.iter().copied().filter(|entry| !is_default(entry)));
    let chosen = pick::pick("Machine", &ordered, |entry| {
        let size = entry.capacity.as_ref().map_or_else(
            || "?".to_owned(),
            |capacity| format!("{}c/{}GiB", capacity.vcpus, capacity.memory_mib / 1024),
        );
        let price = crate::discover::pricing(entry);
        format!(
            "{} · {} · {} · {}",
            entry.machine_type, entry.region, size, price
        )
    })?;
    let entry = ordered[chosen];

    let spot = if spec.spot {
        true
    } else if spec.on_demand {
        false
    } else if entry.pricing.offers_spot() {
        pick::confirm("Spot capacity (interruptible, cheaper)", true)?
    } else {
        false
    };
    let disk_gib = if let Some(disk) = spec.disk {
        disk
    } else {
        let text = pick::input("Disk, GiB", Some(&flyco_core::DEFAULT_DISK_GIB.to_string()))?;
        text.trim()
            .parse::<u32>()
            .map_err(|_| Failure::usage(format!("`{text}` is not a disk size")))?
    };
    Ok(MachineChoice {
        provider_account: entry.account.expect("filtered to named accounts"),
        machine_type: entry.machine_type.clone(),
        runtime: entry.runtime,
        region: entry.region.clone(),
        spot,
        disk_gib,
    })
}

/// Get the session onto a machine, put its harness TUI in the foreground,
/// and bridge the local terminal to it.
///
/// The stream opens before anything is posted so nothing the daemon emits
/// in answer can race past it. The launch itself needs no wait: a command
/// is a row the daemon's stream replays, so a machine still provisioning
/// takes it on first attach and one already up takes it now — `ensure`
/// semantics make a re-post harmless when the TUI already runs.
async fn attach(
    api: &Api,
    id: flyco_core::SessionId,
    tui: HarnessTui,
    mode: out::Mode,
) -> Outcome<Exit> {
    let mut follow = Follow::new(api, id);

    // Commands are refused on anything but `Active` — a provisioning
    // session has no daemon to take them — so the launch waits for the
    // state a daemon attach flips. An already-live session checks the row
    // rather than waiting for an event that already happened.
    let detail: SessionDetail = api.get(&format!("/v1/sessions/{id}")).await?;
    if detail.summary.state != SessionState::Active {
        loop {
            let item = follow.next().await?;
            let Some(event) = item.event() else { continue };
            match event {
                ClientEvent::SessionStateChanged {
                    state: SessionState::Active,
                } => break,
                ClientEvent::SessionStateChanged {
                    state: SessionState::Failed | SessionState::Archived,
                } => {
                    return Err(Failure::problem(
                        Exit::Failed,
                        "the session failed — it cannot take a terminal",
                    ));
                }
                ClientEvent::SessionStateChanged {
                    state: SessionState::Paused,
                } => {
                    return Err(Failure::problem(
                        Exit::Paused,
                        "the session paused — resolve the pause, then resume",
                    ));
                }
                ClientEvent::ProvisioningStage { stage, .. } => {
                    let label = serde_json::to_value(stage)
                        .ok()
                        .and_then(|value| value.as_str().map(str::to_owned))
                        .unwrap_or_else(|| format!("{stage:?}"));
                    note(mode, &format!("provisioning: {label}"));
                }
                ClientEvent::MachineConnection { connected: false } => {
                    note(mode, "machine disconnected — waiting for it back");
                }
                _ => {}
            }
        }
    }

    api.post::<HarnessTui, serde_json::Value>(&format!("/v1/sessions/{id}/terminal/harness"), &tui)
        .await?;
    // A resize is a repaint: a TUI already in the foreground answers it
    // with a full screen, which is how an attach to a live-but-quiet
    // session learns it is attached rather than hanging on output that was
    // never coming.
    bridge::send_resize(api, id).await?;
    until_output(&mut follow, mode).await?;

    match bridge::run(api, id, &mut follow).await? {
        Ended::Exited(code) => {
            note(mode, &format!("harness exited ({})", code_label(code)));
            Ok(code.map_or(Exit::Ok, exit_code))
        }
        Ended::Detached => {
            note(mode, &format!("detached — `flyco resume {id}` re-enters"));
            Ok(Exit::Ok)
        }
        Ended::InputEnded => Ok(Exit::Ok),
    }
}

/// Waits for the pane's first byte — the TUI is up and its screen is
/// coming — or for the session to leave the states it can be waited in.
async fn until_output(follow: &mut Follow<'_>, mode: out::Mode) -> Outcome<()> {
    loop {
        let item = follow.next().await?;
        let Some(event) = item.event() else { continue };
        match event {
            ClientEvent::TerminalOutput { .. } => return Ok(()),
            ClientEvent::TerminalExited { code } => {
                return Err(Failure::problem(
                    Exit::Failed,
                    format!(
                        "the harness exited before the bridge opened ({})",
                        code_label(code)
                    ),
                ));
            }
            ClientEvent::SessionStateChanged {
                state: SessionState::Failed | SessionState::Archived,
            } => {
                return Err(Failure::problem(
                    Exit::Failed,
                    "the session failed — it cannot take a terminal",
                ));
            }
            ClientEvent::SessionStateChanged {
                state: SessionState::Interrupted,
            } => {
                note(mode, "the machine was reclaimed — waiting for it back");
            }
            ClientEvent::SessionStateChanged {
                state: SessionState::Paused,
            } => {
                return Err(Failure::problem(
                    Exit::Paused,
                    "the session paused — resolve the pause, then resume",
                ));
            }
            ClientEvent::MachineConnection { connected: false } => {
                note(mode, "machine disconnected — waiting for it back");
            }
            _ => {}
        }
    }
}

/// An exit code as prose, for the one place a number alone would read as
/// noise.
fn code_label(code: Option<i32>) -> String {
    code.map_or_else(|| "no status".to_owned(), |code| format!("code {code}"))
}

/// A progress line on stderr — the TUI owns stdout once it starts, and
/// JSON mode keeps stdout for documents alone.
fn note(mode: out::Mode, text: &str) {
    if mode == out::Mode::Human {
        let _ = out::raw_err(format!("{text}\n").as_bytes());
    }
}
