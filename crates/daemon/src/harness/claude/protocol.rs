//! The flycod⇄sidecar protocol.
//!
//! One JSON object per line in each direction over the sidecar's stdin and
//! stdout. Every message is internally tagged on `type` and uses
//! `snake_case` field names; [`sidecar/protocol.ts`] mirrors these
//! declarations as zod schemas and the fixtures under `fixtures/protocol/`
//! pin both sides to the same bytes.
//!
//! The protocol deliberately keeps the SDK's own messages opaque
//! ([`SidecarEvent::SdkMessage`] carries them verbatim): the sidecar's job
//! is transport and callback plumbing, and every interpretation of an SDK
//! message happens in Rust, in [`super::normalize`].
//!
//! [`sidecar/protocol.ts`]: https://github.com/lexoliu/flyco/blob/main/crates/daemon/sidecar/protocol.ts

use std::collections::BTreeMap;
use std::path::PathBuf;

use flyco_core::ApprovalId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::mount::{ClaudeMcpServer, MountedServer};

/// Identifies one outstanding [`StoreOp`] round trip.
///
/// Minted by the sidecar as a monotonic counter — unlike an
/// [`ApprovalId`], a store request never leaves the daemon, so it needs no
/// globally unique form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StoreRequestId(u64);

impl StoreRequestId {
    /// Wraps a raw counter value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
}

/// The permission mode the Claude Agent SDK runs the session under.
///
/// Defined in [`flyco_core`] because the control plane writes it into the
/// `flycod` configuration it provisions, and re-exported here so the
/// sidecar protocol reads as one module.
pub use flyco_core::PermissionMode;

/// How the sidecar authenticates the `claude` CLI it supervises.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SidecarAuth {
    /// Use whatever login the host user already has.
    ///
    /// No `CLAUDE_CONFIG_DIR` override and no credential injection: the CLI
    /// reads the default `~/.claude` tree. This is the developer-machine
    /// mode; a session VM never runs it.
    Inherit,
    /// Inject a Claude subscription OAuth token.
    OauthToken {
        /// Value for `CLAUDE_CODE_OAUTH_TOKEN`.
        token: String,
    },
    /// Inject an Anthropic API key.
    ApiKey {
        /// Value for `ANTHROPIC_API_KEY`.
        key: String,
    },
}

/// A command flycod writes to the sidecar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SidecarCommand {
    /// Open the session. Sent exactly once, after [`SidecarEvent::Ready`].
    Start {
        /// Working directory the harness operates in.
        cwd: PathBuf,
        /// Credentials for the supervised CLI.
        auth: SidecarAuth,
        /// `CLAUDE_CONFIG_DIR` — an isolated config tree. `None` leaves the
        /// variable unset, which is only correct under
        /// [`SidecarAuth::Inherit`].
        config_dir: Option<PathBuf>,
        /// `CLAUDE_CODE_PROJECT_DIR_NAME` — the stable project key the SDK
        /// files sessions under.
        project_dir_name: Option<String>,
        /// Model override; `None` leaves the CLI's own default in place.
        model: Option<String>,
        /// Reasoning effort for that model, as the SDK's `EffortLevel`
        /// spells it. `None` leaves the CLI's own default for the model in
        /// place, which is the only correct value for a model that accepts
        /// no effort levels at all.
        effort: Option<String>,
        /// Permission mode for the session.
        permission_mode: PermissionMode,
        /// Harness-native session id to resume, for cross-host History.
        resume_session_id: Option<String>,
        /// Whether the CLI should be told to use only the servers below and
        /// ignore every scope it would otherwise discover.
        ///
        /// False on a provisioned machine, and it has to be: the root-owned
        /// `managed-mcp.json` there is an *enterprise* MCP config, and the
        /// CLI refuses to start at all when one is present and
        /// `--strict-mcp-config` is also asked for (issue #195). The two
        /// are the same guarantee by different means — exclusivity the
        /// agent cannot reach, and exclusivity on the command line — so
        /// exactly one of them is ever in force.
        strict_mcp_config: bool,
        /// Every MCP server this session may reach, keyed by the name the
        /// harness announces it under.
        ///
        /// Passed even on a machine whose `managed-mcp.json` already
        /// declares the same set, and the two jobs are different: the
        /// managed file makes the set *exclusive*, this makes it *present*.
        /// A developer's flycod is not root and writes no managed file, and
        /// its session still gets flyco's tools.
        mcp_servers: BTreeMap<String, ClaudeMcpServer>,
    },
    /// Push one user message into the streaming-input generator.
    UserMessage {
        /// The message text.
        text: String,
    },
    /// End the current turn through the SDK's `interrupt()` (SIGINT
    /// semantics). Never signal the child process directly.
    Interrupt,
    /// Run Claude Code's native `/compact` command.
    Compact,
    /// Put the running query on another model, at another effort.
    ///
    /// Two SDK calls behind one command, because they are one decision:
    /// `setModel` moves the query and `applyFlagSettings` sets the effort
    /// on it, and an effort applied to the model before it would be a level
    /// of something the session is no longer running.
    SetModel {
        /// The identifier the SDK's own model list names, verbatim.
        model: String,
        /// The effort level, or `None` to clear whatever was set and leave
        /// the CLI's default for the new model in place.
        effort: Option<String>,
    },
    /// Resolve a pending [`SidecarEvent::ApprovalRequest`].
    ApprovalDecision {
        /// The approval being decided.
        id: ApprovalId,
        /// Whether the tool call may proceed.
        allow: bool,
        /// Replacement tool input. Only meaningful when `allow` is true;
        /// `None` means the sidecar echoes the original input back to the
        /// SDK, which requires an `updatedInput` on every allow.
        updated_input: Option<Value>,
        /// Reason shown to the model. Required in practice when `allow` is
        /// false — the SDK's deny branch carries a message.
        message: Option<String>,
    },
    /// Resolve a pending [`SidecarEvent::StoreRequest`].
    StoreResponse {
        /// The request being answered.
        id: StoreRequestId,
        /// `null` for an append, and for a load of a stream that was never
        /// written — which is the SDK's own "no such session" signal, not
        /// an empty array. Otherwise the array of entries.
        result: Value,
    },
    /// Close the streaming-input generator and exit cleanly.
    Shutdown,
}

