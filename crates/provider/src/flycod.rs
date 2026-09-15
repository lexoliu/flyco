//! The `flycod` configuration a provisioned machine boots with.
//!
//! Every provider hands the daemon the same document — Azure writes it into
//! cloud-init, a host into the container's environment — so it is rendered
//! once, here, and both drivers embed the result.
//!
//! It is a serde structure serialized by [`toml`] rather than a template:
//! the daemon's own [`DaemonConfig`] is `deny_unknown_fields` and rejects the
//! whole file over one stray key, so the escaping and the layout have to be
//! the serializer's problem, not a template author's. What the serializer
//! cannot check is that the *shape* still matches what the daemon expects,
//! which is why `crates/daemon/tests/provisioned_config.rs` renders this and
//! parses it back with the daemon's own loader: a field renamed on either
//! side fails that test rather than a machine that boots and never phones
//! home.
//!
//! [`DaemonConfig`]: https://github.com/lexoliu/flyco/blob/main/crates/daemon/src/config.rs

use core::fmt;
use std::collections::BTreeMap;
use std::path::PathBuf;

use flyco_core::machine::SessionMachine;
use flyco_core::{
    BranchName, CloudProviderKind, DEVIN_ID_TAILS, DriverKind, HarnessKind, MachineOrigin,
    McpServerConfig, McpServerMount, PermissionMode, RepoSlug, Runtime, SessionId,
};
use serde::Serialize;

use crate::{DaemonBootstrap, GitIdentity, cloud_init, datetime};

/// Where the agent's checkout lives inside a flyco machine.
pub const WORKDIR: &str = "/srv/flyco/work";

/// Where a daemon with no control plane would keep its transcript. Present
/// because the field is required, unused because a provisioned machine
/// always has a control plane and keeps its transcript in R2.
pub const TRANSCRIPT_DIR: &str = "/var/lib/flyco/transcripts";

/// Where the Bun sidecar is materialized.
pub const SIDECAR_DIR: &str = "/var/lib/flyco/sidecar";

/// The isolated `CLAUDE_CONFIG_DIR` an injected credential runs under.
pub const CLAUDE_CONFIG_DIR: &str = "/var/lib/flyco/claude";

/// `CLAUDE_CODE_PROJECT_DIR_NAME`, the Agent SDK's project key.
///
/// It is what a session's transcript is filed under, and therefore what
/// cross-host resume joins on. Varying it with the machine would lose the
/// history on every move, which is why it is a constant.
pub const CLAUDE_PROJECT_DIR_NAME: &str = "flyco-session";

/// The isolated `CODEX_HOME` an injected Codex credential runs under.
pub const CODEX_HOME: &str = "/var/lib/flyco/codex";

/// Where the session image and the installer put the daemon binary.
///
/// Every harness's flyco MCP entry launches a second copy of it — the
/// daemon resolves its own path at runtime, but the root-owned registry
/// files are rendered here, on the control plane, where only the image's
/// install location is known.
pub const FLYCOD: &str = "/usr/local/bin/flycod";

/// The `XDG_DATA_HOME` a provisioned Devin runs under.
///
/// `devin` keeps its `credentials.toml` at `<XDG_DATA_HOME>/devin/`, so
/// pointing the variable at a flyco-owned root is what isolates one
/// session's login from everything else the CLI would read.
pub const DEVIN_DATA_HOME: &str = "/var/lib/flyco/devin";

/// The `XDG_CONFIG_HOME` a provisioned Devin runs under.
///
/// `mcp_config.json` lands at `<XDG_CONFIG_HOME>/devin/` — the registry
/// written through `[acp.files]` is the whole of what the agent sees at
/// that scope.
pub const DEVIN_CONFIG_HOME: &str = "/var/lib/flyco/devin-config";

/// Claude Code's managed-policy directory, declared with the release that
/// has to make it writable. An absolute path outside `CLAUDE_CONFIG_DIR` on
/// purpose: an isolated config tree is the session's, and this is the
/// machine's.
pub use flyco_core::release::CLAUDE_MANAGED_DIR;

/// How the supervised `claude` CLI authenticates on a provisioned machine.
///
/// [`Inherit`](Self::Inherit) is the developer-machine mode and is what a
/// machine gets when the user has linked no Claude account yet: the session
/// comes up, and the harness says it is unauthenticated, which is a better
/// failure than a machine that never provisions.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ClaudeCredential {
    /// No credential injected.
    Inherit,
    /// A Claude subscription OAuth token.
    OauthToken {
        /// Value for `CLAUDE_CODE_OAUTH_TOKEN`.
        token: String,
    },
    /// An Anthropic API key.
    ApiKey {
        /// Value for `ANTHROPIC_API_KEY`.
        key: String,
    },
}

impl fmt::Debug for ClaudeCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode = match self {
            Self::Inherit => "inherit",
            Self::OauthToken { .. } => "oauth_token",
            Self::ApiKey { .. } => "api_key",
        };
        f.debug_struct("ClaudeCredential")
            .field("mode", &mode)
            .finish_non_exhaustive()
    }
}

/// How the supervised `codex` CLI authenticates on a provisioned machine.
///
/// The two ways `codex` itself can be signed in, and nothing else: an
/// `OPENAI_API_KEY`, or the `ChatGPT` grant `codex login --device-auth`
/// produces. [`Inherit`](Self::Inherit) is the developer-machine mode, the
/// same as [`ClaudeCredential::Inherit`].
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum CodexCredential {
    /// No credential injected.
    Inherit,
    /// An `OpenAI` API key.
    ApiKey {
        /// Value written into `auth.json` as `OPENAI_API_KEY`.
        key: String,
    },
    /// A `ChatGPT` subscription grant from the device-code flow.
    ///
    /// All four values, because Codex's own `auth.json` holds all four: the
    /// access token alone authenticates nothing that outlives an hour, and
    /// the account id is the workspace every request is billed to.
    #[serde(rename = "chatgpt")]
    ChatGpt {
        /// The id token, a JWT naming the account.
        id_token: String,
        /// The bearer token the agent runs under.
        access_token: String,
        /// Redeemed by the control plane for the next set.
        refresh_token: String,
        /// `chatgpt_account_id`, the workspace the grant belongs to.
        account_id: String,
    },
}

