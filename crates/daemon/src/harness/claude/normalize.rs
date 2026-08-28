//! Claude Agent SDK stream messages → [`HarnessEvent`].
//!
//! # Normalization policy
//!
//! The SDK's stream is a superset of what flyco models. This module maps
//! the messages flyco has a normalized shape for and **drops the rest,
//! logged at `debug`**. That is a deliberate, documented policy rather than
//! a silent fallback: the SDK adds message types on its own release
//! schedule, and a daemon that failed on the first unrecognized `subtype`
//! would break every time Anthropic shipped a feature. What flyco *does*
//! claim to understand is fully typed below, so a change to one of those
//! shapes surfaces as a `warn` and a missing event, not as a wrong one.
//!
//! Two consequences worth stating explicitly:
//!
//! - **Assistant text comes only from partial-message deltas.** The driver
//!   always sets `includePartialMessages`, so text arrives as
//!   `stream_event` / `content_block_delta` / `text_delta`. The text blocks
//!   of the corresponding complete `assistant` message are dropped, because
//!   emitting both would duplicate every character.
//! - **Turn identity is flyco's, not the SDK's.** The Agent SDK has no turn
//!   id: it has a session, messages, and a terminal `result`. The driver
//!   mints a turn id when it pushes a user message and the normalizer
//!   stamps it onto everything until the `result` arrives. Events that
//!   arrive outside a turn (`system/init`, for instance) are dropped.

use flyco_core::harness::{ContextWindow, HarnessEvent, UsageReport};
use flyco_core::money::Usd;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// One microdollar per millionth of a dollar.
const MICROS_PER_DOLLAR: f64 = 1_000_000.0;

/// `u64::MAX` rounded to the nearest `f64`, which is `2^64` — one above the
/// largest representable amount. The guard below is therefore `>=`.
#[expect(
    clippy::cast_precision_loss,
    reason = "an upper bound only has to be at least u64::MAX, and this rounds up"
)]
const MAX_MICROS: f64 = u64::MAX as f64;

/// Converts a `total_cost_usd` figure to exact microdollars.
///
/// Returns `None` for a value the domain cannot represent — negative, NaN,
/// infinite, or beyond `u64` — because an unrepresentable cost is unknown
/// cost, and [`UsageReport::estimated_cost`] already models "unknown".
fn cost_in_micros(dollars: f64) -> Option<Usd> {
    let micros = (dollars * MICROS_PER_DOLLAR).round();
    if !micros.is_finite() || micros < 0.0 || micros >= MAX_MICROS {
        tracing::warn!(dollars, "harness reported a cost flyco cannot represent");
        return None;
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "guarded immediately above: finite, non-negative, within u64"
    )]
    Some(Usd::from_micros(micros as u64))
}

/// A content block inside an assistant or user message.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    /// Assistant prose. Dropped — the partial-message deltas carry it.
    Text,
    /// The model asked to run a tool.
    ToolUse {
        /// Harness-native call id.
        id: String,
        /// Tool name.
        name: String,
        /// Tool input.
        input: Value,
    },
    /// The harness fed a tool's output back to the model.
    ToolResult {
        /// The call this answers.
        tool_use_id: String,
        /// Whether the tool failed.
        #[serde(default)]
        is_error: bool,
    },
    /// Thinking blocks, images, and anything the SDK adds later.
    #[serde(other)]
    Other,
}

/// Per-message token accounting on an `assistant` message.
#[derive(Debug, Default, Deserialize)]
struct AssistantUsage {
    #[serde(default, rename = "input_tokens")]
    input: u64,
    #[serde(default, rename = "output_tokens")]
    output: u64,
    #[serde(default, rename = "cache_read_input_tokens")]
    cache_read: u64,
    #[serde(default, rename = "cache_creation_input_tokens")]
    cache_creation: u64,
}

impl AssistantUsage {
    /// Everything the model had in front of it plus what it produced —
    /// the numerator of the context-window gauge.
    const fn context_tokens(&self) -> u64 {
        self.input + self.cache_read + self.cache_creation + self.output
    }
}

#[derive(Debug, Deserialize)]
struct AssistantBody {
    model: String,
    content: Vec<ContentBlock>,
    #[serde(default)]
    usage: AssistantUsage,
}

