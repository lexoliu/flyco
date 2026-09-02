//! `flycod`'s on-disk configuration.
//!
//! One TOML file, deserialized with `deny_unknown_fields` throughout: a
//! typo in a session VM's config is a provisioning bug, and the daemon says
//! so at startup rather than running with a silently ignored setting.

use std::path::{Path, PathBuf};

use flyco_core::{BranchName, DAEMON_TOKEN_PREFIX, HarnessKind, RepoSlug, SessionId};
use serde::Deserialize;
use url::Url;

use crate::harness::claude::protocol::{PermissionMode, SidecarAuth};
use crate::harness::claude::sidecar::SidecarConfig;

/// The configuration file could not be used.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("could not read the flycod config at {path}")]
    Read {
        /// The path that was tried.
        path: PathBuf,
        /// The underlying cause.
        #[source]
        source: std::io::Error,
    },
    /// The file is not valid TOML, or names something flycod does not know.
    #[error("the flycod config at {path} is invalid")]
    Parse {
        /// The path that was parsed.
        path: PathBuf,
        /// The underlying cause.
        #[source]
        source: toml::de::Error,
    },
    /// `[control_plane].daemon_token` is not a daemon token.
    #[error(
        "`control_plane.daemon_token` must be a `{DAEMON_TOKEN_PREFIX}` token from \
         `POST /v1/sessions/{{id}}/daemon-token`, not a session token or an API key"
    )]
    NotADaemonToken,
    /// The tables in the file do not match `harness`.
    #[error("{0}")]
    WrongHarness(&'static str),
}

/// An isolated Claude Code configuration tree.
///
/// Present on exactly the auth modes that inject credentials: a session VM
/// gets its own config directory and a stable project key, so nothing about
/// one session's Claude state can be seen or trampled by another.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Isolation {
    /// `CLAUDE_CONFIG_DIR` for the supervised CLI.
    pub config_dir: PathBuf,
    /// `CLAUDE_CODE_PROJECT_DIR_NAME` — the stable project key the Agent
    /// SDK files sessions and transcripts under.
    pub project_dir_name: String,
}

/// How the supervised `claude` CLI authenticates.
///
/// Credentials and isolation are one decision, not two: injecting a token
/// into the host's own `~/.claude` would rewrite a real person's login, and
/// an isolated config tree with no credentials has no way to authenticate.
/// Making [`Isolation`] a field of the credential-bearing variants means
/// neither mistake is expressible.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClaudeAuth {
    /// Use the host user's existing Claude login.
    ///
    /// No `CLAUDE_CONFIG_DIR`, no credential injection — the CLI reads the
    /// default `~/.claude`. This is how flyco is driven on a developer's
    /// own machine; a provisioned session VM never uses it.
    Inherit,
    /// A Claude subscription OAuth token in an isolated config tree.
    OauthToken {
        /// Value for `CLAUDE_CODE_OAUTH_TOKEN`.
        token: String,
        /// The config tree it applies to.
        isolation: Isolation,
    },
    /// An Anthropic API key in an isolated config tree.
    ApiKey {
        /// Value for `ANTHROPIC_API_KEY`.
        key: String,
        /// The config tree it applies to.
        isolation: Isolation,
    },
}

impl ClaudeAuth {
    /// The credentials half, as the sidecar protocol carries it.
    #[must_use]
    pub fn sidecar_auth(&self) -> SidecarAuth {
        match self {
            Self::Inherit => SidecarAuth::Inherit,
            Self::OauthToken { token, .. } => SidecarAuth::OauthToken {
                token: token.clone(),
            },
            Self::ApiKey { key, .. } => SidecarAuth::ApiKey { key: key.clone() },
        }
    }

    /// The isolated config tree, when this mode has one.
    #[must_use]
    pub const fn isolation(&self) -> Option<&Isolation> {
        match self {
            Self::Inherit => None,
            Self::OauthToken { isolation, .. } | Self::ApiKey { isolation, .. } => Some(isolation),
        }
    }
}

/// Settings specific to the Claude Code harness.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeConfig {
    /// Model override; omitted leaves the CLI's own default in place.
    #[serde(default)]
    pub model: Option<String>,
    /// Permission mode, spelled the way the Agent SDK spells it
    /// (`default`, `acceptEdits`, `bypassPermissions`, `plan`).
    pub permission_mode: PermissionMode,
    /// Credentials and config isolation.
    pub auth: ClaudeAuth,
}

