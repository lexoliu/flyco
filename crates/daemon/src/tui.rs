//! The session's harness, as a terminal application.
//!
//! `flyco claude`, `flyco codex` and `flyco resume` ask the control plane
//! for [`ControlToDaemon::TerminalHarness`](flyco_core::ControlToDaemon),
//! and the answer lands here: the daemon builds the invocation from its
//! own configuration, so credentials reach the TUI through its environment
//! and nothing a client sends ever carries them.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use flyco_core::HarnessKind;
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
    /// An executable the configuration names — Codex's `bin`.
    Bin(PathBuf),
    /// The Agent SDK's bundled `claude`, resolved inside the sidecar's
    /// `node_modules` at startup. A session whose sidecar lacks it still
    /// runs headless; the error is answered at launch instead.
    Claude(Result<PathBuf, TuiError>),
}

impl HarnessTui {
    /// Builds the launch description from the daemon's configuration.
    ///
    /// The harness the session runs decides which configuration speaks:
    /// for Claude Code the SDK's bundled `claude` binary under the
    /// sidecar's `node_modules`, for Codex the configured `bin`.
    pub async fn resolve(config: &DaemonConfig) -> Self {
        match config.harness {
            HarnessKind::ClaudeCode => Self::claude(config).await,
            HarnessKind::Codex => Self::codex(config),
        }
    }

    /// The invocation for one launch, fresh or re-entering.
    ///
    /// # Errors
    ///
    /// Returns [`TuiError`] when the program itself could not be resolved —
    /// the claude binary missing from the sidecar tree.
    pub fn command(&self, resume: bool) -> Result<CommandBuilder, TuiError> {
        let program = match &self.program {
            Program::Bin(bin) => bin.clone(),
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

    /// The Codex TUI: the configured binary under the session's isolated
    /// `CODEX_HOME`, with the session's model, effort and permission mode
    /// as `-c` overrides — Codex's own spelling of the same facts.
    fn codex(config: &DaemonConfig) -> Self {
        let codex = config.codex();

        let env = codex.auth.home().map_or_else(Vec::new, |home| {
            vec![(
                OsStr::new("CODEX_HOME").to_owned(),
                home.as_os_str().to_owned(),
            )]
        });

        // `-c` takes TOML, so string values are quoted.
        let mut args: Vec<OsString> = Vec::new();
        let mut set = |key: &str, value: &str| {
            args.push("-c".into());
            args.push(format!("{key}=\"{value}\"").into());
        };
        if let Some(model) = &codex.model {
            set("model", model);
        }
        if let Some(effort) = &codex.effort {
            set("model_reasoning_effort", effort);
        }
        set(
            "approval_policy",
            codex.permission_mode.codex_approval_policy(),
        );
        set("sandbox_mode", codex.permission_mode.codex_sandbox());

        let mut resume_args = vec![OsString::from("resume")];
        match &config.resume_session_id {
            Some(id) => resume_args.push(id.into()),
            None => resume_args.push("--last".into()),
        }
        resume_args.extend(args.iter().cloned());

        Self {
            program: Program::Bin(codex.bin.clone()),
            args,
            resume_args,
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