impl fmt::Debug for CodexCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode = match self {
            Self::Inherit => "inherit",
            Self::ApiKey { .. } => "api_key",
            Self::ChatGpt { .. } => "chatgpt",
        };
        f.debug_struct("CodexCredential")
            .field("mode", &mode)
            .finish_non_exhaustive()
    }
}

/// How the supervised Devin agent authenticates on a provisioned machine.
///
/// Driven over ACP like every harness but Claude Code: credentials reach it
/// as files and environment under the `[acp]` table rather than as a
/// vendor-shaped home directory. [`Inherit`](Self::Inherit) is the
/// developer-machine mode — the agent uses whatever login the image
/// carries.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum DevinCredential {
    /// No credential injected.
    Inherit,
    /// A Devin API key, written as `credentials.toml`'s `windsurf_api_key`.
    ///
    /// The field's name is the CLI's own, predating the Devin branding —
    /// the file predates it too, and `devin` reads it verbatim.
    ApiKey {
        /// The key the user linked in the browser.
        key: String,
    },
}

impl fmt::Debug for DevinCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode = match self {
            Self::Inherit => "inherit",
            Self::ApiKey { .. } => "api_key",
        };
        f.debug_struct("DevinCredential")
            .field("mode", &mode)
            .finish_non_exhaustive()
    }
}

/// The credential a provisioned machine's harness runs under.
///
/// Tagged by harness rather than carried beside a separate `harness` field:
/// a Claude token on a Codex machine is not a mode to fall back from, it is
/// a bootstrap that cannot boot, and this is what makes it unspellable.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "harness", content = "auth", rename_all = "snake_case")]
pub enum HarnessCredential {
    /// Claude Code's credential.
    ClaudeCode(ClaudeCredential),
    /// Codex's credential.
    Codex(CodexCredential),
    /// Devin's credential.
    Devin(DevinCredential),
}

impl fmt::Debug for HarnessCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClaudeCode(credential) => f
                .debug_tuple("HarnessCredential::ClaudeCode")
                .field(credential)
                .finish(),
            Self::Codex(credential) => f
                .debug_tuple("HarnessCredential::Codex")
                .field(credential)
                .finish(),
            Self::Devin(credential) => f
                .debug_tuple("HarnessCredential::Devin")
                .field(credential)
                .finish(),
        }
    }
}

impl HarnessCredential {
    /// Which harness this credential drives.
    #[must_use]
    pub const fn harness(&self) -> HarnessKind {
        match self {
            Self::ClaudeCode(_) => HarnessKind::ClaudeCode,
            Self::Codex(_) => HarnessKind::Codex,
            Self::Devin(_) => HarnessKind::Devin,
        }
    }

    /// The credential a machine gets when the user has linked no account.
    #[must_use]
    pub const fn inherit(harness: HarnessKind) -> Self {
        match harness {
            HarnessKind::ClaudeCode => Self::ClaudeCode(ClaudeCredential::Inherit),
            HarnessKind::Codex => Self::Codex(CodexCredential::Inherit),
            HarnessKind::Devin => Self::Devin(DevinCredential::Inherit),
        }
    }
}

/// An isolated Claude configuration tree, as the daemon's config spells it.
#[derive(Debug, Clone, Copy, Serialize)]
struct Isolation {
    config_dir: &'static str,
    project_dir_name: &'static str,
}

/// The `[claude.auth]` table.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum Auth<'a> {
    Inherit,
    OauthToken {
        token: &'a str,
        isolation: Isolation,
    },
    ApiKey {
        key: &'a str,
        isolation: Isolation,
    },
}

/// The `[claude]` table.
#[derive(Debug, Clone, Serialize)]
struct Claude<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    /// Reasoning effort, omitted where the session chose none so the CLI's
    /// own default for the model stands.
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<&'a str>,
    permission_mode: PermissionMode,
    managed_dir: &'static str,
    auth: Auth<'a>,
}

/// The `[control_plane]` table.
#[derive(Debug, Clone, Serialize)]
struct ControlPlane<'a> {
    url: &'a str,
    daemon_token: &'a str,
}

/// The `[repo]` table, and `[repo.identity]` under it.
///
/// The token is a field of the same table as the slug because the two are
/// one decision: a checkout flyco cannot authenticate is not a checkout, and
/// a token with no repository to spend it on has no reason to be on the
/// machine at all.
#[derive(Debug, Clone, Serialize)]
struct Repo<'a> {
    slug: &'a RepoSlug,
    branch: &'a BranchName,
    token: &'a str,
    identity: &'a GitIdentity,
}

/// The `[sidecar]` table.
#[derive(Debug, Clone, Copy, Serialize)]
struct Sidecar {
    dir: &'static str,
    bun: &'static str,
}

/// One file of `[acp.files]`: credentials and managed configuration the
/// daemon materializes — mode `0600` — before spawning the agent.
#[derive(Debug, Clone, Serialize)]
struct AcpFile {
    path: PathBuf,
    contents: String,
}

/// One `session/set_config_option` write, as the daemon's `[acp]` table
/// spells it.
#[derive(Debug, Clone, Serialize)]
struct AcpOptionWrite {
    option: &'static str,
    value: &'static str,
}

/// How one flyco permission mode reaches the agent — a `session/set_mode`
/// id plus ordered config-option writes, as `[acp.modes.<mode>]`.
#[derive(Debug, Clone, Default, Serialize)]
struct AcpMode {
    #[serde(skip_serializing_if = "Option::is_none")]
    set_mode: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    options: Vec<AcpOptionWrite>,
}