/// An isolated Codex home directory.
///
/// Present on exactly the auth modes that inject credentials: a session VM
/// gets its own `CODEX_HOME`, so nothing about one session's Codex state can
/// be seen or trampled by another.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexIsolation {
    /// `CODEX_HOME` for the supervised app-server.
    pub home: PathBuf,
}

/// How the supervised `codex` CLI authenticates.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum CodexAuth {
    /// Use the host user's existing Codex login.
    Inherit,
    /// A `ChatGPT` OAuth access token in an isolated `CODEX_HOME`.
    OauthToken {
        /// Value written into `auth.json` as `tokens.access_token`.
        token: String,
        /// The home directory it applies to.
        isolation: CodexIsolation,
    },
    /// An `OpenAI` API key in an isolated `CODEX_HOME`.
    ApiKey {
        /// Value written into `auth.json` as `OPENAI_API_KEY`.
        key: String,
        /// The home directory it applies to.
        isolation: CodexIsolation,
    },
}

impl CodexAuth {
    /// The isolated `CODEX_HOME`, when this mode has one.
    #[must_use]
    pub fn home(&self) -> Option<&Path> {
        match self {
            Self::Inherit => None,
            Self::OauthToken { isolation, .. } | Self::ApiKey { isolation, .. } => {
                Some(isolation.home.as_path())
            }
        }
    }
}

/// Approval policy the app-server applies, spelled as the protocol's kebab-case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodexApprovalPolicy {
    /// Prompt on everything.
    Untrusted,
    /// Prompt on request — flyco's default, so every tool reaches the UI.
    OnRequest,
    /// Never prompt.
    Never,
}

impl CodexApprovalPolicy {
    /// The protocol token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Untrusted => "untrusted",
            Self::OnRequest => "on-request",
            Self::Never => "never",
        }
    }
}

/// Sandbox mode the app-server applies, spelled as the protocol's kebab-case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodexSandbox {
    /// Read-only sandbox.
    ReadOnly,
    /// Workspace-write sandbox.
    WorkspaceWrite,
    /// No sandbox.
    DangerFullAccess,
}

impl CodexSandbox {
    /// The protocol token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}

fn default_codex_bin() -> PathBuf {
    PathBuf::from("codex")
}

/// Settings specific to the Codex harness.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexConfig {
    /// The `codex` executable. A bare name is resolved through `PATH`.
    #[serde(default = "default_codex_bin")]
    pub bin: PathBuf,
    /// Model override; omitted leaves the CLI's own default in place.
    #[serde(default)]
    pub model: Option<String>,
    /// Approval policy for `thread/start`.
    pub approval_policy: CodexApprovalPolicy,
    /// Sandbox mode for `thread/start`.
    pub sandbox: CodexSandbox,
    /// Credentials and `CODEX_HOME` isolation.
    pub auth: CodexAuth,
}

fn default_shell() -> PathBuf {
    PathBuf::from("fish")
}

/// The interactive web terminal flycod attaches to a session.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalConfig {
    /// Shell to spawn in the session workdir. A bare name is resolved
    /// through `PATH`.
    #[serde(default = "default_shell")]
    pub shell: PathBuf,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            shell: default_shell(),
        }
    }
}

/// Where the control plane is, and what authenticates this daemon to it.
///
/// Present on a provisioned session VM; absent on a developer machine,
/// where `flycod run` falls back to the [REPL](crate::repl). Which of the
/// two a run chose is logged at startup, because "why is nothing reaching
/// the browser" has exactly one cheap answer and this is it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlPlaneConfig {
    /// Base URL of the control plane, e.g. `https://flyco.dev/`.
    ///
    /// The relay endpoint is derived from it, so the address is configured
    /// once rather than twice in two schemes.
    pub url: Url,
    /// The session's `fd_` daemon token, minted by
    /// `POST /v1/sessions/{id}/daemon-token`.
    pub daemon_token: String,
}

impl ControlPlaneConfig {
    /// Checks that the token is the kind of credential this field takes.
    ///
    /// A session token or an API key here would authenticate nothing and
    /// fail at the first relay attempt; a provisioning bug should stop the
    /// daemon at startup instead, where the message can name the mistake.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::NotADaemonToken`] if the token lacks the
    /// [`fd_`](flyco_core::DAEMON_TOKEN_PREFIX) prefix.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.daemon_token.starts_with(DAEMON_TOKEN_PREFIX) {
            Ok(())
        } else {
            Err(ConfigError::NotADaemonToken)
        }
    }
}