/// A `user` message's content is either a plain string echo of what the
/// caller pushed, or the block list carrying tool results.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum UserContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Deserialize)]
struct UserBody {
    content: UserContent,
}

/// Cumulative token accounting on the terminal `result` message.
#[derive(Debug, Deserialize)]
struct ResultUsage {
    input_tokens: u64,
    output_tokens: u64,
}

/// Per-model accounting on the terminal `result` message. `context_window`
/// is the only place the Agent SDK names a window size.
#[derive(Debug, Default, Deserialize)]
struct ModelUsage {
    #[serde(default, rename = "contextWindow")]
    context_window: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ResultBody {
    subtype: String,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    result: Option<String>,
    usage: ResultUsage,
    #[serde(default)]
    total_cost_usd: Option<f64>,
    #[serde(default, rename = "modelUsage")]
    model_usage: BTreeMap<String, ModelUsage>,
}

/// `system` messages, discriminated on their `subtype`.
#[derive(Debug, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
enum SystemBody {
    /// The SDK is retrying an API call. `error: "rate_limit"` is the
    /// account usage-limit signal.
    ApiRetry {
        #[serde(default)]
        error: Option<String>,
    },
    /// A `SessionStore` batch could not be mirrored after retries.
    MirrorError {
        #[serde(default)]
        error: Option<String>,
    },
    /// `init`, `compact_boundary`, and everything else.
    #[serde(other)]
    Other,
}

/// Subscription rate-limit status. The only place the SDK names a reset
/// time, which is what [`HarnessEvent::UsageLimited`] wants.
#[derive(Debug, Deserialize)]
struct RateLimitInfo {
    status: RateLimitStatus,
    #[serde(default, rename = "resetsAt")]
    resets_at: Option<u64>,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RateLimitStatus {
    /// Requests are going through.
    Allowed,
    /// Going through, but close to the limit.
    AllowedWarning,
    /// The limit is hit; the account waits for the reset.
    Rejected,
    /// A status flyco does not model.
    #[serde(other)]
    Other,
}

/// A partial-message stream event.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamEventBody {
    ContentBlockDelta {
        delta: StreamDelta,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamDelta {
    TextDelta {
        text: String,
    },
    #[serde(other)]
    Other,
}

/// The SDK stream messages flyco understands.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SdkMessage {
    Assistant {
        message: AssistantBody,
    },
    User {
        message: UserBody,
    },
    Result(ResultBody),
    System(SystemBody),
    StreamEvent {
        event: StreamEventBody,
    },
    RateLimitEvent {
        rate_limit_info: RateLimitInfo,
    },
    /// Any message type flyco does not model.
    #[serde(other)]
    Unknown,
}

/// Translates one Claude Code session's SDK stream into [`HarnessEvent`]s.
///
/// Stateful in exactly two ways: it holds the id of the turn in flight, and
/// it remembers the most recent assistant message's model and token tally
/// so the terminal `result` can report a context-window gauge.
#[derive(Debug, Default)]
pub struct Normalizer {
    turn: Option<String>,
    last_assistant: Option<(String, u64)>,
}

impl Normalizer {
    /// A normalizer with no turn in flight.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            turn: None,
            last_assistant: None,
        }
    }

    /// Opens a turn. The driver calls this when it pushes a user message.
    ///
    /// Returns the [`HarnessEvent::TurnStarted`] the caller should emit.
    pub fn begin_turn(&mut self, turn_id: String) -> HarnessEvent {
        if let Some(previous) = self.turn.replace(turn_id.clone()) {
            tracing::warn!(
                previous_turn = %previous,
                new_turn = %turn_id,
                "a user message opened a turn while another was still in flight"
            );
        }
        self.last_assistant = None;
        HarnessEvent::TurnStarted { turn_id }
    }

    /// Whether a turn is currently in flight.
    #[must_use]
    pub const fn turn_in_flight(&self) -> bool {
        self.turn.is_some()
    }