impl AcpMode {
    /// A mode that is only a `set_mode` id.
    const fn mode(id: &'static str) -> Self {
        Self {
            set_mode: Some(id),
            options: Vec::new(),
        }
    }
}

/// One agent extension method and its params — `[acp.methods.<name>]`.
#[derive(Debug, Clone, Serialize)]
struct AcpMethodCall {
    call: &'static str,
    params: serde_json::Value,
}

/// The `[acp.methods]` table: the vendor extension methods the driver
/// calls, configured rather than assumed.
#[derive(Debug, Clone, Default, Serialize)]
struct AcpMethods {
    #[serde(skip_serializing_if = "Option::is_none")]
    compact: Option<AcpMethodCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<AcpMethodCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mcp_status: Option<AcpMethodCall>,
}

/// The `[acp.tui]` table: the agent's own interactive interface, bridged
/// to by `flyco <agent>`.
#[derive(Debug, Clone, Serialize)]
struct AcpTui {
    program: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    args: Vec<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    resume_args: Vec<&'static str>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<&'static str, &'static str>,
}

/// The `[acp]` table — the one shape every non-Claude harness renders
/// into, so the daemon's driver never learns which agent it is steering.
#[derive(Debug, Clone, Serialize)]
struct Acp<'a> {
    agent: &'static str,
    program: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    args: Vec<&'static str>,
    env: BTreeMap<&'static str, &'static str>,
    files: Vec<AcpFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<&'a str>,
    model_option: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    effort_option: Option<&'static str>,
    /// Suffixes the agent hangs after the effort word inside a fused
    /// model id — non-empty marks the ids as effort-fused.
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    fused_effort_tails: &'a [&'static str],
    permission_mode: PermissionMode,
    modes: BTreeMap<PermissionMode, AcpMode>,
    methods: AcpMethods,
    #[serde(skip_serializing_if = "Option::is_none")]
    tui: Option<AcpTui>,
}

/// The whole document.
///
/// Field order is the serialization order and TOML puts every scalar before
/// the first table, so the scalars come first here. Getting that wrong makes
/// `toml` refuse to serialize rather than emit an invalid document.
#[derive(Debug, Clone, Serialize)]
struct Document<'a> {
    session: SessionId,
    /// The driver the daemon runs — `acp` for every harness but Claude
    /// Code, because the product harness lives on the session row and the
    /// daemon only needs to know which machinery steers it.
    harness: DriverKind,
    workdir: &'static str,
    transcript_dir: &'static str,
    /// Whether this machine's filesystem outlives a stop.
    ///
    /// Always written, unlike [`Self::spot_provider`]: "the disk survives"
    /// is what the daemon assumes when nothing says otherwise, and a
    /// container whose configuration forgot to say so would lose the
    /// working tree the first time the platform stopped it.
    runtime: Runtime,
    machine_origin: MachineOrigin,
    #[serde(skip_serializing_if = "Option::is_none")]
    spot_provider: Option<CloudProviderKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resume_session_id: Option<&'a str>,
    control_plane: ControlPlane<'a>,
    repo: Repo<'a>,
    machine: &'a SessionMachine,
    #[serde(skip_serializing_if = "Option::is_none")]
    claude: Option<Claude<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sidecar: Option<Sidecar>,
    #[serde(skip_serializing_if = "Option::is_none")]
    acp: Option<Acp<'a>>,
    /// `[[mcp_servers]]`, last because an array of tables closes the
    /// document: everything after it in TOML would land inside its last
    /// element.
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    mcp_servers: &'a [flyco_core::McpServerMount],
}

/// Why a configuration could not be rendered.
#[derive(Debug, thiserror::Error)]
#[error("the flycod configuration could not be rendered as TOML: {0}")]
pub struct RenderError(#[from] toml::ser::Error);

/// The `[claude.auth]` table for one credential.
///
/// Credentials and isolation are one decision in the daemon's model:
/// injecting a token into a shared `~/.claude` would trample a real login,
/// so every credential-bearing mode carries its own tree and
/// [`Inherit`](ClaudeCredential::Inherit) carries none.
fn claude_auth(credential: &ClaudeCredential) -> Auth<'_> {
    let isolation = Isolation {
        config_dir: CLAUDE_CONFIG_DIR,
        project_dir_name: CLAUDE_PROJECT_DIR_NAME,
    };
    match credential {
        ClaudeCredential::Inherit => Auth::Inherit,
        ClaudeCredential::OauthToken { token } => Auth::OauthToken { token, isolation },
        ClaudeCredential::ApiKey { key } => Auth::ApiKey { key, isolation },
    }
}

/// `$CODEX_HOME/config.toml`, written into `[acp.files]`.
///
/// The file is the machine's MCP registry *and* its credential-store
/// setting: Codex has no separate allowlist document — `[mcp_servers.<id>]`
/// *is* the server's identity — so the complete set being here, in a home
/// the agent cannot write before flycod owns it, is the allowlist.
#[derive(Debug, Serialize)]
struct CodexHomeFile {
    cli_auth_credentials_store: &'static str,
    mcp_servers: BTreeMap<String, CodexMcpServer>,
}

/// One `[mcp_servers.<id>]` table of Codex's `config.toml`.
///
/// Field order is serialization order and TOML puts every scalar before
/// the first table it meets, so the maps come last or `toml` refuses to
/// serialize the document at all.
#[derive(Debug, Serialize)]
struct CodexMcpServer {
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    enabled: bool,
    required: bool,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    http_headers: BTreeMap<String, String>,
}