impl SidecarCommand {
    /// The `type` tag this command serializes under.
    #[must_use]
    pub const fn tag(&self) -> &'static str {
        match self {
            Self::Start { .. } => "start",
            Self::UserMessage { .. } => "user_message",
            Self::Interrupt => "interrupt",
            Self::Compact => "compact",
            Self::SetModel { .. } => "set_model",
            Self::ApprovalDecision { .. } => "approval_decision",
            Self::StoreResponse { .. } => "store_response",
            Self::Shutdown => "shutdown",
        }
    }
}

/// The `SessionKey` the Agent SDK's `SessionStore` addresses entries by.
///
/// The SDK spells these `projectKey` / `sessionId`; the sidecar translates
/// to this protocol's `snake_case` on the way out and back on the way in,
/// so the flycod protocol has exactly one naming convention.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionKey {
    /// Stable project key, from `CLAUDE_CODE_PROJECT_DIR_NAME`.
    pub project_key: String,
    /// The SDK session this batch belongs to.
    pub session_id: String,
    /// Sub-stream within the session — subagent transcripts arrive as
    /// `subagents/agent-{id}`. Absent for the main transcript; the SDK
    /// treats an empty string as invalid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
}

/// One `SessionStore` operation the sidecar needs flycod to perform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreOp {
    /// Mirror a batch of transcript entries.
    Append {
        /// Where the entries belong.
        key: SessionKey,
        /// The raw SDK entries. Most carry a `uuid`, which the store uses
        /// as an idempotency key; the SDK documents that some (titles,
        /// tags, mode markers) carry none and are appended without dedup.
        entries: Vec<Value>,
    },
    /// Read a stream back, for cross-host resume.
    Load {
        /// Which stream to read.
        key: SessionKey,
    },
}

/// One rolling plan window as the sidecar read it from the SDK.
///
/// The *reading*, not the finished [`flyco_core::UsageWindow`]: the label
/// is derived in Rust so that a five-hour window is called the same thing
/// whichever harness reported it, and a rule that lived in TypeScript would
/// have to be written a second time for Codex. So the sidecar translates
/// only what it alone knows — that the SDK's `five_hour` key means three
/// hundred minutes, and that a `model_scoped` row is a weekly window scoped
/// to one model — and hands the numbers on.
///
/// `used_percent` is an integer because that is what the protocol pins: the
/// SDK's `utilization` is an unbounded 0–100 number, and rounding it at the
/// boundary keeps one spelling of a percentage on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarUsageWindow {
    /// How long the window is, in minutes.
    pub window_minutes: Option<u32>,
    /// The part of the plan this window covers, when it covers only part of
    /// one — the display name of a per-model weekly bucket.
    pub scope: Option<String>,
    /// How much of the window is spent, 0–100.
    pub used_percent: u8,
    /// When the window turns over, seconds since the Unix epoch.
    pub resets_at_unix: Option<i64>,
}

