//! `flycod`'s on-disk configuration.
//!
//! One TOML file, deserialized with `deny_unknown_fields` throughout: a
//! typo in a session VM's config is a provisioning bug, and the daemon says
//! so at startup rather than running with a silently ignored setting.

use std::path::{Path, PathBuf};

use flyco_core::{
    BranchName, DAEMON_TOKEN_PREFIX, HarnessKind, MachineOrigin, McpServerMount, RepoSlug,
    SessionId, SessionMachine,
};
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
    /// Reasoning effort for that model, as the SDK's `EffortLevel` spells
    /// it (`low`, `medium`, `high`, `xhigh`, `max`).
    ///
    /// Separate from [`Self::model`] because the harness takes them
    /// separately, and omitted where the session chose none so the CLI's
    /// own default for the model stands. Every provisioned machine now
    /// names the model, and most name no effort.
    #[serde(default)]
    pub effort: Option<String>,
    /// Permission mode, spelled the way the Agent SDK spells it
    /// (`default`, `acceptEdits`, `bypassPermissions`, `plan`).
    pub permission_mode: PermissionMode,
    /// Claude Code's managed-policy directory — `/etc/claude-code` on a
    /// provisioned machine.
    ///
    /// Where flycod writes `managed-settings.json` and `managed-mcp.json`,
    /// which outrank every other settings source and are what make the MCP
    /// allowlist a fact about the filesystem. Absent on a developer
    /// machine, where flycod is not root and the host's own managed policy
    /// is not flyco's to overwrite; the session's servers are still mounted
    /// there, through the Agent SDK, but nothing stops the agent adding
    /// more.
    #[serde(default)]
    pub managed_dir: Option<PathBuf>,
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
///
/// The two modes Codex's own `auth.json` has, spelled the way Codex spells
/// them: an `OPENAI_API_KEY`, or the `ChatGPT` grant `codex login
/// --device-auth` produces. The `ChatGPT` grant is four values rather than
/// one because `auth.json` is four values — the access token expires within
/// the hour, and the control plane, not the daemon, is what renews it.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum CodexAuth {
    /// Use the host user's existing Codex login.
    Inherit,
    /// An `OpenAI` API key in an isolated `CODEX_HOME`.
    ApiKey {
        /// Value written into `auth.json` as `OPENAI_API_KEY`.
        key: String,
        /// The home directory it applies to.
        isolation: CodexIsolation,
    },
    /// A `ChatGPT` subscription grant in an isolated `CODEX_HOME`.
    #[serde(rename = "chatgpt")]
    ChatGpt {
        /// Written into `auth.json` as `tokens.id_token`.
        id_token: String,
        /// Written into `auth.json` as `tokens.access_token`.
        access_token: String,
        /// Written into `auth.json` as `tokens.refresh_token`.
        refresh_token: String,
        /// Written into `auth.json` as `tokens.account_id`.
        account_id: String,
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
            Self::ApiKey { isolation, .. } | Self::ChatGpt { isolation, .. } => {
                Some(isolation.home.as_path())
            }
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
    /// Reasoning effort, which reaches the app-server as
    /// `model_reasoning_effort` on `thread/start` and as `effort` on every
    /// `turn/start`.
    ///
    /// Omitted where the session chose none, so the app-server's own
    /// `defaultReasoningEffort` for the model stands.
    #[serde(default)]
    pub effort: Option<String>,
    /// Permission mode the thread runs under.
    ///
    /// Spelled flyco's way rather than the app-server's: Codex has no
    /// permission modes, it has an approval policy and a sandbox that say
    /// the same thing together, so the driver holds the one mode and
    /// translates it into the pair — [`PermissionMode::codex_approval_policy`]
    /// and [`PermissionMode::codex_sandbox`] — wherever the protocol asks.
    pub permission_mode: PermissionMode,
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

/// `bash`, the program a `!` composer message is run by.
fn default_bash() -> PathBuf {
    PathBuf::from("bash")
}

/// How long a `!` command may run before it is killed.
///
/// Two minutes is a test suite or a build, and past that a command the user
/// wanted to watch belongs in the web terminal, which has no deadline. The
/// bound exists at all because the machine runs one `!` command at a time:
/// without it, a single `tail -f` would take the feature away for the rest
/// of the session.
const fn default_shell_timeout_seconds() -> u64 {
    120
}

/// How much of a `!` command's output reaches the transcript.
///
/// Every chunk is appended to the session room's durable stream, so this is
/// a bound on a Durable Object rather than on a terminal window.
const fn default_shell_max_output_bytes() -> usize {
    64 * 1024
}

/// How a message beginning with `!` is run (docs/ux.md §9.3).
///
/// The command runs as whoever `flycod` runs as — the agent's own user on a
/// provisioned machine — in the session's [`workdir`](DaemonConfig::workdir),
/// which is what makes `!git status` answer about the checkout the user is
/// looking at.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellConfig {
    /// The bash executable. A bare name is resolved through `PATH`.
    ///
    /// `bash` rather than the [terminal's](TerminalConfig) shell: the
    /// composer tells the user their `!` message is bash, so what runs it
    /// has to be bash whatever they chose to type in interactively.
    #[serde(default = "default_bash")]
    pub program: PathBuf,
    /// How long one command may run before it is killed.
    #[serde(default = "default_shell_timeout_seconds")]
    pub timeout_seconds: u64,
    /// How much output one command may put in the transcript.
    #[serde(default = "default_shell_max_output_bytes")]
    pub max_output_bytes: usize,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            program: default_bash(),
            timeout_seconds: default_shell_timeout_seconds(),
            max_output_bytes: default_shell_max_output_bytes(),
        }
    }
}