/// `$CODEX_HOME/auth.json`, as Codex's own loader reads it.
#[derive(Debug, Serialize)]
struct CodexAuthFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_mode: Option<&'static str>,
    #[serde(rename = "OPENAI_API_KEY", skip_serializing_if = "Option::is_none")]
    openai_api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tokens: Option<CodexTokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_refresh: Option<String>,
}

/// The `tokens` object of a `ChatGPT` `auth.json`.
#[derive(Debug, Serialize)]
struct CodexTokens {
    id_token: String,
    access_token: String,
    refresh_token: String,
    account_id: String,
}

/// `mcp_config.json`, as `devin` reads it at its `XDG_CONFIG_HOME`.
///
/// One `mcpServers` map — the same key Claude's files use, because Devin's
/// registry inherited the shape. Each entry spells its transport
/// explicitly; `stdio` servers carry `command`/`args`/`env`, remote ones
/// `url`/`headers`.
#[derive(Debug, Serialize)]
struct DevinMcpFile {
    #[serde(rename = "mcpServers")]
    mcp_servers: BTreeMap<String, DevinMcpServer>,
}

/// One `mcpServers` entry of Devin's `mcp_config.json`.
#[derive(Debug, Serialize)]
struct DevinMcpServer {
    transport: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    args: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    headers: BTreeMap<String, String>,
}

/// The flyco MCP server's own entry, in every vendor's registry file.
///
/// A second `flycod` process serves the session's tools; the path and the
/// config it reads are the image's contract, fixed by the entrypoint and
/// installer rather than resolved at runtime the way the daemon's own
/// mount declaration is.
fn flyco_entry() -> (String, McpServerConfig) {
    (
        "flyco".to_owned(),
        McpServerConfig::Stdio {
            command: FLYCOD.to_owned(),
            args: vec![
                "mcp".to_owned(),
                "--config".to_owned(),
                cloud_init::CONFIG_PATH.to_owned(),
            ],
            env: Vec::new(),
        },
    )
}

/// Every server the registry file lists: flyco's first, then the user's
/// mounts, keyed by name so a database's `ORDER BY` never shuffles the
/// rendered file.
fn registry(mounts: &[McpServerMount]) -> BTreeMap<String, McpServerConfig> {
    let mut servers = BTreeMap::from([flyco_entry()]);
    servers.extend(
        mounts
            .iter()
            .map(|mount| (mount.name.clone(), mount.config.clone())),
    );
    servers
}

/// One registered server as Codex's `config.toml` spells it; flyco's own
/// entry is marked `required` so a thread cannot open without it.
fn codex_mcp(name: &str, config: &McpServerConfig) -> CodexMcpServer {
    let required = name == "flyco";
    match config {
        McpServerConfig::Stdio { command, args, env } => CodexMcpServer {
            command: Some(command.clone()),
            args: args.clone(),
            url: None,
            enabled: true,
            required,
            env: env
                .iter()
                .map(|entry| (entry.key.clone(), entry.value.clone()))
                .collect(),
            http_headers: BTreeMap::new(),
        },
        McpServerConfig::Http { url, headers } => CodexMcpServer {
            command: None,
            args: Vec::new(),
            url: Some(url.clone()),
            enabled: true,
            required,
            env: BTreeMap::new(),
            http_headers: headers
                .iter()
                .map(|header| (header.name.clone(), header.value.clone()))
                .collect(),
        },
    }
}

/// One registered server as Devin's `mcp_config.json` spells it.
fn devin_mcp(config: &McpServerConfig) -> DevinMcpServer {
    match config {
        McpServerConfig::Stdio { command, args, env } => DevinMcpServer {
            transport: "stdio",
            command: Some(command.clone()),
            args: args.clone(),
            env: env
                .iter()
                .map(|entry| (entry.key.clone(), entry.value.clone()))
                .collect(),
            url: None,
            headers: BTreeMap::new(),
        },
        McpServerConfig::Http { url, headers } => DevinMcpServer {
            transport: "http",
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            url: Some(url.clone()),
            headers: headers
                .iter()
                .map(|header| (header.name.clone(), header.value.clone()))
                .collect(),
        },
    }
}

/// The contents of `$CODEX_HOME/config.toml` for one session.
fn codex_config_toml(mounts: &[McpServerMount]) -> String {
    let file = CodexHomeFile {
        cli_auth_credentials_store: "file",
        mcp_servers: registry(mounts)
            .iter()
            .map(|(name, config)| (name.clone(), codex_mcp(name, config)))
            .collect(),
    };
    toml::to_string_pretty(&file).expect("CodexHomeFile serializes")
}

/// The contents of `$CODEX_HOME/auth.json` for one credential — `None`
/// when the session injects nothing and the image's own login stands.
fn codex_auth_json(credential: &CodexCredential) -> Option<String> {
    let file = match credential {
        CodexCredential::Inherit => return None,
        CodexCredential::ApiKey { key } => CodexAuthFile {
            auth_mode: None,
            openai_api_key: Some(key.clone()),
            tokens: None,
            last_refresh: None,
        },
        CodexCredential::ChatGpt {
            id_token,
            access_token,
            refresh_token,
            account_id,
        } => CodexAuthFile {
            auth_mode: Some("chatgpt"),
            openai_api_key: None,
            tokens: Some(CodexTokens {
                id_token: id_token.clone(),
                access_token: access_token.clone(),
                refresh_token: refresh_token.clone(),
                account_id: account_id.clone(),
            }),
            // Codex reads this to decide how stale the grant is; the grant
            // was minted or renewed at provision time, so "now" is the
            // truth.
            last_refresh: Some(now_rfc3339()),
        },
    };
    Some(serde_json::to_string_pretty(&file).expect("CodexAuthFile serializes"))
}

/// The current instant as Codex writes `last_refresh`: RFC 3339, UTC.
fn now_rfc3339() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is set before the Unix epoch")
        .as_secs();
    datetime::rfc3339(seconds).expect("a current timestamp is representable")
}

