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

/// How full the model's context window is.
///
/// Reported as a pair or not at all: a fill gauge needs both numbers, and a
/// harness that names only one of them tells us nothing displayable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ContextWindow {
    /// Tokens currently occupying the context window.
    pub used_tokens: u64,
    /// Size of the context window in tokens.
    pub size_tokens: u64,
}

impl ContextWindow {
    /// The fill fraction in basis points (1/100 of a percent), rounded down.
    ///
    /// # Panics
    ///
    /// Panics if [`Self::size_tokens`] is zero — a zero-width context window
    /// is a harness bug, not a state the UI should render.
    #[must_use]
    pub const fn fill_basis_points(self) -> u64 {
        assert!(self.size_tokens > 0, "context window size must be positive");
        self.used_tokens * 10_000 / self.size_tokens
    }
}

/// Token and context-window accounting reported by the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct UsageReport {
    /// Input tokens consumed so far in this session.
    pub input_tokens: u64,
    /// Output tokens produced so far in this session.
    pub output_tokens: u64,
    /// Context-window fill, when the harness reports both halves of it.
    ///
    /// Codex reports `modelContextWindow` on every token-usage
    /// notification; the Claude Agent SDK only names a window size when its
    /// `result` message carries per-model usage, so this is `None` for
    /// sessions where it does not.
    pub context: Option<ContextWindow>,
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

#[cfg(test)]
mod tests {
    use super::{ContextWindow, HarnessEvent, UsageReport};
    use crate::money::Usd;

    #[test]
    fn context_fill_is_exact() {
        let window = ContextWindow {
            used_tokens: 50_000,
            size_tokens: 200_000,
        };
        assert_eq!(window.fill_basis_points(), 2_500);
    }

    #[test]
    #[should_panic(expected = "context window size must be positive")]
    fn a_zero_width_context_window_is_a_bug() {
        let _ = ContextWindow {
            used_tokens: 1,
            size_tokens: 0,
        }
        .fill_basis_points();
    }

    #[test]
    fn a_harness_without_a_context_gauge_reports_none() {
        let event = HarnessEvent::TurnCompleted {
            turn_id: "t-1".to_owned(),
            usage: UsageReport {
                input_tokens: 12,
                output_tokens: 34,
                context: None,
                estimated_cost: Some(Usd::from_cents(7)),
            },
        };
        let json = serde_json::to_value(&event).expect("serialize");
        assert_eq!(json["type"], "turn_completed");
        assert!(json["usage"]["context"].is_null());
        let back: HarnessEvent = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, event);
    }
}