    /// Maps one raw SDK message onto zero or more normalized events.
    pub fn normalize(&mut self, message: &Value) -> Vec<HarnessEvent> {
        let parsed: SdkMessage = match serde_json::from_value(message.clone()) {
            Ok(parsed) => parsed,
            Err(error) => {
                tracing::warn!(
                    %error,
                    kind = ?message.get("type"),
                    "an SDK message flyco claims to model did not match its shape"
                );
                return Vec::new();
            }
        };

        match parsed {
            SdkMessage::Assistant { message } => self.on_assistant(message),
            SdkMessage::User { message } => self.on_user(&message),
            SdkMessage::Result(body) => self.on_result(&body),
            SdkMessage::System(body) => Self::on_system(&body),
            SdkMessage::StreamEvent { event } => self.on_stream_event(&event),
            SdkMessage::RateLimitEvent { rate_limit_info } => Self::on_rate_limit(&rate_limit_info),
            SdkMessage::Unknown => {
                tracing::debug!(
                    kind = ?message.get("type"),
                    "dropping an SDK message type flyco does not model"
                );
                Vec::new()
            }
        }
    }

    /// The turn to stamp on an event, or `None` with a debug log.
    fn turn_id(&self, what: &'static str) -> Option<String> {
        if self.turn.is_none() {
            tracing::debug!(what, "dropping an SDK message that arrived outside a turn");
        }
        self.turn.clone()
    }