/// The contents of Devin's `credentials.toml` for one credential — `None`
/// when the session injects nothing.
fn devin_credentials_toml(credential: &DevinCredential) -> Option<String> {
    match credential {
        DevinCredential::Inherit => None,
        DevinCredential::ApiKey { key } => Some(
            toml::to_string(&toml::map::Map::from_iter([(
                "windsurf_api_key".to_owned(),
                toml::Value::String(key.clone()),
            )]))
            .expect("a one-key table serializes"),
        ),
    }
}

/// The contents of Devin's `mcp_config.json` for one session.
fn devin_mcp_config_json(mounts: &[McpServerMount]) -> String {
    let file = DevinMcpFile {
        mcp_servers: registry(mounts)
            .iter()
            .map(|(name, config)| (name.clone(), devin_mcp(config)))
            .collect(),
    };
    serde_json::to_string_pretty(&file).expect("DevinMcpFile serializes")
}

/// How flyco's permission modes reach Codex through the `codex-acp`
/// adapter.
///
/// Codex's ACP surface has two dials: the session mode — `read-only`,
/// `agent`, `agent-full-access` — and a `collaboration_mode` config option
/// whose `plan` value is what planning actually is on this agent. The
/// pairings follow the ones the retired app-server driver encoded: every
/// mutation asking reads as the read-only mode, workspace-write reads as
/// `agent`, and full access is reserved for `bypassPermissions`.
fn codex_modes() -> BTreeMap<PermissionMode, AcpMode> {
    BTreeMap::from([
        (PermissionMode::Default, AcpMode::mode("read-only")),
        (PermissionMode::AcceptEdits, AcpMode::mode("agent")),
        (
            PermissionMode::Plan,
            AcpMode {
                set_mode: Some("read-only"),
                options: vec![AcpOptionWrite {
                    option: "collaboration_mode",
                    value: "plan",
                }],
            },
        ),
        (PermissionMode::Auto, AcpMode::mode("agent")),
        (
            PermissionMode::BypassPermissions,
            AcpMode::mode("agent-full-access"),
        ),
        (PermissionMode::DontAsk, AcpMode::mode("read-only")),
    ])
}

/// The app-server extension methods the `codex-acp` adapter forwards.
fn codex_methods() -> AcpMethods {
    let call = |call: &'static str, params: serde_json::Value| Some(AcpMethodCall { call, params });
    AcpMethods {
        compact: call(
            "thread/compact/start",
            serde_json::json!({ "threadId": "<session>" }),
        ),
        usage: call("account/rateLimits/read", serde_json::json!({})),
        mcp_status: call(
            "mcpServerStatus/list",
            serde_json::json!({ "threadId": "<session>" }),
        ),
    }
}

/// How flyco's permission modes reach Devin — a near-verbatim mapping,
/// because Devin's mode names are Claude's own vocabulary. `dontAsk` has
/// no counterpart: nothing on this agent both refuses mutation and never
/// asks, so it shares `plan`'s read-only floor.
fn devin_modes() -> BTreeMap<PermissionMode, AcpMode> {
    BTreeMap::from([
        (PermissionMode::Default, AcpMode::mode("ask")),
        (PermissionMode::AcceptEdits, AcpMode::mode("accept-edits")),
        (PermissionMode::Plan, AcpMode::mode("plan")),
        (PermissionMode::Auto, AcpMode::mode("smart")),
        (PermissionMode::BypassPermissions, AcpMode::mode("bypass")),
        (PermissionMode::DontAsk, AcpMode::mode("plan")),
    ])
}

/// The `[acp]` table a Codex session is rendered into.
fn codex_acp<'a>(
    credential: &CodexCredential,
    model: &'a str,
    effort: Option<&'a str>,
    permission_mode: PermissionMode,
    mounts: &[McpServerMount],
) -> Acp<'a> {
    let home = PathBuf::from(CODEX_HOME);
    let mut files = vec![AcpFile {
        path: home.join("config.toml"),
        contents: codex_config_toml(mounts),
    }];
    files.extend(codex_auth_json(credential).map(|contents| AcpFile {
        path: home.join("auth.json"),
        contents,
    }));
    Acp {
        agent: "codex",
        // The `@agentclientprotocol/codex-acp` adapter, which owns the
        // `codex app-server` subprocess it spawns through `CODEX_PATH`.
        program: "codex-acp",
        args: Vec::new(),
        env: BTreeMap::from([
            ("CODEX_PATH", "/usr/local/bin/codex"),
            ("CODEX_HOME", CODEX_HOME),
        ]),
        files,
        model: Some(model),
        effort,
        model_option: "model",
        effort_option: Some("reasoning_effort"),
        fused_effort_tails: &[],
        permission_mode,
        modes: codex_modes(),
        methods: codex_methods(),
        tui: Some(AcpTui {
            program: "codex",
            args: Vec::new(),
            // `codex resume <id>`; the id is substituted at launch, and a
            // launch with no recorded id drops the placeholder and lands
            // on the picker.
            resume_args: vec!["resume", "{session}"],
            env: BTreeMap::from([("CODEX_HOME", CODEX_HOME)]),
        }),
    }
}

