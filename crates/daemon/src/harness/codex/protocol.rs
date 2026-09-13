//! The subset of `codex app-server` JSON-RPC that flycod speaks.
//!
//! Framing is newline-delimited JSON with **no** `"jsonrpc":"2.0"` field.
//! Discriminate by field presence: `{id,method}` is a request, `{method}` a
//! notification, `{id,result}` a response, `{id,error}` an error. Request
//! ids are a string or an integer. Ground truth is
//! `docs/research/codex-app-server.md`, not the app-server README.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC request identifier: a string or an integer, never a float.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// Numeric id, which is what flycod mints.
    Number(u64),
    /// String id, which a server may use for its own requests.
    Text(String),
}

impl RequestId {
    /// A numeric id flycod minted.
    #[must_use]
    pub const fn number(n: u64) -> Self {
        Self::Number(n)
    }
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcError {
    /// JSON-RPC error code.
    pub code: i64,
    /// Human-readable message.
    pub message: String,
    /// Optional structured data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// One framed message on the app-server's stdio.
///
/// Serialized without a `jsonrpc` field, matching the app-server's wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Envelope {
    /// Client or server request that expects a response.
    Request {
        /// Correlation id.
        id: RequestId,
        /// Method name.
        method: String,
        /// Parameters, omitted when empty.
        params: Value,
    },
    /// One-way notification.
    Notification {
        /// Method name.
        method: String,
        /// Parameters, omitted when empty.
        params: Value,
    },
    /// Successful response.
    Response {
        /// Correlation id of the request.
        id: RequestId,
        /// Result payload.
        result: Value,
    },
    /// Failed response.
    Error {
        /// Correlation id of the request.
        id: RequestId,
        /// The error.
        error: RpcError,
    },
}

impl Envelope {
    /// A request with the given method and params.
    #[must_use]
    pub fn request(id: RequestId, method: &'static str, params: Value) -> Self {
        Self::Request {
            id,
            method: method.to_owned(),
            params,
        }
    }

    /// A notification with the given method and params.
    #[must_use]
    pub fn notification(method: &'static str, params: Value) -> Self {
        Self::Notification {
            method: method.to_owned(),
            params,
        }
    }

    /// A successful response.
    #[must_use]
    pub const fn response(id: RequestId, result: Value) -> Self {
        Self::Response { id, result }
    }

    /// A JSON-RPC error response.
    #[must_use]
    pub const fn error_response(id: RequestId, code: i64, message: String) -> Self {
        Self::Error {
            id,
            error: RpcError {
                code,
                message,
                data: None,
            },
        }
    }

