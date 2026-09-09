//! Harness abstraction: the two supported coding agents and the
//! normalized event stream flyco extracts from them.

use serde::{Deserialize, Serialize};

use crate::id::{ClaudeOauthAttemptId, CodexOauthAttemptId, HarnessAccountId};
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

/// One model a harness can run a session on, as the harness itself lists it.
///
/// Flyco never curates this: the identifiers, the names and the order are
/// the harness's own, so a model Anthropic or `OpenAI` adds tomorrow reaches
/// the picker the first time a session asks its harness what it offers.
/// [`builtin_models`] is what the picker shows before any session has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ModelOption {
    /// The identifier the harness accepts, verbatim.
    ///
    /// The Claude Agent SDK's `value` and Codex's model `id`. Passed
    /// through untouched — `default` and `claude-fable-5-1[1m]` are both
    /// things the CLI takes — because a normalized spelling would be a
    /// second vocabulary flyco would then have to translate back.
    pub id: String,
    /// What the picker shows, which is the harness's `displayName`.
    pub label: String,
    /// One line under the label, which is the harness's `description`.
    pub description: String,
    /// Whether the harness runs this one when nothing is chosen.
    ///
    /// Claude says so by naming the row `default`; Codex says so with
    /// `isDefault`. Exactly one row of a list carries it, which is what
    /// [`ModelChoice::default_of`] relies on.
    pub is_default: bool,
    /// The effort levels the model accepts, in the harness's own order.
    ///
    /// Empty for a model that accepts none — Claude's Haiku row names no
    /// effort levels at all — and an empty list is the whole answer: a
    /// picker that offered one anyway would be offering a request the
    /// harness refuses.
    pub efforts: Vec<String>,
    /// The effort the harness uses when none is chosen, where it says.
    ///
    /// Codex states one per model (`defaultReasoningEffort`); the Claude
    /// SDK does not, so this is `None` for every Claude row and the CLI's
    /// own default stands.
    pub default_effort: Option<String>,
}

/// What a session runs on: a model and, optionally, an effort.
///
/// One value rather than two fields wherever a session's model is written,
/// read or changed, because the pair is only ever meaningful together: an
/// effort names a level of a *model*, and a request that changed one
/// without the other would be asking for a combination nobody chose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ModelChoice {
    /// The model's [`id`](ModelOption::id).
    pub model: String,
    /// `None` leaves the harness's own default effort in place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// Why a model choice was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ModelChoiceError {
    /// The model is not in the list the harness offers.
    #[error("`{model}` is not a model this agent offers")]
    UnknownModel {
        /// What was asked for.
        model: String,
    },
    /// The model is offered but does not take this effort level.
    #[error("`{effort}` is not an effort level `{model}` accepts")]
    UnknownEffort {
        /// The model the effort was asked of.
        model: String,
        /// What was asked for.
        effort: String,
    },
}

impl ModelChoice {
    /// The choice a session opens with when the caller names none.
    ///
    /// The list's own default, with no effort: a session that did not
    /// choose a model has not chosen an effort either, and naming the
    /// harness's stated default here would freeze today's answer into the
    /// session row.
    ///
    /// # Panics
    ///
    /// Panics if `models` names no default, which is a harness that
    /// answered its own model list without saying which one it runs.
    #[must_use]
    pub fn default_of(models: &[ModelOption]) -> Self {
        let default = models
            .iter()
            .find(|model| model.is_default)
            .expect("a harness model list names exactly one default");
        Self {
            model: default.id.clone(),
            effort: None,
        }
    }