/// The `[acp]` table a Devin session is rendered into.
fn devin_acp<'a>(
    credential: &DevinCredential,
    model: &'a str,
    effort: Option<&'a str>,
    permission_mode: PermissionMode,
    mounts: &[McpServerMount],
) -> Acp<'a> {
    let data = PathBuf::from(DEVIN_DATA_HOME);
    let config = PathBuf::from(DEVIN_CONFIG_HOME);
    let env = BTreeMap::from([
        ("XDG_DATA_HOME", DEVIN_DATA_HOME),
        ("XDG_CONFIG_HOME", DEVIN_CONFIG_HOME),
    ]);
    let mut files = vec![AcpFile {
        path: config.join("devin/mcp_config.json"),
        contents: devin_mcp_config_json(mounts),
    }];
    files.extend(devin_credentials_toml(credential).map(|contents| AcpFile {
        path: data.join("devin/credentials.toml"),
        contents,
    }));
    Acp {
        agent: "devin",
        program: "devin",
        args: vec!["acp"],
        env,
        files,
        model: Some(model),
        // Devin has no effort dial: the level is part of the model id,
        // folded back in by the daemon through `fused_effort_tails`.
        effort,
        model_option: "model",
        effort_option: None,
        fused_effort_tails: &DEVIN_ID_TAILS,
        permission_mode,
        modes: devin_modes(),
        // No extension methods are configured: Devin's vendor surface is
        // notifications, which the mount watch reads, not calls to make.
        methods: AcpMethods::default(),
        tui: Some(AcpTui {
            program: "devin",
            args: Vec::new(),
            // `devin --resume <id>`; bare `--resume` opens the picker.
            resume_args: vec!["--resume", "{session}"],
            env: BTreeMap::from([
                ("XDG_DATA_HOME", DEVIN_DATA_HOME),
                ("XDG_CONFIG_HOME", DEVIN_CONFIG_HOME),
            ]),
        }),
    }
}

/// Which provider's metadata endpoint this machine's daemon must watch for
/// an eviction notice, if any.
///
/// Two conditions, and both are load-bearing. On-demand capacity is never
/// reclaimed, so a daemon polling for a notice that cannot arrive would be
/// a request a second, for the life of the session, against an endpoint
/// that has nothing to say. And a container on hardware the user
/// registered has no instance metadata at all: it is started and stopped
/// by its owner, and there is no notice to watch for.
const fn spot_provider(bootstrap: &DaemonBootstrap) -> Option<CloudProviderKind> {
    match bootstrap.provider {
        // Codespaces has no capacity market to be evicted from — GitHub's
        // idle suspension arrives with no metadata endpoint to watch and is
        // reconciled by the control plane instead.
        CloudProviderKind::Host | CloudProviderKind::Codespaces => None,
        provider @ (CloudProviderKind::Azure | CloudProviderKind::Aws | CloudProviderKind::Gcp) => {
            if bootstrap.machine.spot {
                Some(provider)
            } else {
                None
            }
        }
    }
}

/// Renders the configuration a machine's `flycod` boots with.
///
/// # Errors
///
/// Returns [`RenderError`] if the document does not serialize, which would
/// mean this module's own structure is malformed rather than anything the
/// caller did.
pub fn render(bootstrap: &DaemonBootstrap) -> Result<String, RenderError> {
    // The session's model reaches exactly one of the two tables, because
    // exactly one harness is being configured — and it reaches the one the
    // credential names, so a Codex model can never land in `[claude]`.
    let model = bootstrap.model.model.as_str();
    let effort = bootstrap.model.effort.as_deref();
    let (claude, sidecar, acp) = match &bootstrap.auth {
        HarnessCredential::ClaudeCode(credential) => (
            Some(Claude {
                model: Some(model),
                effort,
                permission_mode: bootstrap.permission_mode,
                managed_dir: CLAUDE_MANAGED_DIR,
                auth: claude_auth(credential),
            }),
            Some(Sidecar {
                dir: SIDECAR_DIR,
                bun: "bun",
            }),
            None,
        ),
        HarnessCredential::Codex(credential) => (
            None,
            None,
            Some(codex_acp(
                credential,
                model,
                effort,
                bootstrap.permission_mode,
                &bootstrap.mcp_servers,
            )),
        ),
        HarnessCredential::Devin(credential) => (
            None,
            None,
            Some(devin_acp(
                credential,
                model,
                effort,
                bootstrap.permission_mode,
                &bootstrap.mcp_servers,
            )),
        ),
    };

    let driver = match bootstrap.auth.harness() {
        HarnessKind::ClaudeCode => DriverKind::ClaudeCode,
        HarnessKind::Codex | HarnessKind::Devin => DriverKind::Acp,
    };
    let document = Document {
        session: bootstrap.session,
        harness: driver,
        workdir: WORKDIR,
        transcript_dir: TRANSCRIPT_DIR,
        runtime: bootstrap.runtime,
        machine_origin: bootstrap.machine_origin,
        spot_provider: spot_provider(bootstrap),
        resume_session_id: bootstrap.resume_session_id.as_deref(),
        control_plane: ControlPlane {
            url: &bootstrap.control_plane_url,
            daemon_token: &bootstrap.daemon_token,
        },
        repo: Repo {
            slug: &bootstrap.repo.slug,
            branch: &bootstrap.repo.branch,
            token: &bootstrap.repo.token,
            identity: &bootstrap.repo.identity,
        },
        machine: &bootstrap.machine,
        claude,
        sidecar,
        acp,
        mcp_servers: &bootstrap.mcp_servers,
    };

    Ok(toml::to_string_pretty(&document)?)
}

#[cfg(test)]
mod tests {
    use flyco_core::{
        CloudProviderKind, HarnessKind, MachineOrigin, PermissionMode, Runtime, SessionId,
    };

    use super::{
        CLAUDE_CONFIG_DIR, CODEX_HOME, ClaudeCredential, CodexCredential, HarnessCredential, render,
    };
    use crate::DaemonBootstrap;
    use crate::testing::{GITHUB_TOKEN, checkout};

    fn bootstrap(auth: HarnessCredential) -> DaemonBootstrap {
        DaemonBootstrap {
            session: SessionId::generate(),
            provider: CloudProviderKind::Azure,
            runtime: Runtime::Vm,
            control_plane_url: "https://flyco.dev/".to_owned(),
            daemon_token: "fd_token".to_owned(),
            permission_mode: PermissionMode::Default,
            auth,
            repo: checkout(),
            machine_origin: MachineOrigin::Auto,
            machine: crate::testing::session_machine(),
            resume_session_id: None,
            model: crate::testing::session_model(),
            mcp_servers: crate::testing::mcp_servers(),
        }
    }