    /// The method name, when this frame is a request or notification.
    #[must_use]
    pub fn method(&self) -> Option<&str> {
        match self {
            Self::Request { method, .. } | Self::Notification { method, .. } => Some(method),
            Self::Response { .. } | Self::Error { .. } => None,
        }
    }
}

impl Serialize for Envelope {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Request { id, method, params } => {
                let mut map = serde_json::Map::new();
                map.insert(
                    "id".to_owned(),
                    serde_json::to_value(id).map_err(serde::ser::Error::custom)?,
                );
                map.insert("method".to_owned(), Value::String(method.clone()));
                if !params.is_null() {
                    map.insert("params".to_owned(), params.clone());
                }
                map.serialize(serializer)
            }
            Self::Notification { method, params } => {
                let mut map = serde_json::Map::new();
                map.insert("method".to_owned(), Value::String(method.clone()));
                if !params.is_null() {
                    map.insert("params".to_owned(), params.clone());
                }
                map.serialize(serializer)
            }
            Self::Response { id, result } => WireResponse {
                id: id.clone(),
                result: result.clone(),
            }
            .serialize(serializer),
            Self::Error { id, error } => WireError {
                id: id.clone(),
                error: error.clone(),
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Envelope {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let obj = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("app-server frames are JSON objects"))?;
        let id = obj.get("id").map(|id| serde_json::from_value(id.clone()));
        let method = obj.get("method").and_then(Value::as_str);
        if let Some(method) = method {
            let params = obj.get("params").cloned().unwrap_or(Value::Null);
            return match id {
                Some(Ok(id)) => Ok(Self::Request {
                    id,
                    method: method.to_owned(),
                    params,
                }),
                Some(Err(error)) => Err(serde::de::Error::custom(error)),
                None => Ok(Self::Notification {
                    method: method.to_owned(),
                    params,
                }),
            };
        }
        let id = id
            .ok_or_else(|| serde::de::Error::custom("a response must have an id"))?
            .map_err(serde::de::Error::custom)?;
        if let Some(result) = obj.get("result") {
            return Ok(Self::Response {
                id,
                result: result.clone(),
            });
        }
        if let Some(error) = obj.get("error") {
            let error = serde_json::from_value(error.clone()).map_err(serde::de::Error::custom)?;
            return Ok(Self::Error { id, error });
        }
        Err(serde::de::Error::custom(
            "an app-server frame must be a request, notification, result, or error",
        ))
    }
}

#[derive(Debug, Serialize)]
struct WireResponse {
    id: RequestId,
    result: Value,
}

#[derive(Debug, Serialize)]
struct WireError {
    id: RequestId,
    error: RpcError,
}

/// Method names flycod sends or handles.
pub mod method {
    /// Client → server handshake request.
    pub const INITIALIZE: &str = "initialize";
    /// Client → server handshake notification after [`INITIALIZE`].
    pub const INITIALIZED: &str = "initialized";
    /// Open a new thread.
    pub const THREAD_START: &str = "thread/start";
    /// Resume an existing thread.
    pub const THREAD_RESUME: &str = "thread/resume";
    /// Open a turn with a user message.
    pub const TURN_START: &str = "turn/start";
    /// Interrupt the in-flight turn.
    pub const TURN_INTERRUPT: &str = "turn/interrupt";
    /// Compact a thread's conversation context.
    pub const THREAD_COMPACT_START: &str = "thread/compact/start";
    /// What the app-server actually mounted, and what each server offers.
    pub const MCP_SERVER_STATUS_LIST: &str = "mcpServerStatus/list";
    /// Server → client: a command wants permission.
    pub const COMMAND_APPROVAL: &str = "item/commandExecution/requestApproval";
    /// Server → client: a file change wants permission.
    pub const FILE_CHANGE_APPROVAL: &str = "item/fileChange/requestApproval";
    /// Server → client: a permissions grant wants permission.
    pub const PERMISSIONS_APPROVAL: &str = "item/permissions/requestApproval";
    /// Server → client: `ChatGPT` tokens must be refreshed.
    pub const AUTH_REFRESH: &str = "account/chatgptAuthTokens/refresh";
    /// Notification: a turn began.
    pub const TURN_STARTED: &str = "turn/started";
    /// Notification: a turn ended.
    pub const TURN_COMPLETED: &str = "turn/completed";
    /// Notification: incremental assistant text.
    pub const AGENT_MESSAGE_DELTA: &str = "item/agentMessage/delta";
    /// Notification: an item started.
    pub const ITEM_STARTED: &str = "item/started";
    /// Notification: an item completed.
    pub const ITEM_COMPLETED: &str = "item/completed";
    /// Notification: token usage snapshot.
    pub const TOKEN_USAGE: &str = "thread/tokenUsage/updated";
    /// Notification: a turn-level error, possibly retried internally.
    pub const ERROR: &str = "error";
    /// Request: the models this build of the app-server offers.
    pub const MODEL_LIST: &str = "model/list";
    /// Request: how much of the account's plan is spent.
    pub const RATE_LIMITS_READ: &str = "account/rateLimits/read";
    /// Notification: a *sparse* revision of that answer.
    pub const RATE_LIMITS_UPDATED: &str = "account/rateLimits/updated";
    /// Request: the skills this thread's checkout offers.
    pub const SKILLS_LIST: &str = "skills/list";
    /// Notification: a watched skill file changed, so the list is stale.
    pub const SKILLS_CHANGED: &str = "skills/changed";
}

/// `initialize.clientInfo`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    /// Client name.
    pub name: String,
    /// Client version.
    pub version: String,
}