    /// Refuses a choice the list does not offer.
    ///
    /// Checked against the list rather than against a hardcoded set, so a
    /// model the harness dropped stops being accepted the moment a session
    /// reports the new list — and the refusal names the model, because
    /// "invalid model" alone tells a user nothing about which of the two
    /// halves they got wrong.
    ///
    /// # Errors
    ///
    /// Returns [`ModelChoiceError`] if the model is not in `models`, or if
    /// it is and does not accept the named effort.
    pub fn validate(&self, models: &[ModelOption]) -> Result<(), ModelChoiceError> {
        let offered = models
            .iter()
            .find(|model| model.id == self.model)
            .ok_or_else(|| ModelChoiceError::UnknownModel {
                model: self.model.clone(),
            })?;
        let Some(effort) = &self.effort else {
            return Ok(());
        };
        if offered.efforts.iter().any(|level| level == effort) {
            return Ok(());
        }
        Err(ModelChoiceError::UnknownEffort {
            model: self.model.clone(),
            effort: effort.clone(),
        })
    }
}

/// The models a harness offers before any session of the account has
/// reported its own list.
///
/// Two JSON documents rather than two `vec![]` literals, and they are the
/// real lists as the installed harnesses answered them: the picker has to
/// show something the first time an account is linked, and a list written
/// out as Rust would be a place for a stale identifier to hide behind a
/// compiling expression. A session that reports its harness's own list
/// replaces this for that account.
///
/// # Panics
///
/// Panics if either document does not parse, which is this crate's own
/// data being malformed rather than anything a caller did.
#[must_use]
pub fn builtin_models(harness: HarnessKind) -> Vec<ModelOption> {
    let document = match harness {
        HarnessKind::ClaudeCode => include_str!("../models/claude_code.json"),
        HarnessKind::Codex => include_str!("../models/codex.json"),
    };
    serde_json::from_str(document).expect("a built-in model list parses")
}

/// Request body of `PUT /v1/sessions/{id}/models`.
///
/// What a session's daemon reports once its harness has answered what it
/// offers. Recorded against the account rather than the session, because
/// the list is a fact about the harness build the account runs on and the
/// composer needs it before any session of the next one exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ReportModels {
    /// Every model the harness listed, in its own order.
    pub models: Vec<ModelOption>,
}

/// A credential accepted when linking a Claude Code or Codex account.
///
/// Each variant determines its harness, so the wire format cannot pair a
/// Claude credential with Codex or vice versa. Secret fields are deliberately
/// omitted from [`Debug`](core::fmt::Debug).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HarnessCredentialInput {
    /// Long-lived Claude subscription token produced by `claude setup-token`.
    ClaudeSetupToken {
        /// Value printed by `claude setup-token`.
        token: String,
    },
    /// Anthropic API key used by Claude Code.
    ClaudeApiKey {
        /// Value for `ANTHROPIC_API_KEY`.
        key: String,
    },
    /// `OpenAI` API key used by Codex.
    CodexApiKey {
        /// Value for `OPENAI_API_KEY`.
        key: String,
    },
    /// A Claude subscription obtained through the OAuth code flow.
    ///
    /// The pair the flow yields rather than a single token: the access
    /// token is what a session's machine runs the agent under, and the
    /// refresh token is what keeps a linked account working past the
    /// access token's lifetime without the user pasting anything again.
    ClaudeOauth {
        /// Value for `CLAUDE_CODE_OAUTH_TOKEN`, until it expires.
        access_token: String,
        /// Redeemed for a new pair once the access token is near its end.
        refresh_token: String,
        /// When the access token stops working, seconds since the Unix
        /// epoch.
        expires_at_unix: u64,
    },
    /// A `ChatGPT` subscription obtained through the Codex device-code flow.
    ///
    /// Four values rather than one, because Codex's own `auth.json` is four
    /// values: the id token names the account, the access token runs the
    /// agent, the refresh token keeps the link alive, and the account id is
    /// the workspace every request is billed to.
    CodexOauth {
        /// The `ChatGPT` id token, a JWT naming the account.
        id_token: String,
        /// The bearer token Codex runs under, until it expires.
        access_token: String,
        /// Redeemed for a new set once the access token is near its end.
        refresh_token: String,
        /// `chatgpt_account_id`, the workspace the grant belongs to.
        account_id: String,
        /// When the access token stops working, seconds since the Unix
        /// epoch.
        expires_at_unix: u64,
    },
}