/// Who the checkout's commits are authored as.
///
/// Flyco's default is to behave as the user rather than as a bot: the clone
/// is authenticated with the user's own GitHub token, so the commits the
/// agent writes carry the user's own name and address. Configuring the two
/// here rather than leaving git to guess is what stops a session VM
/// authoring commits as `flyco@<hostname>`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitIdentity {
    /// `user.name` for the checkout.
    pub name: String,
    /// `user.email` for the checkout.
    pub email: String,
}

/// The repository this session works in, and what authenticates it.
///
/// Present on a provisioned session VM; omitted on a developer machine,
/// where [`DaemonConfig::workdir`] is a directory the developer already
/// has. A daemon with no `[repo]` clones nothing and works in the directory
/// it was pointed at.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    /// The repository, `owner/name`.
    pub slug: RepoSlug,
    /// The branch to check out.
    pub branch: BranchName,
    /// The user's GitHub token.
    ///
    /// Fed to git through a credential helper that reads it from the
    /// environment of one child process, so it is never written into a
    /// remote URL, into `.git/config`, or into a log line. The hand-written
    /// [`fmt::Debug`] is the other half of that: this structure is exactly
    /// what a `?config` in a trace would print.
    pub token: String,
    /// Who the checkout's commits are authored as.
    pub identity: GitIdentity,
}

impl core::fmt::Debug for RepoConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RepoConfig")
            .field("slug", &self.slug)
            .field("branch", &self.branch)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

impl RepoConfig {
    /// Where the repository is cloned from.
    ///
    /// HTTPS rather than SSH because the credential flyco holds is an OAuth
    /// token, not a key: `https://github.com/owner/name.git` is the one form
    /// a token can authenticate, and it carries no credential itself — the
    /// token reaches git through the credential helper instead, so nothing
    /// on disk or in a process listing ever holds it.
    #[must_use]
    pub fn remote_url(&self) -> String {
        format!("https://github.com/{}.git", self.slug)
    }
}

/// Everything `flycod run` needs.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    /// The flyco session this daemon serves.
    pub session: SessionId,
    /// Which harness to drive.
    pub harness: HarnessKind,
    /// Directory the agent works in.
    pub workdir: PathBuf,
    /// Root of the local append-only transcript store.
    ///
    /// Used only when there is no [`control_plane`](Self::control_plane):
    /// a session that reports to a control plane keeps its transcript there
    /// instead, which is what lets it resume onto another machine.
    pub transcript_dir: PathBuf,
    /// Harness-native session id to resume, for cross-host History.
    #[serde(default)]
    pub resume_session_id: Option<String>,
    /// The control plane to report to. Omitted drives the session from the
    /// [REPL](crate::repl) instead.
    #[serde(default)]
    pub control_plane: Option<ControlPlaneConfig>,
    /// The repository to clone into [`workdir`](Self::workdir) before the
    /// harness starts. Omitted works in whatever is already there, which is
    /// the developer-machine shape.
    #[serde(default)]
    pub repo: Option<RepoConfig>,
    /// Claude Code settings. Required when [`harness`](Self::harness) is
    /// [`HarnessKind::ClaudeCode`].
    #[serde(default)]
    pub claude: Option<ClaudeConfig>,
    /// Where the Bun sidecar is materialized and how Bun is run. Required
    /// when [`harness`](Self::harness) is [`HarnessKind::ClaudeCode`].
    #[serde(default)]
    pub sidecar: Option<SidecarConfig>,
    /// Codex settings. Required when [`harness`](Self::harness) is
    /// [`HarnessKind::Codex`].
    #[serde(default)]
    pub codex: Option<CodexConfig>,
    /// The interactive web terminal. Omitted uses `fish` in the session
    /// workdir.
    #[serde(default)]
    pub terminal: TerminalConfig,
}