/// `initialize.capabilities`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    /// Experimental API surface. Flycod stays on the stable methods.
    pub experimental_api: bool,
}

/// Params for `initialize`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// Who is speaking.
    pub client_info: ClientInfo,
    /// What this client supports.
    pub capabilities: ClientCapabilities,
}

/// Params for `thread/start` and `thread/resume`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadParams {
    /// Working directory the agent operates in.
    pub cwd: String,
    /// Approval policy, kebab-case: `untrusted` | `on-request` | `never`.
    pub approval_policy: String,
    /// Sandbox mode, kebab-case: `read-only` | `workspace-write` | `danger-full-access`.
    pub sandbox: String,
    /// Model override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Thread to resume. Only on `thread/resume`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// `config.toml` overrides that apply to this thread and never touch
    /// the disk.
    ///
    /// Flycod sends exactly one thing here, `mcp_servers`, and sends it on
    /// every session: the root-owned `config.toml` already declares the
    /// same set on a provisioned machine, and this is what mounts it on a
    /// machine whose `CODEX_HOME` is a real person's and not flyco's to
    /// rewrite.
    pub config: ThreadConfig,
}

/// The `config.toml` overrides one thread runs under.
#[derive(Debug, Clone, Serialize)]
pub struct ThreadConfig {
    /// `[mcp_servers]`, keyed by the id that is also the server's identity.
    pub mcp_servers: std::collections::BTreeMap<String, crate::mount::CodexMcpServer>,
    /// `model_reasoning_effort`: the effort the thread opens at.
    ///
    /// A `config.toml` key rather than a field of
    /// [`ThreadParams`](super::protocol::ThreadParams), because that is
    /// where the app-server takes it — the model is a parameter and its
    /// effort is configuration. Omitted where the session chose none, so
    /// the app-server's own `defaultReasoningEffort` for the model stands.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_reasoning_effort: Option<String>,
}

/// One rolling window of the account's plan limit.
///
/// `usedPercent` is the only required field: the app-server states a
/// percentage even where it cannot state how long the window is or when it
/// turns over, and both of those are honestly absent rather than zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitWindow {
    /// How much of the window is spent, 0–100.
    pub used_percent: u8,
    /// How long the window is, in minutes.
    #[serde(default)]
    pub window_duration_mins: Option<u32>,
    /// When it turns over, seconds since the Unix epoch.
    #[serde(default)]
    pub resets_at: Option<i64>,
}

/// The account's rate limits, as one `account/rateLimits/read` answers them.
///
/// Only the two windows flyco draws are read. The snapshot also carries
/// credits, plan type and spend-control state, which answer a different
/// question — what the account *is* rather than what is left of it — and
/// which neither the composer nor the settings page asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitSnapshot {
    /// The shorter window, when the account has one.
    #[serde(default)]
    pub primary: Option<RateLimitWindow>,
    /// The longer window, when the account has one.
    #[serde(default)]
    pub secondary: Option<RateLimitWindow>,
    /// Why the account is blocked, when the backend says it is.
    ///
    /// The app-server's own answer to "is this account out of plan right
    /// now", and the signal issue #244 pauses a session on. `None` is an
    /// account that is not blocked — it is `null` on every ordinary read, as
    /// the recorded fixture below shows.
    #[serde(default)]
    pub rate_limit_reached_type: Option<RateLimitReached>,
}

/// Why the app-server says an account is out of plan.
///
/// The five tokens `RateLimitReachedType` declares in the app-server's own
/// JSON Schema (`codex app-server generate-json-schema`, codex-cli 0.153.4).
/// They divide into two kinds and flyco treats them differently:
///
/// * [`RateLimitReached`](Self::RateLimitReached) is a rolling window being
///   spent, which turns over at a stated instant. That is a wait flyco can
///   schedule around, and it is what a usage-limit pause is for.
/// * The four workspace variants are a workspace's *credits* being gone,
///   which no window reset fixes — somebody has to buy more. They are read so
///   they can be told apart from the first, and nothing is paused for them.
///
/// `Other` is a token this build has not heard of. Treated as blocked and
/// never as a rolling window, because a limit flyco cannot classify must not
/// become a wait for a reset that may never come.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitReached {
    /// A rolling window of the plan is spent.
    RateLimitReached,
    /// The workspace owner is out of credits.
    WorkspaceOwnerCreditsDepleted,
    /// A workspace member is out of credits.
    WorkspaceMemberCreditsDepleted,
    /// The workspace owner is out of plan.
    WorkspaceOwnerUsageLimitReached,
    /// A workspace member is out of plan.
    WorkspaceMemberUsageLimitReached,
    /// A reason this build does not model.
    #[serde(other)]
    Other,
}

