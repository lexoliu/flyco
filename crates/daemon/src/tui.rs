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
use serde::Serialize;

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
    /// The OAuth credential file could not be written.
    #[error("the OAuth credential could not be written: {0}")]
    CredentialWrite(String),
}

/// The OAuth grant scopes a `claude setup-token` is minted with.
///
/// The CLI's own OAuth client requests this set at login; the token itself
/// carries them server-side, and the credentials file repeats them so the
/// resolved credential's `scopes` satisfies the rate-limit gate. The same
/// list lives in `sidecar.ts`'s `SETUP_TOKEN_SCOPES` — the two writers
/// cannot share a constant across languages.
const SETUP_TOKEN_SCOPES: &[&str] = &[
    "org:create_api_key",
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
];

/// The `.credentials.json` the CLI resolves an OAuth login from.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CredentialFile<'a> {
    claude_ai_oauth: Credential<'a>,
}

/// One OAuth credential entry, as the CLI's credentials store spells it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Credential<'a> {
    access_token: &'a str,
    scopes: &'static [&'static str],
}

/// Writes the OAuth credential the launched CLI resolves.
///
/// An env-injected `CLAUDE_CODE_OAUTH_TOKEN` resolves as env-quad auth,
/// which the CLI's rate-limit gate treats as ineligible for plan limits —
/// `/usage` would show nothing on every OAuth session. The credentials
/// store — `CLAUDE_CONFIG_DIR/.credentials.json` — is the shape a real
/// `claude login` leaves behind, so the gate passes and the plan windows
/// read true. `claude setup-token` mints a long-lived grant with no
/// refresh token, so the entry cannot expire or refresh from the CLI's
/// side.
fn provision_oauth(dir: &Path, token: &str) -> Result<(), TuiError> {
    let io = |error: std::io::Error| TuiError::CredentialWrite(error.to_string());
    std::fs::create_dir_all(dir).map_err(io)?;
    let file = CredentialFile {
        claude_ai_oauth: Credential {
            access_token: token,
            scopes: SETUP_TOKEN_SCOPES,
        },
    };
    let body = serde_json::to_string(&file).expect("a credential file serializes");
    let path = dir.join(".credentials.json");
    std::fs::write(&path, body).map_err(io)?;
    // The credential is a secret: the CLI's own store is owner-only, and
    // this one follows it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(io)?;
    }
    Ok(())
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
    /// Where an OAuth credential is provisioned, and the token it carries —
    /// written fresh at every launch because the CLI removes the file on
    /// some failures.
    oauth_credential: Option<(PathBuf, String)>,
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
        if let Some((dir, token)) = &self.oauth_credential {
            provision_oauth(dir, token)?;
        }
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
        // The OAuth token reaches the CLI as `.credentials.json` inside the
        // isolated config tree, written at launch — env injection resolves
        // as env-quad auth, which is ineligible for plan rate limits.
        let mut oauth_credential = None;
        match &claude.auth {
            ClaudeAuth::Inherit => {}
            ClaudeAuth::OauthToken { token, isolation } => {
                oauth_credential = Some((isolation.config_dir.clone(), token.clone()));
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
            oauth_credential,
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
                oauth_credential: None,
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
            oauth_credential: None,
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
            oauth_credential: None,
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