impl From<SidecarUsageWindow> for flyco_core::UsageWindow {
    fn from(window: SidecarUsageWindow) -> Self {
        Self::new(
            window.window_minutes,
            window.scope.as_deref(),
            window.used_percent,
            window.resets_at_unix,
        )
    }
}

/// An event the sidecar writes to flycod.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SidecarEvent {
    /// The Agent SDK imported successfully and the sidecar is listening.
    /// Always the first line, exactly once.
    Ready {
        /// Version of the installed `@anthropic-ai/claude-agent-sdk`.
        sdk_version: String,
    },
    /// The session is identified and its CLI is warming.
    ///
    /// Emitted as soon as the SDK query is constructed — before any user
    /// message — so the control plane can record the session and the UI can
    /// go live without waiting for a turn. The id is the sidecar's own
    /// (`Options.sessionId`) on a fresh session, and the resumed id
    /// otherwise.
    Started {
        /// Harness-native session id, used to resume this session later.
        session_id: String,
    },
    /// Capability tokens advertised by this CLI build.
    ///
    /// Feature detection reads this list and never a version string. It is
    /// its own event because the Agent SDK only reports it on the
    /// `system/init` stream frame, which the CLI emits at the start of a
    /// turn — so it necessarily arrives after
    /// [`SidecarEvent::Started`]. Emitted on every init frame; the newest
    /// set wins.
    Capabilities {
        /// The capability tokens, as the CLI names them.
        capabilities: Vec<String>,
    },
    /// Every model this CLI build offers, from the SDK's
    /// `supportedModels()`.
    ///
    /// Emitted once, as soon as the CLI has answered its `initialize`
    /// handshake and before any turn — the earliest moment the answer
    /// exists, and early enough that the composer's picker is right for the
    /// first message. Translated into flyco's own vocabulary in the
    /// sidecar rather than passed through raw, because the same shape has
    /// to come back from Codex.
    Models {
        /// The models, in the order the SDK listed them.
        models: Vec<flyco_core::ModelOption>,
    },
    /// How much of the account's plan is spent, from the SDK's
    /// `usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET()`.
    ///
    /// Emitted once the CLI has answered its handshake, and again after
    /// every `result` message — the two moments the number can have moved.
    /// The list is empty for a session with no plan behind it at all (an
    /// API key, Bedrock, Vertex), which the SDK states outright with
    /// `rate_limits_available: false`.
    PlanUsage {
        /// Every window the SDK reported, in no particular order.
        windows: Vec<SidecarUsageWindow>,
    },
    /// Every slash command this session offers, from the SDK's
    /// `supportedCommands()`.
    ///
    /// Emitted once the CLI has answered its `initialize` handshake, and
    /// again on every `system/commands_changed` frame — the CLI discovers
    /// skills as the agent walks into subdirectories, so the set a session
    /// opens with is not the set it ends with. The newest list replaces the
    /// last one entirely, which is what the SDK documents that frame to
    /// mean.
    Commands {
        /// The commands, in the order the SDK listed them.
        commands: Vec<flyco_core::HarnessCommand>,
    },
    /// What the CLI actually mounted, from the SDK's `mcpServerStatus()`.
    ///
    /// Emitted once, as soon as the CLI has finished its `initialize`
    /// handshake and before any turn — the earliest moment the answer
    /// exists. flycod checks it against [`crate::mount::verify`] and fails
    /// the session if flyco's own server is not there, because an agent
    /// that cannot read its budget will spend past it.
    McpServers {
        /// One entry per server the CLI was told to mount.
        servers: Vec<MountedServer>,
    },
    /// One SDK stream message, verbatim. Interpreted in
    /// [`super::normalize`].
    SdkMessage {
        /// The raw SDK message.
        message: Value,
    },
    /// A `canUseTool` callback is blocked waiting for a decision.
    ApprovalRequest {
        /// Echo this in [`SidecarCommand::ApprovalDecision`].
        id: ApprovalId,
        /// Tool name the model asked to run.
        tool: String,
        /// Tool input as the model produced it.
        input: Value,
        /// The SDK's own suggested permission updates, when it offers any.
        suggestions: Option<Value>,
    },
    /// A `SessionStore` adapter method is blocked waiting for a result.
    StoreRequest {
        /// Echo this in [`SidecarCommand::StoreResponse`].
        id: StoreRequestId,
        /// What the store must do.
        op: StoreOp,
    },
    /// The sidecar cannot continue. Terminal: it exits after this line.
    Fatal {
        /// Human-readable cause.
        error: String,
    },
}