impl RateLimitSnapshot {
    /// This snapshot revised by a sparse `account/rateLimits/updated`.
    ///
    /// The app-server documents that notification as sparse — "merge
    /// available values into the most recent read" — so an absent window
    /// means *unchanged*, not *gone*. Replacing the snapshot with the
    /// notification would blank one window every time the other moved.
    #[must_use]
    pub const fn merged(self, update: Self) -> Self {
        Self {
            primary: match update.primary {
                Some(window) => Some(window),
                None => self.primary,
            },
            secondary: match update.secondary {
                Some(window) => Some(window),
                None => self.secondary,
            },
            // The app-server documents the *nullable account metadata* as
            // possibly unavailable in a rolling update rather than cleared,
            // so an absent reason means unchanged here too. A limit that has
            // ended is reported by the windows dropping below full, which is
            // what `flyco_core::blocking_window` reads.
            rate_limit_reached_type: match update.rate_limit_reached_type {
                Some(reached) => Some(reached),
                None => self.rate_limit_reached_type,
            },
        }
    }
}

/// The `account/rateLimits/read` result and the
/// `account/rateLimits/updated` params, which carry the same field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitsBody {
    /// The snapshot.
    pub rate_limits: RateLimitSnapshot,
}

/// Params for `mcpServerStatus/list`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStatusParams {
    /// Report the servers as this thread's runtime sees them.
    pub thread_id: String,
    /// `toolsAndAuthOnly`: the tools are what flycod checks the mount
    /// against, and a server's resource inventory is bytes nobody reads.
    pub detail: &'static str,
    /// Continue a previous page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// One row of the `mcpServerStatus/list` result.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStatus {
    /// The id the server was configured under, which is its identity.
    pub name: String,
    /// `notStarted` | `starting` | `connected` | `authenticationRequired` |
    /// `failed` | `cancelled` | `disabled`, or absent when the app-server
    /// has no thread-runtime state for it.
    #[serde(default)]
    pub runtime_status: Option<String>,
    /// The tools it advertises, keyed by name.
    #[serde(default)]
    pub tools: std::collections::BTreeMap<String, Value>,
}

/// The `mcpServerStatus/list` result.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStatusPage {
    /// This page's servers.
    pub data: Vec<McpServerStatus>,
    /// Present while more pages remain.
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// One item of a `turn/start` input.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UserInput {
    /// Plain text.
    Text {
        /// The message.
        text: String,
        /// Structured elements; flycod sends none.
        text_elements: [(); 0],
    },
    /// A skill the user chose from the composer's `/` palette.
    ///
    /// Codex invokes a skill by naming it as an input item rather than by
    /// any spelling inside the prose, so a `/name` the user picked has to
    /// become this on the way in — a turn that carried the slash as text
    /// would ask the model to read a command instead of running it.
    Skill {
        /// The skill's name, as `skills/list` reported it.
        name: String,
        /// Where its `SKILL.md` lives, as `skills/list` reported it.
        path: String,
    },
}

/// The sandbox a `turn/start` override names.
///
/// Tagged the way the app-server's `SandboxPolicy` union is tagged — the
/// `type` discriminant in `camelCase` — rather than the kebab-case string
/// `thread/start` takes for the same fact. Only the three shapes a flyco
/// permission mode maps to exist here: a workspace the agent writes, a
/// workspace it only reads, and no sandbox at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SandboxPolicy {
    /// The agent reads but cannot write — `plan`, `default`, `dontAsk`.
    ReadOnly,
    /// Writes inside the workspace run without asking — `acceptEdits`,
    /// `auto`.
    WorkspaceWrite,
    /// Nothing is sandboxed — `bypassPermissions`.
    DangerFullAccess,
}