impl core::fmt::Debug for HarnessCredentialInput {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let kind = match self {
            Self::ClaudeSetupToken { .. } => "claude_setup_token",
            Self::ClaudeApiKey { .. } => "claude_api_key",
            Self::CodexApiKey { .. } => "codex_api_key",
            Self::ClaudeOauth { .. } => "claude_oauth",
            Self::CodexOauth { .. } => "codex_oauth",
        };
        f.debug_struct("HarnessCredentialInput")
            .field("kind", &kind)
            .finish_non_exhaustive()
    }
}

impl HarnessCredentialInput {
    /// Harness this credential can authenticate.
    #[must_use]
    pub const fn harness(&self) -> HarnessKind {
        match self {
            Self::ClaudeSetupToken { .. }
            | Self::ClaudeApiKey { .. }
            | Self::ClaudeOauth { .. } => HarnessKind::ClaudeCode,
            Self::CodexApiKey { .. } | Self::CodexOauth { .. } => HarnessKind::Codex,
        }
    }

    /// Secret carried by this credential.
    #[must_use]
    pub fn secret(&self) -> &str {
        match self {
            Self::ClaudeSetupToken { token } => token,
            Self::ClaudeApiKey { key } | Self::CodexApiKey { key } => key,
            Self::ClaudeOauth { access_token, .. } | Self::CodexOauth { access_token, .. } => {
                access_token
            }
        }
    }
}

/// Request to link a Claude Code or Codex account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct LinkHarnessAccount {
    /// User-facing name that distinguishes this credential from another.
    pub label: String,
    /// Authentication material, tagged with the mode that consumes it.
    pub credential: HarnessCredentialInput,
}

/// Response of `POST /v1/harness-accounts/claude/oauth/start`.
///
/// The PKCE verifier never appears here: it stays in the control plane's
/// key-value store for the ten minutes the attempt lives, and the browser
/// carries only the opaque attempt id that names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ClaudeOauthStart {
    /// Names the verifier and `state` the completion must be redeemed
    /// against.
    pub attempt_id: ClaudeOauthAttemptId,
    /// Fully-formed `https://claude.ai/oauth/authorize` URL to open.
    pub authorize_url: String,
}

/// Request body of `POST /v1/harness-accounts/claude/oauth/complete`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CompleteClaudeOauth {
    /// The attempt this code belongs to, from [`ClaudeOauthStart`].
    pub attempt_id: ClaudeOauthAttemptId,
    /// What Anthropic showed the user, which is `CODE#STATE` — the bare
    /// code alone is accepted too, because a user who selects only the
    /// first half of it has still supplied everything the exchange needs.
    pub code: String,
}

/// Response of `POST /v1/harness-accounts/codex/oauth/start`.
///
/// The three things `codex login --device-auth` prints, plus the opaque
/// attempt id the browser polls against. `OpenAI`'s `device_auth_id` is
/// deliberately not among them: it is the half that redeems the grant, so
/// it stays in the control plane's key-value store beside the user who
/// started the attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CodexOauthStart {
    /// Names the device authorization this sign-in is polled against.
    pub attempt_id: CodexOauthAttemptId,
    /// The one-time code the user types at
    /// [`verification_url`](Self::verification_url).
    pub user_code: String,
    /// Where the user approves the code, which is
    /// `https://auth.openai.com/codex/device`.
    pub verification_url: String,
    /// How long to wait between polls, as `OpenAI` states it.
    pub interval_seconds: u64,
}

/// Body of a `GET /v1/harness-accounts/codex/oauth/{attempt_id}` that found
/// the sign-in still waiting.
///
/// An approved sign-in answers `201` with the linked
/// [`HarnessAccountView`] instead, so the two outcomes are told apart by
/// the status code and never by a nullable field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CodexOauthPending {
    /// Nobody has approved the code yet. Poll again after
    /// [`CodexOauthStart::interval_seconds`].
    Pending,
}