impl SidecarEvent {
    /// The `type` tag this event serializes under.
    #[must_use]
    pub const fn tag(&self) -> &'static str {
        match self {
            Self::Ready { .. } => "ready",
            Self::Started { .. } => "started",
            Self::Capabilities { .. } => "capabilities",
            Self::Models { .. } => "models",
            Self::PlanUsage { .. } => "plan_usage",
            Self::Commands { .. } => "commands",
            Self::McpServers { .. } => "mcp_servers",
            Self::SdkMessage { .. } => "sdk_message",
            Self::ApprovalRequest { .. } => "approval_request",
            Self::StoreRequest { .. } => "store_request",
            Self::Fatal { .. } => "fatal",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PermissionMode, SessionKey, SidecarAuth, SidecarCommand, SidecarEvent};

    #[test]
    fn unit_commands_are_bare_tags() {
        assert_eq!(
            serde_json::to_string(&SidecarCommand::Interrupt).expect("serialize"),
            r#"{"type":"interrupt"}"#
        );
        assert_eq!(
            serde_json::to_string(&SidecarCommand::Shutdown).expect("serialize"),
            r#"{"type":"shutdown"}"#
        );
        assert_eq!(
            serde_json::to_string(&SidecarCommand::Compact).expect("serialize"),
            r#"{"type":"compact"}"#
        );
    }

    #[test]
    fn permission_modes_use_the_sdks_camel_case() {
        for (mode, token) in [
            (PermissionMode::Default, "\"default\""),
            (PermissionMode::AcceptEdits, "\"acceptEdits\""),
            (PermissionMode::BypassPermissions, "\"bypassPermissions\""),
            (PermissionMode::Plan, "\"plan\""),
            (PermissionMode::DontAsk, "\"dontAsk\""),
            (PermissionMode::Auto, "\"auto\""),
        ] {
            assert_eq!(serde_json::to_string(&mode).expect("serialize"), token);
        }
    }

    #[test]
    fn an_absent_subpath_is_omitted_rather_than_null() {
        let key = SessionKey {
            project_key: "flyco".to_owned(),
            session_id: "s-1".to_owned(),
            subpath: None,
        };
        let json = serde_json::to_string(&key).expect("serialize");
        assert_eq!(json, r#"{"project_key":"flyco","session_id":"s-1"}"#);
    }

    #[test]
    fn auth_modes_are_tagged_on_mode() {
        let json = serde_json::to_value(SidecarAuth::ApiKey {
            key: "sk-test".to_owned(),
        })
        .expect("serialize");
        assert_eq!(json["mode"], "api_key");
        assert_eq!(json["key"], "sk-test");
    }

    #[test]
    fn capabilities_travel_separately_from_session_identity() {
        // The two cannot share an event: the SDK reports a session's
        // identity at construction and its capabilities only on the first
        // turn's `system/init`.
        let started = SidecarEvent::Started {
            session_id: "9d0f4b1a".to_owned(),
        };
        let json = serde_json::to_value(&started).expect("serialize");
        assert!(json.get("capabilities").is_none());

        let capabilities = SidecarEvent::Capabilities {
            capabilities: vec!["interrupt_receipt_v1".to_owned()],
        };
        let json = serde_json::to_value(&capabilities).expect("serialize");
        assert_eq!(json["type"], "capabilities");
    }

    #[test]
    fn tags_match_the_serialized_type_field() {
        let event = SidecarEvent::Ready {
            sdk_version: "0.3.250".to_owned(),
        };
        let json = serde_json::to_value(&event).expect("serialize");
        assert_eq!(json["type"], event.tag());
    }
}
