//! `flycod`'s on-disk configuration.
//!
//! One TOML file, deserialized with `deny_unknown_fields` throughout: a
//! typo in a session VM's config is a provisioning bug, and the daemon says
//! so at startup rather than running with a silently ignored setting.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use flyco_core::{
    BranchName, DAEMON_TOKEN_PREFIX, DriverKind, MachineOrigin, McpServerMount, RepoSlug,
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

/// A file the daemon writes before spawning an ACP agent.
///
/// Credentials and managed configuration reach an ACP agent as files —
/// Codex's `auth.json`, Devin's `credentials.toml`, an MCP registry — so
/// the `[acp]` table carries them as content rather than as a shape per
/// vendor. Everything here is written mode `0600`: the point of the field
/// is secrets, and a secret file's permissions are not a choice.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpFile {
    /// Absolute path to write.
    pub path: PathBuf,
    /// The file's exact contents.
    pub contents: String,
}

/// One config-option write on the agent's `session/set_config_option`.
///
/// A list rather than a map because writes can be order-dependent: Codex's
/// plan mode is a `collaboration_mode` write that reads differently
/// depending on the `mode` beside it, so the provisioner picks the order
/// and the daemon keeps it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpOptionWrite {
    /// The config option's id.
    pub option: String,
    /// The value to select.
    pub value: String,
}

/// How one flyco [`PermissionMode`] is expressed to an ACP agent.
///
/// ACP has two related dials — `session/set_mode` and
/// `session/set_config_option` — and agents expose their permission
/// surface through either or both. `set_mode` runs first, then the
/// option writes in order; an entry naming neither is a mode the agent
/// simply does not have, which the daemon refuses rather than approximates.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpMode {
    /// `session/set_mode` mode id, when the agent exposes modes.
    #[serde(default)]
    pub set_mode: Option<String>,
    /// Ordered `session/set_config_option` writes.
    #[serde(default)]
    pub options: Vec<AcpOptionWrite>,
}

/// How an ACP session's harness TUI is launched, when the agent has one.
///
/// Optional because the protocol carries no terminal surface: `devin` and
/// `codex` have interactive TUIs a `flyco devin`/`flyco codex` bridges to,
/// an arbitrary ACP agent may have nothing to launch.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpTui {
    /// The executable. A bare name is resolved through `PATH`.
    pub program: PathBuf,
    /// Arguments a fresh launch takes.
    #[serde(default)]
    pub args: Vec<String>,
    /// Arguments re-entering the session's own conversation takes.
    ///
    /// Distinct from `args` for the same reason Claude's `--resume <id>`
    /// is: a conversation that can be re-entered is what makes
    /// `flyco resume` more than a new window.
    #[serde(default)]
    pub resume_args: Vec<String>,
    /// Extra environment for the child, on top of the daemon's own.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Settings for the generic ACP driver — `[acp]`.
///
/// Everything the driver needs to spawn and steer one agent process:
/// which program, under which environment, with which files in place, and
/// how flyco's session vocabulary — model, effort, permission mode —
/// maps onto the agent's config options. Provisioned Codex and Devin
/// sessions are both rendered into this one table; a hand-written config
/// pointing `program` at any other ACP agent drives it identically.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpConfig {
    /// What to call the agent in logs and diagnostics (`"codex"`,
    /// `"devin"`, or whatever a hand-written config is driving).
    pub agent: String,
    /// The ACP server executable. A bare name is resolved through `PATH`.
    pub program: PathBuf,
    /// Arguments the program is spawned with (`["acp"]` for `devin acp`).
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment for the agent process, on top of the daemon's
    /// own. Where credentials that travel as variables are injected —
    /// `CODEX_HOME`, `XDG_DATA_HOME` and friends are values here.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Files materialized before the agent is spawned: credentials and
    /// managed configuration, every one mode `0600`.
    #[serde(default)]
    pub files: Vec<AcpFile>,
    /// Model to select at session start, written to
    /// [`model_option`](Self::model_option). Omitted leaves the agent's
    /// own default in place.
    #[serde(default)]
    pub model: Option<String>,
    /// Effort level to select at session start, written to
    /// [`effort_option`](Self::effort_option).
    #[serde(default)]
    pub effort: Option<String>,
    /// Id of the config option that carries the model — `"model"` on both
    /// provisioned agents, configured rather than assumed because ACP does
    /// not reserve the id.
    #[serde(default = "default_model_option")]
    pub model_option: String,
    /// Id of the config option that carries the effort level, where the
    /// agent keeps one — Codex's `reasoning_effort`. `None` on an agent
    /// like Devin whose model ids carry the effort; a configured `effort`
    /// then fails loudly rather than being dropped.
    #[serde(default)]
    pub effort_option: Option<String>,
    /// Permission mode the session opens under, expressed through
    /// [`modes`](Self::modes).
    pub permission_mode: PermissionMode,
    /// How each flyco [`PermissionMode`] reaches this agent. A mode with
    /// no entry is one the agent cannot express: asking for it fails with
    /// the name of the mode rather than silently running under another.
    #[serde(default)]
    pub modes: BTreeMap<PermissionMode, AcpMode>,
    /// Agent extension methods the driver calls, configured rather than
    /// assumed because none of them are ACP. Each entry names a JSON-RPC
    /// method and the params object to send; a `"<session>"` string in the
    /// params is replaced by the live session id, which is how
    /// `thread/compact/start` gets its `threadId` and
    /// `mcpServerStatus/list` its page parameters.
    #[serde(default)]
    pub methods: AcpMethods,
    /// The agent's interactive TUI, when it has one `flyco <agent>` can
    /// bridge to.
    #[serde(default)]
    pub tui: Option<AcpTui>,
}