/// A Claude or Codex account the user has linked, as `GET
/// /v1/harness-accounts` lists it.
///
/// No representation of an account carries its credential; the sealed value
/// leaves the control plane only when it is provisioned onto a session machine.
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
    /// The models a session on this account may run on.
    ///
    /// What the account's last session reported its harness offers, and
    /// [`builtin_models`] until one has. Carried on the account rather than
    /// asked for separately because the composer picks a model *while*
    /// choosing which account to open the session on, and a second request
    /// per account would be a picker that renders after the form it belongs
    /// to.
    pub models: Vec<ModelOption>,
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
        Availability, ContextWindow, Feature, HarnessEvent, HarnessKind, ModelChoice,
        ModelChoiceError, ModelOption, UsageReport, availability, builtin_models, matrix,
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

    #[test]
    fn both_built_in_lists_parse_and_name_one_default() {
        for harness in [HarnessKind::ClaudeCode, HarnessKind::Codex] {
            let models = builtin_models(harness);
            assert!(!models.is_empty(), "{harness:?} lists no models");
            let defaults = models.iter().filter(|model| model.is_default).count();
            assert_eq!(defaults, 1, "{harness:?} must name exactly one default");
            for model in &models {
                assert!(!model.id.is_empty());
                assert!(!model.label.is_empty());
                assert!(!model.description.is_empty());
            }
        }
    }

    #[test]
    fn a_session_that_chose_nothing_opens_on_the_harnesss_default() {
        let claude = ModelChoice::default_of(&builtin_models(HarnessKind::ClaudeCode));
        assert_eq!(claude.model, "default");
        // No effort: not choosing a model is not choosing an effort either,
        // and the harness's own default stands.
        assert_eq!(claude.effort, None);
        let codex = ModelChoice::default_of(&builtin_models(HarnessKind::Codex));
        assert_eq!(codex.model, "gpt-5.6-terra");
    }

    #[test]
    #[should_panic(expected = "a harness model list names exactly one default")]
    fn a_model_list_with_no_default_is_a_harness_bug() {
        let _ = ModelChoice::default_of(&[]);
    }

    #[test]
    fn a_choice_is_refused_against_the_list_that_does_not_offer_it() {
        let models = builtin_models(HarnessKind::ClaudeCode);
        assert_eq!(
            ModelChoice {
                model: "sonnet".to_owned(),
                effort: Some("high".to_owned()),
            }
            .validate(&models),
            Ok(())
        );
        assert_eq!(
            ModelChoice {
                model: "gpt-5.6-terra".to_owned(),
                effort: None,
            }
            .validate(&models),
            Err(ModelChoiceError::UnknownModel {
                model: "gpt-5.6-terra".to_owned(),
            })
        );
        // Haiku names no effort levels, so every effort is one it refuses.
        assert_eq!(
            ModelChoice {
                model: "haiku".to_owned(),
                effort: Some("low".to_owned()),
            }
            .validate(&models),
            Err(ModelChoiceError::UnknownEffort {
                model: "haiku".to_owned(),
                effort: "low".to_owned(),
            })
        );
    }

    #[test]
    fn a_choice_without_an_effort_omits_it_rather_than_sending_null() {
        let json = serde_json::to_string(&ModelChoice {
            model: "opus".to_owned(),
            effort: None,
        })
        .expect("serialize");
        assert_eq!(json, r#"{"model":"opus"}"#);
    }

    #[test]
    fn a_model_option_round_trips() {
        let option = ModelOption {
            id: "gpt-5.5".to_owned(),
            label: "GPT-5.5".to_owned(),
            description: "Proven previous-generation model for coding and general work.".to_owned(),
            is_default: false,
            efforts: vec!["low".to_owned(), "medium".to_owned()],
            default_effort: Some("medium".to_owned()),
        };
        let json = serde_json::to_value(&option).expect("serialize");
        assert_eq!(json["is_default"], false);
        assert_eq!(json["default_effort"], "medium");
        let back: ModelOption = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, option);
    }
}
