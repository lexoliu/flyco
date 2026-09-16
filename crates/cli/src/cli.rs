//! The `flyco` command line, as `clap` sees it.
//!
//! Two surfaces share the one binary: the human path (`claude`, `codex`,
//! `resume`, and the bare command's harness picker) and the agent path —
//! `session …`, `run`, and the discovery commands, which are a faithful
//! projection of `/v1` routes onto flags.

use clap::{Args, Parser, Subcommand};

/// `flyco` — run Claude Code, Codex and Devin sessions on flyco from a terminal.
#[derive(Debug, Parser)]
#[command(name = "flyco", version, about)]
pub struct Cli {
    /// Emit API documents rather than human renderings. Forced whenever
    /// stdout is not a terminal — an agent on a pipe never parses a table.
    #[arg(long, global = true)]
    pub json: bool,

    /// Answer confirmations without asking. Meaningless off a TTY, where
    /// nothing is ever asked.
    #[arg(long, short = 'y', global = true)]
    pub yes: bool,

    /// The command to run; bare `flyco` is the human harness picker.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// The top-level commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Sign this machine in: open the approval page in a browser, wait for
    /// the user to approve it there, and store the issued `fk_` key.
    Login {
        /// Store an existing `fk_` key instead of running the browser
        /// approval flow — the headless path.
        #[arg(long, value_name = "KEY")]
        token: Option<String>,
    },
    /// Sign out: revoke the stored API key and remove the credentials file.
    Logout,
    /// Who the stored credential authenticates as.
    Auth {
        /// The auth subcommand.
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// The machine catalog every linked provider offers.
    Catalog,
    /// Repositories the linked GitHub account can open sessions on.
    Repos,
    /// A repository's branches.
    Branches {
        /// `owner/name`.
        repo: String,
    },
    /// Linked harness accounts and the verified feature matrix.
    Harnesses,
    /// Everything about one session, or the list of them.
    Session {
        /// The session subcommand.
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// One prompt, one session, one turn: create the session, stream its
    /// events as JSONL, and end when the turn does.
    Run {
        /// Which harness drives the session.
        #[arg(long, value_parser = parse_harness)]
        harness: flyco_core::HarnessKind,
        /// The session's settings.
        #[command(flatten)]
        spec: SessionSpec,
        /// Report the session id and return at once, without watching it.
        #[arg(long)]
        detach: bool,
        /// Stop the session's machine when the turn ends.
        #[arg(long, conflicts_with = "archive")]
        stop: bool,
        /// Archive the session when the turn ends.
        #[arg(long)]
        archive: bool,
        /// Give up waiting after this long (`300`, `5m`, `1h30m`).
        #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
        timeout: Option<std::time::Duration>,
    },
    /// Open a Claude Code TUI on a new session, bridged to this terminal.
    Claude {
        /// `owner/name`; asked for when omitted on a TTY.
        repo: Option<String>,
        /// The session's settings.
        #[command(flatten)]
        spec: SessionSpec,
    },
    /// Open a Codex TUI on a new session, bridged to this terminal.
    Codex {
        /// `owner/name`; asked for when omitted on a TTY.
        repo: Option<String>,
        /// The session's settings.
        #[command(flatten)]
        spec: SessionSpec,
    },
    /// Open a Devin TUI on a new session, bridged to this terminal.
    Devin {
        /// `owner/name`; asked for when omitted on a TTY.
        repo: Option<String>,
        /// The session's settings.
        #[command(flatten)]
        spec: SessionSpec,
    },
    /// Hand a local harness session off to a fresh cloud session: the
    /// tracked working tree goes as a patch, the session's own summary as
    /// the brief, and the full transcript as a file the cloud agent can
    /// consult. Not a resume — the cloud session is a new conversation
    /// that knows it moved machines.
    Handoff {
        /// Only look at this harness's local sessions; the default
        /// considers all three.
        #[arg(long, value_parser = parse_harness)]
        from: Option<flyco_core::HarnessKind>,
        /// The local session to send — Claude's session UUID, a Codex
        /// thread id, a Devin session name. One candidate is taken
        /// without asking; several on a TTY are picked, on a pipe this is
        /// required.
        #[arg(long)]
        session: Option<String>,
        /// Which harness drives the cloud session (default: the
        /// source's own).
        #[arg(long, value_parser = parse_harness)]
        harness: Option<flyco_core::HarnessKind>,
        /// An extra instruction appended to the handoff brief.
        #[arg(long, short = 'm')]
        message: Option<String>,
        /// Skip asking the session to summarize itself; the transcript
        /// alone carries the context.
        #[arg(long)]
        no_summary: bool,
        /// Include untracked (but not ignored) files in the patch.
        #[arg(long)]
        include_untracked: bool,
        /// The session's settings.
        #[command(flatten)]
        spec: SessionSpec,
    },
    /// Re-attach to an existing session's TUI — re-entering its
    /// conversation — re-provisioning the machine when it was away.
    Resume {
        /// The session to re-enter; picked interactively when omitted.
        id: Option<String>,
        /// Re-enter the most recently touched session.
        #[arg(long, conflicts_with = "id")]
        last: bool,
    },
}

/// The `auth` subcommands.
#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// `GET /v1/me`: the user the credential belongs to.
    Status,
}

/// The `session` subcommands.
#[derive(Debug, Subcommand)]
pub enum SessionCommand {
    /// `GET /v1/sessions`: every session, newest first.
    List {
        /// Only sessions in this state (`provisioning`, `active`,
        /// `paused`, `interrupted`, `archived`, `failed`).
        #[arg(long, value_parser = parse_state)]
        state: Option<flyco_core::SessionState>,
    },
    /// `GET /v1/sessions/{id}`.
    Get {
        /// The session.
        id: String,
    },
    /// `POST /v1/sessions` — a session is born with a goal; `--prompt` (or
    /// piped stdin) is required.
    Create {
        /// Which harness drives the session.
        #[arg(long, value_parser = parse_harness)]
        harness: flyco_core::HarnessKind,
        /// The session's settings.
        #[command(flatten)]
        spec: SessionSpec,
    },
    /// `POST …/messages` — say something to the session's agent. A leading
    /// `!` in the text is the shell escape.
    Send {
        /// The session.
        id: String,
        /// The message, inline.
        #[arg(long, short = 'm', conflicts_with_all = ["file", "stdin_flag"])]
        text: Option<String>,
        /// The message, from a file.
        #[arg(long, short = 'f', value_name = "PATH")]
        file: Option<String>,
        /// The message, from stdin (default when stdin is piped).
        #[arg(long = "stdin")]
        stdin_flag: bool,
    },
    /// Run a command on the session's machine — `!cmd` — and relay its
    /// output until it exits.
    Exec {
        /// The session.
        id: String,
        /// The command and its arguments, after `--`.
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// The session's recorded event stream, as JSONL.
    Events {
        /// The session.
        id: String,
        /// Start strictly after this sequence position.
        #[arg(long)]
        after: Option<u64>,
        /// Keep emitting as events arrive — riding `/v1/events` and
        /// catching gaps up through `events?after=`.
        #[arg(long, short = 'f')]
        follow: bool,
    },
    /// Block until the session reaches one of the named conditions.
    Wait {
        /// The session.
        id: String,
        /// Comma-separated: `active`, `idle`, `approval`, `paused`,
        /// `stopped`, `failed`, `archived`. The first to occur wins.
        #[arg(long = "for", value_delimiter = ',', required = true)]
        conditions: Vec<String>,
        /// Give up after this long (`300`, `5m`, `1h30m`).
        #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
        timeout: Option<std::time::Duration>,
    },
    /// `PATCH /v1/sessions/{id}` — rename, re-budget, re-model, re-mode.
    Set {
        /// The session.
        id: String,
        /// What to call it.
        #[arg(long)]
        title: Option<String>,
        /// New spend limit, in dollars.
        #[arg(long, value_parser = parse_usd)]
        budget: Option<flyco_core::Usd>,
        /// The model to run on.
        #[arg(long)]
        model: Option<String>,
        /// The effort level, meaningful only with `--model`.
        #[arg(long)]
        effort: Option<String>,
        /// The permission mode: `default`, `accept-edits`, `plan`,
        /// `dont-ask`, `auto`, `bypass-permissions`.
        #[arg(long, value_parser = parse_permission_mode)]
        permission_mode: Option<flyco_core::PermissionMode>,
        /// Give the session a desktop the agent can see and drive —
        /// `--computer-use false` takes it away.
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        computer_use: Option<bool>,
    },
    /// Stop the session's machine; disk and session survive.
    Stop {
        /// The session.
        id: String,
    },
    /// Put the session back on a machine (interrupted, paused, archived
    /// and failed sessions resume; an active one is already there).
    Resume {
        /// The session.
        id: String,
    },
    /// Interrupt the turn in flight; the machine stays.
    Interrupt {
        /// The session.
        id: String,
    },
    /// Archive the session — turns kept, environment released.
    Archive {
        /// The session.
        id: String,
        /// Discard uncommitted work. Required when the tree is dirty;
        /// without it a dirty archive is refused.
        #[arg(long)]
        discard_uncommitted: bool,
    },
    /// `GET /v1/approvals?session={id}` — approvals the session raised.
    Approvals {
        /// The session.
        id: String,
        /// Only the ones still waiting.
        #[arg(long)]
        pending: bool,
    },
    /// Decide one of the session's approvals.
    Approve {
        /// The session the approval belongs to (kept for command symmetry;
        /// the route names the approval alone).
        id: String,
        /// The approval.
        approval: String,
        /// Allow it.
        #[arg(long, conflicts_with = "deny")]
        allow: bool,
        /// Refuse it.
        #[arg(long)]
        deny: bool,
    },
    /// `GET …/diff` — the session's working tree against its base branch.
    Diff {
        /// The session.
        id: String,
    },
    /// `GET …/files` — list a directory of the session's checkout.
    Files {
        /// The session.
        id: String,
        /// The directory, relative to the checkout root (default: root).
        path: Option<String>,
    },
    /// `GET …/files/content` — print one file.
    Read {
        /// The session.
        id: String,
        /// The file, relative to the checkout root.
        path: String,
    },
    /// `GET`/`PUT …/env` — the session's environment variables.
    Env {
        /// The session.
        id: String,
        /// `KEY=VALUE` to set (repeatable); without any, prints the set.
        #[arg(long, value_name = "KEY=VALUE")]
        set: Vec<String>,
    },
}

/// The flags every session-creating command shares.
///
/// A flag per picker step: each is asked for interactively only when it is
/// missing *and* stdin is a TTY — on a pipe the missing one is exit 2
/// naming the flag.
#[derive(Debug, Clone, Default, Args)]
pub struct SessionSpec {
    /// `owner/name`; repeatable — each is checked out side by side.
    #[arg(long)]
    pub repo: Vec<String>,
    /// Branch to check out on the first `--repo` (default: the
    /// repository's).
    #[arg(long)]
    pub branch: Option<String>,
    /// The goal the session is born with. Required: a session with nothing
    /// to do is a machine nobody asked for. `-` or piped stdin reads it.
    #[arg(long, short = 'p', conflicts_with = "prompt_file")]
    pub prompt: Option<String>,
    /// The goal, from a file.
    #[arg(long, value_name = "PATH")]
    pub prompt_file: Option<String>,
    /// Spend limit for the whole session, in dollars (`5.00`).
    #[arg(long, value_parser = parse_usd)]
    pub budget: Option<flyco_core::Usd>,
    /// Catalog machine type; `--account` and `--region` belong with it.
    #[arg(long, requires_all = ["account", "region"])]
    pub machine: Option<String>,
    /// The linked provider account to provision through.
    #[arg(long, requires = "machine")]
    pub account: Option<String>,
    /// The catalog region.
    #[arg(long)]
    pub region: Option<String>,
    /// Ask for interruptible spot capacity (the default).
    #[arg(long, conflicts_with = "on_demand")]
    pub spot: bool,
    /// Ask for on-demand capacity.
    #[arg(long)]
    pub on_demand: bool,
    /// Disk size in GiB.
    #[arg(long, value_name = "GIB")]
    pub disk: Option<u32>,
    /// The model to run on.
    #[arg(long)]
    pub model: Option<String>,
    /// The effort level, meaningful only with `--model`.
    #[arg(long)]
    pub effort: Option<String>,
    /// The permission mode: `default`, `accept-edits`, `plan`,
    /// `dont-ask`, `auto`, `bypass-permissions`.
    #[arg(long, value_parser = parse_permission_mode)]
    pub permission_mode: Option<flyco_core::PermissionMode>,
    /// `KEY=VALUE` environment variable (repeatable).
    #[arg(long, value_name = "KEY=VALUE")]
    pub env: Vec<String>,
    /// Reuse a prior attempt's outcome within its 24-hour window: a retried
    /// `run` or `create` names the same key and gets the same session back
    /// rather than provisioning a second machine.
    #[arg(long, value_name = "KEY")]
    pub idempotency_key: Option<String>,
}

impl SessionSpec {
    /// The agent path's `CreateSession` body.
    ///
    /// The spec's fields are `Option` because the human picker fills them
    /// interactively; a non-interactive caller names them all by flag, and
    /// a missing one is usage — never a guess.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when a required flag is missing or malformed.
    pub fn to_request(
        &self,
        harness: flyco_core::HarnessKind,
        prompt: String,
    ) -> Result<flyco_core::CreateSession, crate::Failure> {
        if self.repo.is_empty() {
            return Err(crate::Failure::usage("a session needs --repo owner/name"));
        }
        let repos: Vec<flyco_core::RepoSelection> = self
            .repo
            .iter()
            .enumerate()
            .map(|(index, repo)| flyco_core::RepoSelection {
                repo: repo.clone(),
                branch: if index == 0 {
                    self.branch.clone()
                } else {
                    None
                },
            })
            .collect();
        let budget_limit = self
            .budget
            .ok_or_else(|| crate::Failure::usage("a session needs --budget, in dollars"))?;
        let machine = match (&self.machine, &self.account, &self.region) {
            (Some(machine_type), Some(account), Some(region)) => Some(flyco_core::MachineChoice {
                provider_account: account
                    .parse()
                    .map_err(|_| crate::Failure::usage("`--account` is not an account id"))?,
                machine_type: machine_type.clone(),
                runtime: flyco_core::Runtime::Vm,
                region: region.clone(),
                spot: !self.on_demand,
                disk_gib: self.disk.unwrap_or(flyco_core::DEFAULT_DISK_GIB),
            }),
            (None, None, None) => None,
            // `requires` in clap already rejects the partial cases.
            _ => unreachable!("clap's requires= rules out a partial machine choice"),
        };
        Ok(flyco_core::CreateSession {
            prompt,
            harness,
            repos,
            budget_limit,
            machine,
            spot: !self.on_demand,
            model: self.model.clone().map(|model| flyco_core::ModelChoice {
                model,
                effort: self.effort.clone(),
            }),
            permission_mode: self.permission_mode,
            source: None,
        })
    }
}

/// `claude`/`codex`/`devin` spellings of the three harnesses.
fn parse_harness(text: &str) -> Result<flyco_core::HarnessKind, String> {
    match text {
        "claude" | "claude_code" | "claude-code" => Ok(flyco_core::HarnessKind::ClaudeCode),
        "codex" => Ok(flyco_core::HarnessKind::Codex),
        "devin" => Ok(flyco_core::HarnessKind::Devin),
        other => Err(format!(
            "`{other}` is not a harness — `claude`, `codex` or `devin`"
        )),
    }
}

/// `provisioning`/`active`/`paused`/`interrupted`/`archived`/`failed`.
fn parse_state(text: &str) -> Result<flyco_core::SessionState, String> {
    use flyco_core::SessionState as S;
    match text {
        "provisioning" => Ok(S::Provisioning),
        "active" => Ok(S::Active),
        "paused" => Ok(S::Paused),
        "interrupted" => Ok(S::Interrupted),
        "archived" => Ok(S::Archived),
        "failed" => Ok(S::Failed),
        other => Err(format!("`{other}` is not a session state")),
    }
}

/// The permission-mode spellings the SDK uses (`acceptEdits` and friends),
/// with the hyphenated ones a flag would suggest.
fn parse_permission_mode(text: &str) -> Result<flyco_core::PermissionMode, String> {
    use flyco_core::PermissionMode as M;
    match text {
        "default" => Ok(M::Default),
        "accept-edits" | "acceptEdits" | "accept_edits" => Ok(M::AcceptEdits),
        "bypass-permissions" | "bypassPermissions" | "bypass_permissions" => {
            Ok(M::BypassPermissions)
        }
        "plan" => Ok(M::Plan),
        "dont-ask" | "dontAsk" | "dont_ask" => Ok(M::DontAsk),
        "auto" => Ok(M::Auto),
        other => Err(format!("`{other}` is not a permission mode")),
    }
}

/// `5.00` → `Usd` microdollars. A leading `$` is stripped; anything past
/// six decimal places is more precise than the unit itself and refused.
pub(crate) fn parse_usd(text: &str) -> Result<flyco_core::Usd, String> {
    let text = text.strip_prefix('$').unwrap_or(text);
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if fraction.len() > 6 {
        return Err(format!("`{text}` is finer than a microdollar"));
    }
    let dollars: u64 = whole
        .parse()
        .map_err(|_| format!("`{text}` is not a dollar amount"))?;
    let micros = fraction
        .chars()
        .chain("000000".chars())
        .take(6)
        .collect::<String>()
        .parse::<u64>()
        .map_err(|_| format!("`{text}` is not a dollar amount"))?;
    dollars
        .checked_mul(1_000_000)
        .and_then(|d| d.checked_add(micros))
        .map(flyco_core::Usd::from_micros)
        .ok_or_else(|| format!("`{text}` is too large"))
}

/// `300`, `90s`, `5m`, `1h30m` — a duration in seconds, minutes, hours.
fn parse_duration(text: &str) -> Result<std::time::Duration, String> {
    let mut seconds = 0u64;
    let mut digits = String::new();
    for ch in text.chars() {
        match ch {
            '0'..='9' => digits.push(ch),
            's' | 'm' | 'h' => {
                let value: u64 = digits
                    .parse()
                    .map_err(|_| format!("`{text}` is not a duration"))?;
                digits.clear();
                let unit = match ch {
                    's' => 1,
                    'm' => 60,
                    'h' => 3600,
                    _ => unreachable!(),
                };
                seconds += value * unit;
            }
            _ => {
                return Err(format!(
                    "`{text}` is not a duration — try `300`, `5m`, `1h`"
                ));
            }
        }
    }
    if !digits.is_empty() {
        seconds += digits
            .parse::<u64>()
            .map_err(|_| format!("`{text}` is not a duration"))?;
    }
    if seconds == 0 {
        return Err("`0` is not a duration".to_owned());
    }
    Ok(std::time::Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dollar_amount_parses_to_microdollars() {
        assert_eq!(
            parse_usd("5").expect("valid"),
            flyco_core::Usd::from_dollars(5)
        );
        assert_eq!(
            parse_usd("5.00").expect("valid"),
            flyco_core::Usd::from_dollars(5)
        );
        assert_eq!(
            parse_usd("0.50").expect("valid"),
            flyco_core::Usd::from_cents(50)
        );
        assert_eq!(
            parse_usd("$1.25").expect("valid"),
            flyco_core::Usd::from_micros(1_250_000)
        );
        assert!(parse_usd("0.0000001").is_err());
        assert!(parse_usd("later").is_err());
    }

    #[test]
    fn a_duration_parses() {
        assert_eq!(
            parse_duration("300").expect("valid"),
            std::time::Duration::from_secs(300)
        );
        assert_eq!(
            parse_duration("5m").expect("valid"),
            std::time::Duration::from_secs(300)
        );
        assert_eq!(
            parse_duration("1h30m").expect("valid"),
            std::time::Duration::from_mins(90)
        );
        assert!(parse_duration("0").is_err());
        assert!(parse_duration("soon").is_err());
    }
}