impl SandboxPolicy {
    /// The policy a `thread/start` kebab-case sandbox token names.
    ///
    /// The one translation point between the two spellings the protocol
    /// uses for the same fact: `thread/start` configures by string,
    /// `turn/start` overrides by tagged object.
    ///
    /// # Panics
    ///
    /// Panics on a token the app-server does not have — which is the
    /// caller's bug, not a runtime condition: the tokens come from
    /// `PermissionMode::codex_sandbox`, a closed set.
    #[must_use]
    pub fn from_token(token: &str) -> Self {
        match token {
            "read-only" => Self::ReadOnly,
            "workspace-write" => Self::WorkspaceWrite,
            "danger-full-access" => Self::DangerFullAccess,
            other => panic!("no Codex sandbox named {other:?}"),
        }
    }
}

/// Params for `turn/start`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams {
    /// Thread this turn belongs to.
    pub thread_id: String,
    /// User input items.
    pub input: Vec<UserInput>,
    /// Model override for this turn and every turn after it, as the
    /// app-server documents the field.
    ///
    /// Sent on every turn rather than only on the one that changes it: the
    /// driver holds what the session runs on, and restating it is what
    /// makes a mid-conversation model change survive whatever the
    /// app-server thought the thread was on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Reasoning effort, on the same terms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Approval policy for this turn and every turn after it, on the same
    /// terms as `model`.
    ///
    /// The permission mode the session runs under, as the app-server
    /// spells the half of it that decides when the agent may ask.
    pub approval_policy: &'static str,
    /// Sandbox for this turn and every turn after it, on the same terms.
    ///
    /// The other half of the mode: what runs without asking at all.
    pub sandbox_policy: SandboxPolicy,
}

/// Params for `model/list`.
///
/// The app-server takes an empty object rather than no params at all, so
/// this is a unit struct that serializes as `{}`.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ModelListParams {}

/// One effort level a model accepts, with the app-server's own gloss.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningEffortOption {
    /// The level, as `turn/start`'s `effort` takes it.
    pub reasoning_effort: String,
    /// What the app-server says the level is for. Read for completeness
    /// rather than shown: flyco's picker labels a level by its own name.
    #[serde(default)]
    pub description: Option<String>,
}

/// One row of the `model/list` result.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexModel {
    /// The identifier `thread/start` and `turn/start` take.
    pub id: String,
    /// What a picker shows.
    pub display_name: String,
    /// One line under the name.
    #[serde(default)]
    pub description: String,
    /// Whether the app-server runs this one when nothing is chosen.
    #[serde(default)]
    pub is_default: bool,
    /// Whether the app-server keeps this row out of pickers.
    ///
    /// Read so it can be dropped: a hidden model is one `model/list`
    /// mentions and no user is meant to choose.
    #[serde(default)]
    pub hidden: bool,
    /// The effort the app-server uses when none is named.
    #[serde(default)]
    pub default_reasoning_effort: Option<String>,
    /// Every effort level this model accepts, in the app-server's order.
    #[serde(default)]
    pub supported_reasoning_efforts: Vec<ReasoningEffortOption>,
}

impl From<CodexModel> for flyco_core::ModelOption {
    fn from(model: CodexModel) -> Self {
        Self {
            id: model.id,
            label: model.display_name,
            description: model.description,
            is_default: model.is_default,
            efforts: model
                .supported_reasoning_efforts
                .into_iter()
                .map(|effort| effort.reasoning_effort)
                .collect(),
            default_effort: model.default_reasoning_effort,
        }
    }
}

/// The `model/list` result.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelListResponse {
    /// This page's models. Flycod reads one page: the list is three rows
    /// and the cursor exists for a catalogue that is not this one.
    pub data: Vec<CodexModel>,
}