    fn claude(credential: ClaudeCredential) -> DaemonBootstrap {
        bootstrap(HarnessCredential::ClaudeCode(credential))
    }

    fn codex(credential: CodexCredential) -> DaemonBootstrap {
        bootstrap(HarnessCredential::Codex(credential))
    }

    /// The grant the device-code flow hands over, as the daemon receives it.
    fn chatgpt() -> CodexCredential {
        CodexCredential::ChatGpt {
            id_token: "header.payload.signature".to_owned(),
            access_token: "chatgpt-access".to_owned(),
            refresh_token: "chatgpt-refresh".to_owned(),
            account_id: "acc_01JD".to_owned(),
        }
    }

    #[test]
    fn a_provisioned_config_names_the_control_plane_and_the_token() {
        let rendered = render(&claude(ClaudeCredential::Inherit)).expect("render");
        assert!(rendered.contains("[control_plane]"));
        assert!(rendered.contains("url = \"https://flyco.dev/\""));
        assert!(rendered.contains("daemon_token = \"fd_token\""));
        assert!(rendered.contains("permission_mode = \"default\""));
        assert!(rendered.contains("mode = \"inherit\""));
    }

    #[test]
    fn a_provisioned_config_states_whether_the_disk_survives_a_stop() {
        // Always written, on both runtimes: "the disk survives" is what the
        // daemon assumes when nothing says otherwise, so a container whose
        // configuration forgot to say so would lose the working tree the
        // first time the platform stopped it.
        let vm = render(&claude(ClaudeCredential::Inherit)).expect("render");
        assert!(vm.contains("runtime = \"vm\""));

        let container = render(&DaemonBootstrap {
            runtime: Runtime::Container,
            ..claude(ClaudeCredential::Inherit)
        })
        .expect("render");
        assert!(container.contains("runtime = \"container\""));
    }

    #[test]
    fn an_injected_credential_always_carries_its_own_config_tree() {
        let rendered = render(&claude(ClaudeCredential::OauthToken {
            token: "sk-ant-oat01-x".to_owned(),
        }))
        .expect("render");

        assert!(rendered.contains("mode = \"oauth_token\""));
        assert!(rendered.contains(CLAUDE_CONFIG_DIR));
    }

    #[test]
    fn a_credential_never_shows_up_in_a_debug_rendering() {
        let credential = HarnessCredential::ClaudeCode(ClaudeCredential::ApiKey {
            key: "sk-ant-secret".to_owned(),
        });
        assert!(!format!("{credential:?}").contains("sk-ant-secret"));
        assert!(!format!("{:?}", HarnessCredential::Codex(chatgpt())).contains("chatgpt-refresh"));
    }

    #[test]
    fn the_machine_is_told_which_repository_and_branch_to_check_out() {
        let rendered = render(&claude(ClaudeCredential::Inherit)).expect("render");

        assert!(rendered.contains("[repo]"));
        assert!(rendered.contains("slug = \"lexoliu/flyco\""));
        assert!(rendered.contains("branch = \"dev\""));
        assert!(rendered.contains("[repo.identity]"));
        assert!(rendered.contains("email = \"4242+lexoliu@users.noreply.github.com\""));
    }

    #[test]
    fn the_github_token_never_shows_up_in_a_debug_rendering() {
        // The bootstrap is what a driver traces while it is being debugged,
        // and it now carries a live GitHub token as well as two other
        // credentials. None of the three may survive a `{:?}`.
        let bootstrap = claude(ClaudeCredential::OauthToken {
            token: "sk-ant-oat01-x".to_owned(),
        });
        let debugged = format!("{bootstrap:?}");

        assert!(!debugged.contains(GITHUB_TOKEN));
        assert!(!debugged.contains("sk-ant-oat01-x"));
        assert!(!debugged.contains("fd_token"));
        // What is left is still enough to tell two bootstraps apart.
        assert!(debugged.contains("lexoliu/flyco"));
    }

