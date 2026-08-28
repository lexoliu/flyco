//! Harness abstraction: the two supported coding agents and the
//! normalized event stream flyco extracts from them.

use serde::{Deserialize, Serialize};

use crate::money::Usd;

/// The coding harness driving a session. Flyco supports exactly these two
/// and never builds its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HarnessKind {
    /// Anthropic's Claude Code, driven through the Agent SDK sidecar.
    ClaudeCode,
    /// `OpenAI`'s Codex, driven through `codex app-server` JSON-RPC.
    Codex,
}

/// A harness capability tracked in the per-harness feature matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    /// Account usage display.
    UsageDisplay,
    /// Context-window fill display.
    ContextWindowDisplay,
    /// Goal mode.
    GoalMode,
    /// Automatic permission mode.
    AutoMode,
    /// Side chat ("btw").
    SideChat,
    /// Ultra mode / dynamic workflows.
    DynamicWorkflows,
    /// Arbitrary settings passthrough.
    Settings,
    /// Context compaction.
    Compact,
    /// Advisor model.
    Advisor,
    /// Monitor tool.
    Monitor,
    /// Background tasks.
    BackgroundTasks,
    /// Automatic continue when the usage limit resets.
    AutoContinueAtUsageLimit,
}

/// Whether a [`Feature`] is available for a given harness, mirrored in the
/// UI so vendor releases create tracked gaps instead of broken promises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    /// Fully supported by flyco.
    Supported,
    /// The harness offers no programmatic route (e.g. terminal-only UI).
    HarnessLimitation,
    /// Reachable but not yet implemented in flyco.
    Planned,
}

/// Token and context-window accounting reported by the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct UsageReport {
    /// Input tokens consumed so far in this session.
    pub input_tokens: u64,
    /// Output tokens produced so far in this session.
    pub output_tokens: u64,
    /// Tokens currently occupying the context window.
    pub context_used_tokens: u64,
    /// Size of the context window in tokens.
    pub context_size_tokens: u64,
    /// Cost estimate reported by the harness for this session, if any.
    pub estimated_cost: Option<Usd>,
}

/// A normalized event extracted from either harness's native stream.
///
/// The daemon translates Claude Code stream-json / Codex `item/*`
/// notifications into this shape; everything upstream (relay, transcript
/// store, frontend) consumes only this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessEvent {
    /// A turn began processing a user message.
    TurnStarted {
        /// Harness-native turn identifier.
        turn_id: String,
    },
    /// Incremental assistant text.
    AssistantDelta {
        /// Harness-native turn identifier.
        turn_id: String,
        /// The appended text fragment.
        text: String,
    },
    /// A tool call started.
    ToolStarted {
        /// Harness-native turn identifier.
        turn_id: String,
        /// Harness-native identifier of this tool invocation.
        call_id: String,
        /// Tool name as the harness reports it.
        tool: String,
        /// Tool input as the harness reports it.
        input: serde_json::Value,
    },
    /// A tool call finished.
    ToolCompleted {
        /// Harness-native turn identifier.
        turn_id: String,
        /// Harness-native identifier of this tool invocation.
        call_id: String,
        /// Whether the tool reported success.
        ok: bool,
    },
    /// The turn finished.
    TurnCompleted {
        /// Harness-native turn identifier.
        turn_id: String,
        /// Usage after this turn.
        usage: UsageReport,
    },
    /// The harness reported a fatal error for this turn.
    TurnFailed {
        /// Harness-native turn identifier.
        turn_id: String,
        /// Human-readable error from the harness.
        error: String,
    },
    /// The harness hit its account usage limit and is waiting for reset.
    UsageLimited {
        /// When the limit resets, as a unix timestamp in seconds, when the
        /// harness reports one.
        resets_at_unix: Option<u64>,
    },
}