impl DaemonConfig {
    /// Reads and validates a config file.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if the file cannot be read, is not valid
    /// TOML, or names a field flycod does not know.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        let config: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source,
        })?;
        if let Some(control_plane) = &config.control_plane {
            control_plane.validate()?;
        }
        config.validate_harness()?;
        Ok(config)
    }

    /// Returns the Claude Code settings, which a Claude session always has.
    ///
    /// # Panics
    ///
    /// Panics if [`load`](Self::load) admitted a Claude session without a
    /// `[claude]` table — that combination is unrepresentable after load.
    #[must_use]
    pub const fn claude(&self) -> &ClaudeConfig {
        self.claude
            .as_ref()
            .expect("a loaded Claude Code config has a [claude] table")
    }

    /// Returns the sidecar settings, which a Claude session always has.
    ///
    /// # Panics
    ///
    /// Panics if [`load`](Self::load) admitted a Claude session without a
    /// `[sidecar]` table — that combination is unrepresentable after load.
    #[must_use]
    pub const fn sidecar(&self) -> &SidecarConfig {
        self.sidecar
            .as_ref()
            .expect("a loaded Claude Code config has a [sidecar] table")
    }

    /// Returns the Codex settings, which a Codex session always has.
    ///
    /// # Panics
    ///
    /// Panics if [`load`](Self::load) admitted a Codex session without a
    /// `[codex]` table — that combination is unrepresentable after load.
    #[must_use]
    pub const fn codex(&self) -> &CodexConfig {
        self.codex
            .as_ref()
            .expect("a loaded Codex config has a [codex] table")
    }

    const fn validate_harness(&self) -> Result<(), ConfigError> {
        match self.harness {
            HarnessKind::ClaudeCode => {
                if self.claude.is_none() {
                    return Err(ConfigError::WrongHarness(
                        "a claude_code session requires a [claude] table",
                    ));
                }
                if self.sidecar.is_none() {
                    return Err(ConfigError::WrongHarness(
                        "a claude_code session requires a [sidecar] table",
                    ));
                }
            }
            HarnessKind::Codex => {
                if self.codex.is_none() {
                    return Err(ConfigError::WrongHarness(
                        "a codex session requires a [codex] table",
                    ));
                }
            }
        }
        Ok(())
    }
}

/// A complete, valid configuration, printed by `flycod example-config`.
pub const EXAMPLE: &str = include_str!("../config.example.toml");

#[cfg(test)]
mod tests {
    use super::{ClaudeAuth, DaemonConfig, EXAMPLE};
    use crate::harness::claude::protocol::{PermissionMode, SidecarAuth};
    use flyco_core::HarnessKind;

    fn parse(text: &str) -> Result<DaemonConfig, toml::de::Error> {
        toml::from_str(text)
    }

    #[test]
    fn the_shipped_example_is_a_valid_config() {
        let config = parse(EXAMPLE).expect("the example config must parse");
        assert_eq!(config.harness, HarnessKind::ClaudeCode);
        let claude = config
            .claude
            .as_ref()
            .expect("the example drives Claude Code");
        assert!(matches!(claude.auth, ClaudeAuth::Inherit));
        assert_eq!(claude.permission_mode, PermissionMode::Default);
        assert_eq!(
            config.sidecar.as_ref().expect("example sidecar").bun,
            std::path::PathBuf::from("bun")
        );
    }

    #[test]
    fn inheriting_the_host_login_isolates_nothing() {
        let config = parse(EXAMPLE).expect("parse");
        let claude = config.claude.as_ref().expect("example");
        assert!(claude.auth.isolation().is_none());
        assert!(matches!(claude.auth.sidecar_auth(), SidecarAuth::Inherit));
    }

    #[test]
    fn injected_credentials_carry_their_own_config_tree() {
        let text = EXAMPLE.replace(
            "mode = \"inherit\"",
            "mode = \"oauth_token\"\ntoken = \"sk-ant-oat01-example\"\n\
             isolation = { config_dir = \"/var/lib/flyco/claude\", project_dir_name = \"flyco-session\" }",
        );
        let config = parse(&text).expect("parse");
        let isolation = config
            .claude
            .as_ref()
            .expect("example")
            .auth
            .isolation()
            .expect("credential modes are isolated");
        assert_eq!(isolation.project_dir_name, "flyco-session");
        assert!(matches!(
            config.claude.as_ref().expect("example").auth.sidecar_auth(),
            SidecarAuth::OauthToken { .. }
        ));
    }

    #[test]
    fn credentials_without_a_config_tree_do_not_parse() {
        let text = EXAMPLE.replace(
            "mode = \"inherit\"",
            "mode = \"api_key\"\nkey = \"sk-ant-example\"",
        );
        parse(&text).expect_err("an isolation-less credential mode is not expressible");
    }

    #[test]
    fn an_unknown_key_is_rejected_rather_than_ignored() {
        let text = format!("{EXAMPLE}\nreconnect_delay_ms = 500\n");
        let error = parse(&text).expect_err("unknown keys must fail the daemon at startup");
        assert!(
            error.to_string().contains("reconnect_delay_ms"),
            "the error should name the offending key: {error}"
        );
    }
}