impl ShellConfig {
    /// The runner this configuration describes, for `workdir`.
    #[must_use]
    pub fn runner(&self, workdir: PathBuf) -> crate::shell::Bash {
        crate::shell::Bash::new(
            self.program.clone(),
            workdir,
            core::time::Duration::from_secs(self.timeout_seconds),
            self.max_output_bytes,
        )
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

/// The display's width, in pixels.
///
/// 1280×800 is the geometry Anthropic's computer-use stack trains and
/// evals at, which makes it the geometry the `computer_*` tools' coordinate
/// space is documented in rather than an arbitrary default.
const fn default_display_width() -> u32 {
    1280
}

/// The display's height, in pixels.
const fn default_display_height() -> u32 {
    800
}

/// How many frames a second the encoder is asked to keep while a browser
/// watches.
///
/// Five is a watchable cadence for an agent driving a desktop and a sixth
/// of what interactive streaming would ask for — the budget it sets is the
/// VM's encoder CPU and the egress meter, not the eye's.
const fn default_fps() -> u32 {
    5
}

/// The session's desktop — the screen the model sees and drives, and the
/// stream the user watches and takes over.
///
/// The provisioned configuration always writes this table; on a developer
/// machine it is absent and the session has no screen.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputerConfig {
    /// Whether this session may have a screen.
    ///
    /// The capability at boot, not a lock: a session that starts with the
    /// flag off can still be given a screen later through
    /// [`SetComputerUse`](flyco_core::ControlToDaemon::SetComputerUse),
    /// which starts the same stack this flag would have started here.
    pub enabled: bool,
    /// Display width in pixels.
    #[serde(default = "default_display_width")]
    pub width: u32,
    /// Display height in pixels.
    #[serde(default = "default_display_height")]
    pub height: u32,
    /// Frames per second the encoder keeps while a browser watches. Encoded
    /// at all only while [`DesktopAudience`](flyco_core::ControlToDaemon::DesktopAudience)
    /// says someone is watching.
    #[serde(default = "default_fps")]
    pub fps: u32,
}

impl Default for ComputerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            width: default_display_width(),
            height: default_display_height(),
            fps: default_fps(),
        }
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
    /// Whether flyco or the user chose the machine this session runs on.
    ///
    /// What the notice injected at session start turns on: a machine the
    /// user picked is a decision the agent must not quietly undo, and it is
    /// told so in as many words (docs/ux.md §9.5). A fact about the session
    /// rather than about the machine, so it outlives every resize.
    pub machine_origin: MachineOrigin,
    /// The machine this daemon booted on, as the agent is told about it.
    ///
    /// The boot-time description. It is what the session-start notice
    /// states, and it is *not* kept current here: a resize restarts the
    /// machine without rewriting this file, and the live answer comes from
    /// `GET /v1/sessions/{id}/agent/machine`, which is what the agent's
    /// `machine_status` tool reads.
    pub machine: SessionMachine,
    /// Whether the filesystem this daemon works on survives the machine
    /// stopping.
    ///
    /// The one fact nothing on the machine can discover for itself, and the
    /// one that decides what `SIGTERM` means here. On a
    /// [`Runtime::Vm`](flyco_core::Runtime::Vm) it is systemd stopping a
    /// unit on a disk that will still be there, and the daemon simply goes.
    /// On a [`Runtime::Container`](flyco_core::Runtime::Container) it is the
    /// platform taking the working tree away in about thirty seconds, so the
    /// daemon spends them saving the session — see [`crate::stop`].
    ///
    /// Defaulted, and the default is a VM: every configuration written
    /// before this field existed describes one, and so does the developer
    /// machine the example config is written for.
    #[serde(default)]
    pub runtime: flyco_core::Runtime,
    /// Whose instance-metadata endpoint announces this machine's
    /// reclamation, when it holds capacity that can be reclaimed at all.
    ///
    /// Written by the provisioner on exactly the machines a notice can
    /// arrive for: interruptible capacity on a cloud provider. Absent on
    /// on-demand capacity, on hardware the user registered, and on a
    /// developer machine — and an absent one is a daemon that watches
    /// nothing, rather than one that probes three endpoints to find out
    /// whose machine it is on (see [`crate::spot`]).
    #[serde(default)]
    pub spot_provider: Option<flyco_core::CloudProviderKind>,
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
    /// What a composer message beginning with `!` is run by. Omitted uses
    /// `bash` with the defaults below.
    #[serde(default)]
    pub shell: ShellConfig,
    /// The user's registered MCP servers, as the control plane provisioned
    /// them.
    ///
    /// The whole set the session gets, beside flyco's own local server.
    /// [`crate::mount`] writes it into the harness's root-owned
    /// configuration, which is the allowlist: an agent cannot reach a
    /// server that is not here, and cannot add one.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerMount>,
    /// The session's desktop. A configuration without the table is a
    /// session without a screen.
    #[serde(default)]
    pub computer: ComputerConfig,
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
