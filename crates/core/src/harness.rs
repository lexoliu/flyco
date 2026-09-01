//! Harness abstraction: the two supported coding agents and the
//! normalized event stream flyco extracts from them.

use serde::{Deserialize, Serialize};

use crate::id::HarnessAccountId;
use crate::money::Usd;

/// The coding harness driving a session. Flyco supports exactly these two
/// and never builds its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "sql", derive(skyzen::Column))]
pub enum HarnessKind {
    /// Anthropic's Claude Code, driven through the Agent SDK sidecar.
    ClaudeCode,
    /// `OpenAI`'s Codex, driven through `codex app-server` JSON-RPC.
    Codex,
}

/// The permission mode the Claude Agent SDK runs a session under.
///
/// Mirrors the SDK's own `PermissionMode` union, spelled in its `camelCase`
/// so the sidecar passes the value straight into `query`'s `permissionMode`
/// option. Every mode other than [`Self::Default`] narrows what reaches
/// flyco's approval UI, because an auto-approved tool never calls back.
///
/// It lives in the domain model rather than in the daemon because it
/// crosses a boundary in both directions: the control plane writes it into
/// the `flycod` configuration it provisions onto a machine, and the daemon
/// reads that configuration back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Every non-auto-approved tool call reaches `canUseTool`.
    Default,
    /// File edits are auto-approved; everything else still asks.
    AcceptEdits,
    /// Nothing asks. Only ever safe behind flyco's managed-settings deny
    /// rules, which bind even in this mode.
    BypassPermissions,
    /// Planning only: the model may not mutate anything.
    Plan,
    /// Never prompt; deny anything not pre-approved.
    DontAsk,
    /// A model classifier decides prompts — the proposal's "Auto mode".
    Auto,
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
    /// Remote control of a local official-app session.
    RemoteControl,
    /// Harness-native resume (flyco owns History instead).
    Resume,
    /// Skills.
    Skills,
    /// MCP servers.
    Mcp,
    /// Memory.
    Memory,
    /// Browser control.
    BrowserControl,
    /// Computer control.
    ComputerControl,
}

impl Feature {
    /// Every feature the matrix tracks, in display order.
    pub const ALL: &'static [Self] = &[
        Self::UsageDisplay,
        Self::ContextWindowDisplay,
        Self::GoalMode,
        Self::AutoMode,
        Self::SideChat,
        Self::DynamicWorkflows,
        Self::Settings,
        Self::Compact,
        Self::Advisor,
        Self::Monitor,
        Self::BackgroundTasks,
        Self::AutoContinueAtUsageLimit,
        Self::RemoteControl,
        Self::Resume,
        Self::Skills,
        Self::Mcp,
        Self::Memory,
        Self::BrowserControl,
        Self::ComputerControl,
    ];
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
    /// Flyco deliberately does not offer this; it owns the equivalent.
    Disabled,
    /// Flyco took the feature over from the harness (central registry, MCP).
    Takeover,
    /// Scheduled for a later product phase.
    Phase2,
    /// This harness does not have the feature.
    NotApplicable,
}

/// One row of the per-harness feature matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HarnessFeature {
    /// The capability.
    pub feature: Feature,
    /// Status on Claude Code.
    pub claude_code: Availability,
    /// Status on Codex.
    pub codex: Availability,
}

/// The verified availability of `feature` on `harness`.
#[must_use]
pub const fn availability(harness: HarnessKind, feature: Feature) -> Availability {
    match (harness, feature) {
        (
            _,
            Feature::UsageDisplay
            | Feature::ContextWindowDisplay
            | Feature::GoalMode
            | Feature::AutoMode
            | Feature::Compact
            | Feature::BackgroundTasks
            | Feature::AutoContinueAtUsageLimit,
        ) => Availability::Supported,
        (_, Feature::SideChat) => Availability::HarnessLimitation,
        (_, Feature::DynamicWorkflows | Feature::Settings)
        | (HarnessKind::ClaudeCode, Feature::Advisor | Feature::Monitor) => Availability::Planned,
        (HarnessKind::Codex, Feature::Advisor | Feature::Monitor) => Availability::NotApplicable,
        (_, Feature::RemoteControl | Feature::Resume) => Availability::Disabled,
        (_, Feature::Skills | Feature::Mcp | Feature::Memory) => Availability::Takeover,
        (_, Feature::BrowserControl | Feature::ComputerControl) => Availability::Phase2,
    }
}

/// The full matrix, one row per [`Feature::ALL`] entry.
#[must_use]
pub fn matrix() -> Vec<HarnessFeature> {
    Feature::ALL
        .iter()
        .map(|&feature| HarnessFeature {
            feature,
            claude_code: availability(HarnessKind::ClaudeCode, feature),
            codex: availability(HarnessKind::Codex, feature),
        })
        .collect()
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

/// A Claude or Codex account the user has linked, as `GET
/// /v1/harness-accounts` lists it.
///
/// Linking runs against the *vendor's own* authorization page — the flow the
/// official CLIs wrap — so flyco never sees a password, and what it stores
/// is the resulting token, sealed. No representation of an account carries
/// that token; it leaves the control plane only when it is provisioned onto
/// a session machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HarnessAccountView {
    /// Identifier.
    pub id: HarnessAccountId,
    /// Which harness this account drives.
    pub harness: HarnessKind,
    /// Account name as the vendor reports it, so the user can tell two
    /// linked accounts apart.
    pub label: String,
    /// When it was linked, seconds since the Unix epoch.
    pub linked_at_unix: u64,
    /// When the stored credential expires, when the vendor states a
    /// lifetime.
    pub expires_at_unix: Option<u64>,
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
    /// The harness compacted the conversation context successfully.
    ContextCompacted,
    /// The harness could not compact the conversation context.
    ContextCompactionFailed {
        /// Harness-reported reason.
        error: String,
    },
}

#[cfg(test)]
mod tests {
    use super::{
        Availability, ContextWindow, Feature, HarnessEvent, HarnessKind, UsageReport, availability,
        matrix,
    };
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

    #[test]
    fn the_matrix_covers_every_feature_once() {
        let rows = matrix();
        assert_eq!(rows.len(), Feature::ALL.len());
        for (row, &feature) in rows.iter().zip(Feature::ALL) {
            assert_eq!(row.feature, feature);
            assert_eq!(
                row.claude_code,
                availability(HarnessKind::ClaudeCode, feature)
            );
            assert_eq!(row.codex, availability(HarnessKind::Codex, feature));
        }
    }

    #[test]
    fn verified_capabilities_are_supported() {
        for feature in [
            Feature::UsageDisplay,
            Feature::ContextWindowDisplay,
            Feature::GoalMode,
            Feature::AutoMode,
            Feature::Compact,
            Feature::BackgroundTasks,
            Feature::AutoContinueAtUsageLimit,
        ] {
            assert_eq!(
                availability(HarnessKind::ClaudeCode, feature),
                Availability::Supported
            );
            assert_eq!(
                availability(HarnessKind::Codex, feature),
                Availability::Supported
            );
        }
        assert_eq!(
            availability(HarnessKind::Codex, Feature::Advisor),
            Availability::NotApplicable
        );
        assert_eq!(
            availability(HarnessKind::ClaudeCode, Feature::Skills),
            Availability::Takeover
        );
        assert_eq!(
            availability(HarnessKind::ClaudeCode, Feature::Resume),
            Availability::Disabled
        );
        assert_eq!(
            availability(HarnessKind::ClaudeCode, Feature::SideChat),
            Availability::HarnessLimitation
        );
    }
}
