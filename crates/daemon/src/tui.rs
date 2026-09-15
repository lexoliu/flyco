//! The session's harness, as a terminal application.
//!
//! `flyco claude`, `flyco codex` and `flyco resume` ask the control plane
//! for [`ControlToDaemon::TerminalHarness`](flyco_core::ControlToDaemon),
//! and the answer lands here: the daemon builds the invocation from its
//! own configuration, so credentials reach the TUI through its environment
//! and nothing a client sends ever carries them.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use flyco_core::DriverKind;
use portable_pty::CommandBuilder;

use crate::config::{ClaudeAuth, DaemonConfig};

/// Why a harness TUI cannot be launched.
#[derive(Debug, Clone, thiserror::Error)]
pub enum TuiError {
    /// The sidecar's `node_modules` holds no `claude` binary.
    #[error("no claude binary under {0}: the sidecar's `bun install` did not leave one")]
    ClaudeMissing(PathBuf),
    /// The sidecar's `node_modules` could not be listed at all.
    #[error("could not list {0}")]
    ClaudeUnreadable(PathBuf),
    /// The agent's `[acp]` table declares no TUI to bridge to.
    #[error("{0} has no TUI: its [acp.tui] table is empty, so there is nothing to launch")]
    NoTui(String),
}

/// How the session's harness TUI is launched.
///
/// Resolved once from the daemon's configuration: the program, the
/// arguments a fresh launch takes, the arguments a re-entry takes, and
/// the environment — credentials included — the child is spawned with.
pub struct HarnessTui {
    program: Program,
    args: Vec<OsString>,
    resume_args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
}