/// One agent extension method and the params to send it.
///
/// Params are a template rather than a literal because the one value every
/// such method wants — which session to act on — only exists at runtime:
/// a string field reading exactly `"<session>"` is substituted with the
/// session id wherever it appears in the object.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpMethodCall {
    /// The JSON-RPC method name, e.g. `"thread/compact/start"`.
    pub call: String,
    /// The params object to send. Defaults to no params.
    #[serde(default)]
    pub params: serde_json::Value,
}

/// The agent extension methods the driver knows how to use.
///
/// ACP standardizes the conversation, not the instrumentation around it:
/// compaction, plan-usage metering and MCP introspection are vendor
/// methods, and the provisioner that knows an agent's surface writes them
/// here. An agent without an entry simply lacks the feature — compacting
/// a session on an agent with no `compact` answers that plainly.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpMethods {
    /// Compacts the conversation's context — Codex's
    /// `thread/compact/start` through the adapter.
    #[serde(default)]
    pub compact: Option<AcpMethodCall>,
    /// Answers the account's plan-usage windows — Codex's
    /// `account/rateLimits/read`.
    #[serde(default)]
    pub usage: Option<AcpMethodCall>,
    /// Answers the mounted MCP servers' status — Codex's
    /// `mcpServerStatus/list`. With no method configured the mount is
    /// observed rather than asked: the driver watches the agent's own
    /// notifications for evidence the flyco server connected, and logs a
    /// mount it can neither prove nor disprove.
    #[serde(default)]
    pub mcp_status: Option<AcpMethodCall>,
}

/// `"model"`, the config-option id both provisioned ACP agents carry their
/// model selection under.
fn default_model_option() -> String {
    "model".to_owned()
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

/// Everything `flycod run` needs.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    /// The flyco session this daemon serves.
    pub session: SessionId,
    /// Which driver supervises the agent: `claude_code` for the Agent SDK
    /// sidecar, `acp` for the generic driver every other harness takes.
    pub harness: DriverKind,
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
    /// [`DriverKind::ClaudeCode`].
    #[serde(default)]
    pub claude: Option<ClaudeConfig>,
    /// Where the Bun sidecar is materialized and how Bun is run. Required
    /// when [`harness`](Self::harness) is [`DriverKind::ClaudeCode`].
    #[serde(default)]
    pub sidecar: Option<SidecarConfig>,
    /// ACP settings. Required when [`harness`](Self::harness) is
    /// [`DriverKind::Acp`].
    #[serde(default)]
    pub acp: Option<AcpConfig>,
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

    /// Returns the ACP settings, which an `acp` session always has.
    ///
    /// # Panics
    ///
    /// Panics if [`load`](Self::load) admitted an ACP session without an
    /// `[acp]` table — that combination is unrepresentable after load.
    #[must_use]
    pub const fn acp(&self) -> &AcpConfig {
        self.acp
            .as_ref()
            .expect("a loaded ACP config has an [acp] table")
    }

    const fn validate_harness(&self) -> Result<(), ConfigError> {
        match self.harness {
            DriverKind::ClaudeCode => {
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
            DriverKind::Acp => {
                if self.acp.is_none() {
                    return Err(ConfigError::WrongHarness(
                        "an acp session requires an [acp] table",
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
    use flyco_core::DriverKind;

    fn parse(text: &str) -> Result<DaemonConfig, toml::de::Error> {
        toml::from_str(text)
    }

    #[test]
    fn the_shipped_example_is_a_valid_config() {
        let config = parse(EXAMPLE).expect("the example config must parse");
        assert_eq!(config.harness, DriverKind::ClaudeCode);
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