    #[test]
    fn a_codex_session_writes_the_acp_table_and_not_claude() {
        let rendered = render(&codex(chatgpt())).expect("render");

        assert!(rendered.contains("[acp]"));
        assert!(rendered.contains("harness = \"acp\""));
        assert!(rendered.contains(r#"agent = "codex""#));
        assert!(rendered.contains(r#"program = "codex-acp""#));
        assert!(rendered.contains("permission_mode = \"default\""));
        assert!(rendered.contains(CODEX_HOME));
        assert!(!rendered.contains("[claude]"));
        assert!(!rendered.contains("[sidecar]"));
    }

    /// The four values Codex's own `auth.json` is written from.
    #[test]
    fn a_chatgpt_grant_carries_every_value_auth_json_needs() {
        let rendered = render(&codex(chatgpt())).expect("render");

        assert!(rendered.contains("auth.json"));
        assert!(rendered.contains(r#""auth_mode": "chatgpt""#), "{rendered}");
        assert!(rendered.contains("header.payload.signature"), "{rendered}");
        assert!(rendered.contains("chatgpt-access"), "{rendered}");
        assert!(rendered.contains("chatgpt-refresh"), "{rendered}");
        assert!(rendered.contains("acc_01JD"), "{rendered}");
    }

    #[test]
    fn a_codex_api_key_is_the_other_codex_mode() {
        let rendered = render(&codex(CodexCredential::ApiKey {
            key: "sk-proj-openai".to_owned(),
        }))
        .expect("render");

        assert!(rendered.contains("auth.json"));
        assert!(rendered.contains("OPENAI_API_KEY"), "{rendered}");
        assert!(rendered.contains("sk-proj-openai"), "{rendered}");
        assert!(rendered.contains(CODEX_HOME));
    }

    #[test]
    fn a_devin_session_is_the_same_acp_table_pointed_at_devin() {
        let rendered = render(&bootstrap(HarnessCredential::Devin(
            super::DevinCredential::ApiKey {
                key: "dv_test_key".to_owned(),
            },
        )))
        .expect("render");

        assert!(rendered.contains("harness = \"acp\""));
        assert!(rendered.contains(r#"agent = "devin""#), "{rendered}");
        assert!(rendered.contains(r#"program = "devin""#), "{rendered}");
        assert!(rendered.contains("credentials.toml"), "{rendered}");
        assert!(rendered.contains("windsurf_api_key"), "{rendered}");
        assert!(rendered.contains("dv_test_key"), "{rendered}");
        assert!(rendered.contains("mcp_config.json"), "{rendered}");
        assert!(!rendered.contains("[claude]"), "{rendered}");
    }

    #[test]
    fn a_credential_decides_the_harness_the_daemon_drives() {
        assert_eq!(
            HarnessCredential::Codex(CodexCredential::Inherit).harness(),
            HarnessKind::Codex
        );
        assert_eq!(
            HarnessCredential::inherit(HarnessKind::ClaudeCode).harness(),
            HarnessKind::ClaudeCode
        );
    }

    #[test]
    fn the_machine_the_agent_is_told_about_is_written_with_who_chose_it() {
        let mut chosen = claude(ClaudeCredential::Inherit);
        chosen.machine_origin = MachineOrigin::User;
        chosen.machine.minimum = Some(flyco_core::BillingMinimum::new(
            24,
            flyco_core::Usd::from_cents(65),
        ));
        let rendered = render(&chosen).expect("render");

        assert!(rendered.contains("machine_origin = \"user\""));
        assert!(rendered.contains("[machine]"));
        assert!(rendered.contains("machine_type = \"Standard_D4s_v6\""));
        assert!(rendered.contains("[machine.minimum]"));
        assert!(rendered.contains("hours = 24"));
    }

    #[test]
    fn a_machine_with_nothing_to_omit_writes_no_empty_facts() {
        // TOML has no null: an absent capacity or minimum has to be an
        // absent key, and a `None` reaching the serializer is a render
        // failure rather than a document the daemon would reject.
        let mut unknown = claude(ClaudeCredential::Inherit);
        unknown.machine.capacity = None;
        unknown.machine.hourly = None;
        let rendered = render(&unknown).expect("render");

        assert!(!rendered.contains("capacity"));
        assert!(!rendered.contains("hourly"));
    }

    #[test]
    fn interruptible_capacity_tells_the_daemon_whose_metadata_to_watch() {
        // The daemon reads its eviction notice off the provider's own
        // instance-metadata endpoint, and nothing else on the machine says
        // whose machine it is.
        let rendered = render(&claude(ClaudeCredential::Inherit)).expect("render");
        assert!(rendered.contains("spot_provider = \"azure\""));

        let mut on_gcp = claude(ClaudeCredential::Inherit);
        on_gcp.provider = CloudProviderKind::Gcp;
        assert!(
            render(&on_gcp)
                .expect("render")
                .contains("spot_provider = \"gcp\"")
        );
    }

    #[test]
    fn a_machine_that_cannot_be_reclaimed_watches_nothing() {
        // On-demand capacity is never taken back, and hardware the user
        // registered has no instance metadata to watch at all. Either way a
        // poll every second for the life of the session would be a request
        // against an endpoint with nothing to say.
        let mut on_demand = claude(ClaudeCredential::Inherit);
        on_demand.machine.spot = false;
        assert!(
            !render(&on_demand)
                .expect("render")
                .contains("spot_provider")
        );

        let mut owned = claude(ClaudeCredential::Inherit);
        owned.provider = CloudProviderKind::Host;
        assert!(!render(&owned).expect("render").contains("spot_provider"));
    }

    #[test]
    fn a_resume_id_is_written_only_when_there_is_one() {
        let mut with_resume = claude(ClaudeCredential::Inherit);
        with_resume.resume_session_id = Some("1f6d2c50-8a4b-4a2b-9f6d-2c508a4b4a2b".to_owned());

        assert!(
            !render(&claude(ClaudeCredential::Inherit))
                .expect("render")
                .contains("resume_session_id")
        );
        assert!(
            render(&with_resume)
                .expect("render")
                .contains("resume_session_id")
        );
    }

    #[test]
    fn the_session_model_lands_in_the_table_the_daemon_reads_it_from() {
        // One harness is configured, so one table carries the model. The
        // effort sits beside it rather than inside the identifier, because
        // the daemon reads them as two settings.
        let rendered = render(&claude(ClaudeCredential::OauthToken {
            token: "sk-ant-oat01-test".to_owned(),
        }))
        .expect("render");
        assert!(rendered.contains("[claude]"));
        assert!(rendered.contains(r#"model = "sonnet""#), "{rendered}");
        assert!(rendered.contains(r#"effort = "high""#), "{rendered}");

        let rendered = render(&codex(chatgpt())).expect("render");
        assert!(rendered.contains("[acp]"));
        assert!(rendered.contains(r#"model = "sonnet""#), "{rendered}");
        assert!(rendered.contains(r#"effort = "high""#), "{rendered}");
    }

    #[test]
    fn a_session_that_chose_no_effort_writes_none() {
        // Absent rather than empty: the harness's own default for the model
        // is the answer, and an `effort = ""` would be flyco asserting a
        // level nobody picked.
        let mut with_no_effort = claude(ClaudeCredential::OauthToken {
            token: "sk-ant-oat01-test".to_owned(),
        });
        with_no_effort.model.effort = None;
        let rendered = render(&with_no_effort).expect("render");
        assert!(rendered.contains(r#"model = "sonnet""#), "{rendered}");
        assert!(!rendered.contains("effort ="), "{rendered}");
    }
}