impl core::fmt::Debug for HarnessTui {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // `env` carries the session's credentials — a Debug that printed
        // it would put them in every trace the value touches, so only the
        // variable *names* are shown.
        f.debug_struct("HarnessTui")
            .field("args", &self.args)
            .field("resume_args", &self.resume_args)
            .field(
                "env",
                &self.env.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

/// The executable half of a [`HarnessTui`].
enum Program {
    /// An executable the configuration names — an `[acp.tui]` program.
    Bin(PathBuf),
    /// The agent has no TUI; the error is answered at launch instead.
    None(String),
    /// The Agent SDK's bundled `claude`, resolved inside the sidecar's
    /// `node_modules` at startup. A session whose sidecar lacks it still
    /// runs headless; the error is answered at launch instead.
    Claude(Result<PathBuf, TuiError>),
}

impl HarnessTui {
    /// Builds the launch description from the daemon's configuration.
    ///
    /// The driver the session runs decides which configuration speaks:
    /// for Claude Code the SDK's bundled `claude` binary under the
    /// sidecar's `node_modules`, for ACP the `[acp.tui]` table — which an
    /// agent without a TUI simply does not have, and says so at launch.
    pub async fn resolve(config: &DaemonConfig) -> Self {
        match config.harness {
            DriverKind::ClaudeCode => Self::claude(config).await,
            DriverKind::Acp => Self::acp(config),
        }
    }

    /// The invocation for one launch, fresh or re-entering.
    ///
    /// # Errors
    ///
    /// Returns [`TuiError`] when the program itself could not be resolved —
    /// the claude binary missing from the sidecar tree, or an agent with
    /// no TUI at all.
    pub fn command(&self, resume: bool) -> Result<CommandBuilder, TuiError> {
        let program = match &self.program {
            Program::Bin(bin) => bin.clone(),
            Program::None(agent) => return Err(TuiError::NoTui(agent.clone())),
            Program::Claude(resolved) => resolved.clone()?,
        };
        let mut command = CommandBuilder::new(program);
        command.args(if resume {
            &self.resume_args
        } else {
            &self.args
        });
        for (name, value) in &self.env {
            command.env(name, value);
        }
        Ok(command)
    }

    /// The Claude Code TUI: the SDK's platform binary, the session's
    /// isolated config tree and credential, and the session's model and
    /// permission mode as flags.
    async fn claude(config: &DaemonConfig) -> Self {
        let claude = config.claude();
        let sidecar = config.sidecar();

        let mut env: Vec<(OsString, OsString)> = Vec::new();
        if let Some(isolation) = claude.auth.isolation() {
            env.push((
                OsStr::new("CLAUDE_CONFIG_DIR").to_owned(),
                isolation.config_dir.clone().into_os_string(),
            ));
            env.push((
                OsStr::new("CLAUDE_CODE_PROJECT_DIR_NAME").to_owned(),
                isolation.project_dir_name.clone().into(),
            ));
        }
        match &claude.auth {
            ClaudeAuth::Inherit => {}
            ClaudeAuth::OauthToken { token, .. } => {
                env.push((
                    OsStr::new("CLAUDE_CODE_OAUTH_TOKEN").to_owned(),
                    token.into(),
                ));
            }
            ClaudeAuth::ApiKey { key, .. } => {
                env.push((OsStr::new("ANTHROPIC_API_KEY").to_owned(), key.into()));
            }
        }

        let mut args: Vec<OsString> = Vec::new();
        if let Some(model) = &claude.model {
            args.push("--model".into());
            args.push(model.into());
        }
        // The SDK's camelCase spelling is the CLI's own spelling for the
        // flag — both name the same vocabulary.
        let mode = serde_json::to_value(claude.permission_mode)
            .expect("a permission mode serializes")
            .as_str()
            .expect("a permission mode serializes to a string")
            .to_owned();
        args.push("--permission-mode".into());
        args.push(mode.into());

        // Re-entry names the harness session this Flyco session continues
        // when the configuration knows it, and the latest conversation
        // otherwise.
        let mut resume_args = args.clone();
        match &config.resume_session_id {
            Some(id) => {
                resume_args.push("--resume".into());
                resume_args.push(id.into());
            }
            None => resume_args.push("--continue".into()),
        }

        Self {
            program: Program::Claude(find_sdk_claude(&sidecar.dir).await),
            args,
            resume_args,
            env,
        }
    }

    /// The ACP agent's TUI: the program `[acp.tui]` names, under the same
    /// environment the driver spawns the agent with plus the TUI's own —
    /// a `CODEX_HOME` or `XDG_DATA_HOME` the agent's credentials live
    /// under reaches its TUI the same way it reaches the agent.
    ///
    /// Resume arguments are spelled by the provisioner rather than built
    /// here: each agent re-enters a conversation its own way, and a
    /// `{session}` placeholder in `resume_args` is filled with the
    /// harness-native session id when one is recorded.
    fn acp(config: &DaemonConfig) -> Self {
        let acp = config.acp();
        let Some(tui) = &acp.tui else {
            return Self {
                program: Program::None(acp.agent.clone()),
                args: Vec::new(),
                resume_args: Vec::new(),
                env: Vec::new(),
            };
        };

        let env = acp
            .env
            .iter()
            .chain(tui.env.iter())
            .map(|(name, value)| (name.clone().into(), value.clone().into()))
            .collect();

        let fill = |args: &[String]| -> Vec<OsString> {
            args.iter()
                .filter_map(|arg| {
                    config.resume_session_id.as_ref().map_or_else(
                        // With nothing recorded the placeholder has no
                        // value: `codex resume` and `devin --resume`
                        // without it land on the agent's own picker, which
                        // is the resume the session can offer.
                        || (!arg.contains("{session}")).then(|| OsString::from(arg)),
                        |id| Some(OsString::from(arg.replace("{session}", id))),
                    )
                })
                .collect()
        };

        Self {
            program: Program::Bin(tui.program.clone()),
            args: fill(&tui.args),
            resume_args: fill(&tui.resume_args),
            env,
        }
    }
}

#[cfg(test)]
impl HarnessTui {
    /// A spec whose command exists and runs nothing — tests record the
    /// launch rather than spawn it.
    pub(crate) fn fixture() -> Self {
        Self {
            program: Program::Bin(PathBuf::from("/bin/true")),
            args: Vec::new(),
            resume_args: Vec::new(),
            env: Vec::new(),
        }
    }
}

/// The `claude` binary the Agent SDK bundles for this platform.
///
/// The SDK ships one package per platform — `claude-agent-sdk-linux-x64`
/// and its siblings — each holding the binary as `claude`. Whichever one
/// `bun install` resolved for this machine is the only one present, so the
/// glob answers with the single match.
async fn find_sdk_claude(sidecar_dir: &Path) -> Result<PathBuf, TuiError> {
    let packages = sidecar_dir.join("node_modules/@anthropic-ai");
    let mut entries = match tokio::fs::read_dir(&packages).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(TuiError::ClaudeMissing(packages));
        }
        Err(_) => return Err(TuiError::ClaudeUnreadable(packages)),
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("claude-agent-sdk-") {
            continue;
        }
        let binary = entry.path().join("claude");
        if matches!(tokio::fs::metadata(&binary).await, Ok(meta) if meta.is_file()) {
            return Ok(binary);
        }
    }
    Err(TuiError::ClaudeMissing(packages))
}