/// Params for `skills/list`.
///
/// `cwds` is left off, which the app-server documents as "the current
/// session working directory" — the only checkout a flyco session has.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillsListParams {
    /// Re-scan from disk instead of answering from the cache.
    ///
    /// False during the handshake, where the cache is as fresh as the
    /// process; true after a [`method::SKILLS_CHANGED`] notification, whose
    /// whole content is that what is on disk is no longer what was cached.
    pub force_reload: bool,
}

/// One skill of a `skills/list` result.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexSkill {
    /// How the skill is named and invoked.
    pub name: String,
    /// The full description from its `SKILL.md` front matter, which runs to
    /// a paragraph.
    pub description: String,
    /// Absolute path of its `SKILL.md`, which `turn/start` needs to invoke
    /// it.
    pub path: String,
    /// Whether the thread will actually run it.
    pub enabled: bool,
    /// The legacy one-line description from `SKILL.md`, when it has one.
    #[serde(default)]
    pub short_description: Option<String>,
    /// The presentation block from `SKILL.json`, when it has one.
    #[serde(default)]
    pub interface: Option<SkillInterface>,
}

impl CodexSkill {
    /// The one line a palette shows under the name.
    ///
    /// Shortest first, because the palette gives a command one clipped row:
    /// `SKILL.json`'s `shortDescription` is what the skill's author wrote
    /// for exactly this place, `SKILL.md`'s legacy field is the same idea a
    /// generation earlier, and the full description — a paragraph aimed at
    /// the model, not at a reader — is what is left when neither exists.
    #[must_use]
    pub fn summary(self) -> String {
        self.interface
            .and_then(|interface| interface.short_description)
            .or(self.short_description)
            .unwrap_or(self.description)
    }
}

/// The `SKILL.json` presentation block, of which flyco reads one field.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillInterface {
    /// One line describing the skill, written for a picker.
    #[serde(default)]
    pub short_description: Option<String>,
}

/// One checkout's worth of a `skills/list` result.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillsListEntry {
    /// The skills found under it.
    pub skills: Vec<CodexSkill>,
}

/// The `skills/list` result.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillsListResponse {
    /// One entry per working directory asked about.
    pub data: Vec<SkillsListEntry>,
}

/// Params for `turn/interrupt`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnInterruptParams {
    /// Thread whose in-flight turn is interrupted.
    pub thread_id: String,
}

/// Params for `thread/compact/start`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCompactStartParams {
    /// Thread whose context should be compacted.
    pub thread_id: String,
}

/// An approval decision flycod returns to the app-server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalDecision {
    /// Allow this invocation.
    Accept,
    /// Refuse this invocation; the turn continues.
    Decline,
    /// Abort the turn.
    Cancel,
}

/// Response body for command/file-change approvals.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ApprovalDecisionBody {
    /// The decision.
    pub decision: ApprovalDecision,
}

#[cfg(test)]
mod tests {
    use super::{Envelope, RequestId, method};
    use serde_json::json;

    #[test]
    fn a_request_omits_the_jsonrpc_field() {
        let frame = Envelope::request(RequestId::number(1), method::INITIALIZE, json!({}));
        let value = serde_json::to_value(&frame).expect("serialize");
        assert!(value.get("jsonrpc").is_none());
        assert_eq!(value["id"], 1);
        assert_eq!(value["method"], method::INITIALIZE);
    }

    #[test]
    fn a_notification_has_no_id() {
        let frame = Envelope::notification(method::INITIALIZED, serde_json::Value::Null);
        let encoded = serde_json::to_string(&frame).expect("serialize");
        assert_eq!(encoded, r#"{"method":"initialized"}"#);
    }

    #[test]
    fn a_server_request_round_trips_a_string_id() {
        let raw = json!({"id":"approval-1","method":"item/commandExecution/requestApproval","params":{"command":"ls"}});
        let frame: Envelope = serde_json::from_value(raw.clone()).expect("deserialize");
        match &frame {
            Envelope::Request { id, method, .. } => {
                assert_eq!(id, &RequestId::Text("approval-1".to_owned()));
                assert_eq!(method, super::method::COMMAND_APPROVAL);
            }
            other => panic!("expected a request, got {other:?}"),
        }
        assert_eq!(serde_json::to_value(&frame).expect("serialize"), raw);
    }
}