    fn on_assistant(&mut self, body: AssistantBody) -> Vec<HarnessEvent> {
        self.last_assistant = Some((body.model, body.usage.context_tokens()));
        let Some(turn_id) = self.turn_id("assistant") else {
            return Vec::new();
        };
        body.content
            .into_iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, name, input } => Some(HarnessEvent::ToolStarted {
                    turn_id: turn_id.clone(),
                    call_id: id,
                    tool: name,
                    input,
                }),
                // Text is carried by the partial-message deltas; tool
                // results never appear on an assistant message.
                _ => None,
            })
            .collect()
    }

    fn on_user(&self, body: &UserBody) -> Vec<HarnessEvent> {
        let blocks = match &body.content {
            UserContent::Blocks(blocks) => blocks,
            UserContent::Text(text) => {
                tracing::debug!(
                    chars = text.len(),
                    "dropping a plain-text user message echo"
                );
                return Vec::new();
            }
        };
        let Some(turn_id) = self.turn_id("user") else {
            return Vec::new();
        };
        blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    is_error,
                } => Some(HarnessEvent::ToolCompleted {
                    turn_id: turn_id.clone(),
                    call_id: tool_use_id.clone(),
                    ok: !is_error,
                }),
                _ => None,
            })
            .collect()
    }

    fn on_stream_event(&self, body: &StreamEventBody) -> Vec<HarnessEvent> {
        let StreamEventBody::ContentBlockDelta {
            delta: StreamDelta::TextDelta { text },
        } = body
        else {
            return Vec::new();
        };
        let Some(turn_id) = self.turn_id("stream_event") else {
            return Vec::new();
        };
        vec![HarnessEvent::AssistantDelta {
            turn_id,
            text: text.clone(),
        }]
    }

    /// The subscription rate-limit gauge. `rejected` is the account usage
    /// limit proper, and unlike `api_retry` it names a reset time.
    fn on_rate_limit(info: &RateLimitInfo) -> Vec<HarnessEvent> {
        if info.status == RateLimitStatus::Rejected {
            vec![HarnessEvent::UsageLimited {
                resets_at_unix: info.resets_at,
            }]
        } else {
            tracing::debug!(status = ?info.status, "dropping a non-blocking rate-limit update");
            Vec::new()
        }
    }

    fn on_system(body: &SystemBody) -> Vec<HarnessEvent> {
        match body {
            // An API retry whose cause is the account rate limit. This
            // arrives before `rate_limit_event` and carries no reset time,
            // so it is the early half of the same signal.
            SystemBody::ApiRetry { error } if error.as_deref() == Some("rate_limit") => {
                vec![HarnessEvent::UsageLimited {
                    resets_at_unix: None,
                }]
            }
            SystemBody::ApiRetry { error } => {
                tracing::debug!(?error, "dropping a non-rate-limit API retry");
                Vec::new()
            }
            SystemBody::MirrorError { error } => {
                tracing::warn!(
                    ?error,
                    "the Agent SDK gave up mirroring a transcript batch to the flyco store"
                );
                Vec::new()
            }
            SystemBody::Other => Vec::new(),
        }
    }

    fn on_result(&mut self, body: &ResultBody) -> Vec<HarnessEvent> {
        let Some(turn_id) = self.turn.take() else {
            tracing::debug!("dropping a result message that closed no known turn");
            return Vec::new();
        };

        if body.is_error {
            let error = body.result.clone().unwrap_or_else(|| body.subtype.clone());
            return vec![HarnessEvent::TurnFailed { turn_id, error }];
        }

        let context = self.last_assistant.as_ref().and_then(|(model, used)| {
            body.model_usage
                .get(model)
                .and_then(|usage| usage.context_window)
                .map(|size_tokens| ContextWindow {
                    used_tokens: *used,
                    size_tokens,
                })
        });

        vec![HarnessEvent::TurnCompleted {
            turn_id,
            usage: UsageReport {
                input_tokens: body.usage.input_tokens,
                output_tokens: body.usage.output_tokens,
                context,
                estimated_cost: body.total_cost_usd.and_then(cost_in_micros),
            },
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::{Normalizer, cost_in_micros};
    use flyco_core::harness::{ContextWindow, HarnessEvent, UsageReport};
    use flyco_core::money::Usd;
    use serde_json::Value;

    /// The normalizer dropped everything in the message.
    const NOTHING: [HarnessEvent; 0] = [];

    /// Loads a hand-written fixture in the shape the Agent SDK emits.
    fn sdk(name: &str) -> Value {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/sdk/").to_owned() + name;
        let text =
            std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {path}: {error}"));
        serde_json::from_str(&text).unwrap_or_else(|error| panic!("parse {path}: {error}"))
    }

    fn in_turn() -> Normalizer {
        let mut normalizer = Normalizer::new();
        let started = normalizer.begin_turn("turn-1".to_owned());
        assert_eq!(
            started,
            HarnessEvent::TurnStarted {
                turn_id: "turn-1".to_owned()
            }
        );
        normalizer
    }

    #[test]
    fn text_deltas_become_assistant_deltas() {
        let mut normalizer = in_turn();
        assert_eq!(
            normalizer.normalize(&sdk("stream_event_text_delta.json")),
            vec![HarnessEvent::AssistantDelta {
                turn_id: "turn-1".to_owned(),
                text: "Reading ".to_owned(),
            }]
        );
    }

    #[test]
    fn thinking_deltas_are_dropped() {
        let mut normalizer = in_turn();
        assert_eq!(
            normalizer.normalize(&sdk("stream_event_thinking_delta.json")),
            NOTHING
        );
    }

    #[test]
    fn assistant_tool_use_blocks_start_tools_and_text_blocks_do_not() {
        let mut normalizer = in_turn();
        assert_eq!(
            normalizer.normalize(&sdk("assistant_tool_use.json")),
            vec![HarnessEvent::ToolStarted {
                turn_id: "turn-1".to_owned(),
                call_id: "toolu_01Ab".to_owned(),
                tool: "Read".to_owned(),
                input: serde_json::json!({ "file_path": "/srv/work/src/main.rs" }),
            }]
        );
    }

    #[test]
    fn tool_results_complete_the_call_they_answer() {
        let mut normalizer = in_turn();
        assert_eq!(
            normalizer.normalize(&sdk("user_tool_result.json")),
            vec![HarnessEvent::ToolCompleted {
                turn_id: "turn-1".to_owned(),
                call_id: "toolu_01Ab".to_owned(),
                ok: true,
            }]
        );
        assert_eq!(
            normalizer.normalize(&sdk("user_tool_result_error.json")),
            vec![HarnessEvent::ToolCompleted {
                turn_id: "turn-1".to_owned(),
                call_id: "toolu_01Cd".to_owned(),
                ok: false,
            }]
        );
    }

    #[test]
    fn a_plain_text_user_echo_is_dropped_rather_than_failing() {
        let mut normalizer = in_turn();
        assert_eq!(normalizer.normalize(&sdk("user_text_echo.json")), NOTHING);
        assert!(normalizer.turn_in_flight());
    }

    #[test]
    fn a_result_closes_the_turn_with_usage_and_a_context_gauge() {
        let mut normalizer = in_turn();
        // The context numerator comes from the last assistant message.
        let _ = normalizer.normalize(&sdk("assistant_tool_use.json"));
        assert_eq!(
            normalizer.normalize(&sdk("result_success.json")),
            vec![HarnessEvent::TurnCompleted {
                turn_id: "turn-1".to_owned(),
                usage: UsageReport {
                    input_tokens: 4,
                    output_tokens: 210,
                    context: Some(ContextWindow {
                        used_tokens: 34_611,
                        size_tokens: 200_000,
                    }),
                    // 0.0421355 USD rounds to 42_136 µ$ (truncation would
                    // give 42_135).
                    estimated_cost: Some(Usd::from_micros(42_136)),
                },
            }]
        );
        assert!(!normalizer.turn_in_flight());
    }

    #[test]
    fn a_result_without_per_model_usage_reports_no_context_gauge() {
        let mut normalizer = in_turn();
        let _ = normalizer.normalize(&sdk("assistant_tool_use.json"));
        let events = normalizer.normalize(&sdk("result_no_model_usage.json"));
        let [HarnessEvent::TurnCompleted { usage, .. }] = events.as_slice() else {
            panic!("expected exactly one turn_completed, got {events:?}");
        };
        assert!(usage.context.is_none());
        assert!(usage.estimated_cost.is_none());
    }

    #[test]
    fn an_errored_result_fails_the_turn() {
        let mut normalizer = in_turn();
        assert_eq!(
            normalizer.normalize(&sdk("result_error.json")),
            vec![HarnessEvent::TurnFailed {
                turn_id: "turn-1".to_owned(),
                error: "Claude Code process exited with code 1".to_owned(),
            }]
        );
        assert!(!normalizer.turn_in_flight());
    }

    #[test]
    fn a_rate_limit_retry_is_the_usage_limit_signal() {
        let mut normalizer = in_turn();
        assert_eq!(
            normalizer.normalize(&sdk("system_api_retry_rate_limit.json")),
            vec![HarnessEvent::UsageLimited {
                resets_at_unix: None
            }]
        );
    }

    #[test]
    fn a_rejected_rate_limit_event_carries_the_reset_time() {
        let mut normalizer = in_turn();
        assert_eq!(
            normalizer.normalize(&sdk("rate_limit_event_rejected.json")),
            vec![HarnessEvent::UsageLimited {
                resets_at_unix: Some(1_787_000_000)
            }]
        );
    }

    #[test]
    fn a_rate_limit_warning_is_not_a_usage_limit() {
        let mut normalizer = in_turn();
        assert_eq!(
            normalizer.normalize(&sdk("rate_limit_event_warning.json")),
            NOTHING
        );
    }

    #[test]
    fn an_overloaded_retry_is_not_a_usage_limit() {
        let mut normalizer = in_turn();
        assert_eq!(
            normalizer.normalize(&sdk("system_api_retry_overloaded.json")),
            NOTHING
        );
    }

    #[test]
    fn system_init_mirror_errors_and_unknown_types_are_dropped() {
        let mut normalizer = in_turn();
        assert_eq!(normalizer.normalize(&sdk("system_init.json")), NOTHING);
        assert_eq!(
            normalizer.normalize(&sdk("system_mirror_error.json")),
            NOTHING
        );
        assert_eq!(
            normalizer.normalize(&sdk("unknown_message_type.json")),
            NOTHING
        );
        assert!(normalizer.turn_in_flight());
    }

    #[test]
    fn events_outside_a_turn_are_dropped() {
        let mut normalizer = Normalizer::new();
        assert_eq!(
            normalizer.normalize(&sdk("stream_event_text_delta.json")),
            NOTHING
        );
        assert_eq!(normalizer.normalize(&sdk("result_success.json")), NOTHING);
    }

    #[test]
    fn cost_conversion_rounds_and_refuses_nonsense() {
        assert_eq!(cost_in_micros(0.000_000_5), Some(Usd::from_micros(1)));
        assert_eq!(cost_in_micros(0.000_000_4), Some(Usd::ZERO));
        assert_eq!(cost_in_micros(1.5), Some(Usd::from_micros(1_500_000)));
        assert_eq!(cost_in_micros(-1.0), None);
        assert_eq!(cost_in_micros(f64::NAN), None);
        assert_eq!(cost_in_micros(f64::INFINITY), None);
    }
}
