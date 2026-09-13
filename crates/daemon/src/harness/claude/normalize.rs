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
//! - **Assistant text comes from partial-message deltas — usually.** The
//!   driver always sets `includePartialMessages`, so streamed text arrives
//!   as `stream_event` / `content_block_delta` / `text_delta` and the text
//!   blocks of the corresponding complete `assistant` message are dropped,
//!   because emitting both would duplicate every character. The exceptions
//!   are the messages that never stream — a synthetic message arrives as a
//!   complete `assistant` message out of nowhere — whose text *is* emitted,
//!   or the output would render as nothing.
//! - **Turn identity is flyco's, not the SDK's.** The Agent SDK has no turn
//!   id: it has a session, messages, and a terminal `result`. The driver
//!   mints a turn id when it pushes a user message and the normalizer
//!   stamps it onto everything until the `result` arrives. Events that
//!   arrive outside a turn (`system/init`, for instance) are dropped —
//!   except output the CLI produced on its own (a `local_command_output`),
//!   which has no turn to belong to and becomes
//!   [`HarnessEvent::LocalCommandOutput`].

use flyco_core::harness::{ContextWindow, HarnessEvent, UsageReport};
use flyco_core::money::Usd;
use flyco_core::wire::UsageWindow;
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
    /// Assistant prose.
    ///
    /// Emitted only when the partial-message stream never carried it —
    /// [`Normalizer::streamed_text`] is what tells the two cases apart.
    Text {
        /// The text.
        text: String,
    },
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
    /// Completion status for a manual or automatic compaction.
    Status {
        #[serde(default)]
        compact_result: Option<CompactResult>,
        #[serde(default)]
        compact_error: Option<String>,
    },
    /// The CLI answered a slash command itself.
    ///
    /// `/usage`, `/voice` and friends are local commands: they never reach
    /// the API, and what they print arrives on this message rather than as
    /// assistant text. (`/context` is not among them — flyco answers that
    /// one out of band; see [`crate::harness::claude::protocol`].)
    LocalCommandOutput {
        /// What the command printed, as Markdown.
        content: String,
    },
    /// `init`, `compact_boundary`, and everything else.
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CompactResult {
    Success,
    Failed,
}

/// Subscription rate-limit status, as `SDKRateLimitInfo` states it.
///
/// The only place the SDK names *which* window struck and when it turns
/// over, which is the whole of what
/// [`HarnessEvent::UsageLimited`] carries. Everything else on the SDK's
/// type is about overage credits — a different product decision the user
/// makes on claude.ai, not something a paused session waits for.
#[derive(Debug, Deserialize)]
struct RateLimitInfo {
    status: RateLimitStatus,
    #[serde(default, rename = "resetsAt")]
    resets_at: Option<i64>,
    #[serde(default, rename = "rateLimitType")]
    kind: Option<RateLimitKind>,
    /// How much of the window is spent, as an unbounded number.
    ///
    /// Read but not trusted: a `rejected` status is the account being
    /// refused, whatever the percentage rounds to, so the window is drawn
    /// full and this only fills in the reading for the statuses that are
    /// not refusals.
    #[serde(default)]
    utilization: Option<f64>,
}

/// Which window the SDK says a rate limit belongs to.
///
/// The vendor's own tokens, and the only thing that says whether a limit is
/// the five-hour one or a weekly per-model bucket. `Other` is a window this
/// build has not heard of: the limit is real and worth announcing, and
/// naming it after a guess would be worse than calling it the plan's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RateLimitKind {
    FiveHour,
    SevenDay,
    SevenDayOpus,
    SevenDaySonnet,
    SevenDayOverageIncluded,
    Overage,
    #[serde(other)]
    Other,
}

/// Minutes in the SDK's weekly windows.
const SEVEN_DAY_MINUTES: u32 = 7 * 24 * 60;

impl RateLimitKind {
    /// How long the window is and what part of the plan it covers.
    ///
    /// The same vocabulary the sidecar uses for the `/usage` snapshot, so a
    /// limit and the ring it fills are called the same thing: a weekly Opus
    /// bucket is `Weekly (Opus)` whichever of the two reported it.
    ///
    /// Overage is not a rolling window at all — it is credit the account
    /// buys — so it names no duration and reads as `Plan (overage)`.
    const fn window(self) -> (Option<u32>, Option<&'static str>) {
        match self {
            Self::FiveHour => (Some(5 * 60), None),
            Self::SevenDay => (Some(SEVEN_DAY_MINUTES), None),
            Self::SevenDayOpus => (Some(SEVEN_DAY_MINUTES), Some("Opus")),
            Self::SevenDaySonnet => (Some(SEVEN_DAY_MINUTES), Some("Sonnet")),
            Self::SevenDayOverageIncluded => (Some(SEVEN_DAY_MINUTES), Some("overage included")),
            Self::Overage => (None, Some("overage")),
            Self::Other => (None, None),
        }
    }
}

impl RateLimitInfo {
    /// This reading as the window flyco states limits in.
    ///
    /// A refusal is a full window by definition: the account asked and was
    /// told no, so the ring is drawn full whatever `utilization` rounds to.
    fn window(&self) -> UsageWindow {
        let (minutes, scope) = self.kind.map_or((None, None), RateLimitKind::window);
        let used = if self.status == RateLimitStatus::Rejected {
            100
        } else {
            self.utilization.map_or(0, |value| {
                #[expect(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "clamped to the range a percentage has before the cast"
                )]
                let percent = value.clamp(0.0, 100.0).round() as u8;
                percent
            })
        };
        UsageWindow::new(minutes, scope, used, self.resets_at)
    }
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
/// Stateful in exactly three ways: it holds the id of the turn in flight,
/// it remembers the most recent assistant message's model and token tally
/// so the terminal `result` can report a context-window gauge, and it notes
/// whether a message's text already arrived as deltas so complete
/// `assistant` messages are not echoed a second time.
#[derive(Debug, Default)]
pub struct Normalizer {
    turn: Option<String>,
    last_assistant: Option<(String, u64)>,
    /// The usage limit currently in force, as it was announced.
    ///
    /// Three different signals name one limit and two of them repeat while
    /// it lasts, so the conversation would otherwise fill with the same
    /// line. Cleared when the plan reports room again, which is what makes
    /// the *next* limit a new announcement rather than a duplicate of this
    /// one.
    limited: Option<UsageWindow>,
    /// Whether the partial-message stream has carried text since the last
    /// complete `assistant` message.
    ///
    /// A complete message's text blocks are redundant exactly when its
    /// deltas already streamed — the common case under
    /// `includePartialMessages`. A message whose text never streamed (the
    /// SDK's synthetic messages, which is what a local command's answer
    /// arrives as) is the exception the flag exists for: its text is
    /// emitted from the complete message instead of being dropped as a
    /// duplicate it never was.
    streamed_text: bool,
}

impl Normalizer {
    /// A normalizer with no turn in flight.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            turn: None,
            last_assistant: None,
            limited: None,
            streamed_text: false,
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
        self.streamed_text = false;
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
            SdkMessage::System(body) => self.on_system(&body),
            SdkMessage::StreamEvent { event } => self.on_stream_event(&event),
            SdkMessage::RateLimitEvent { rate_limit_info } => self.on_rate_limit(&rate_limit_info),
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
        // Whether this message's text already arrived as deltas. Taking it
        // re-arms the flag for the next message: a message whose text never
        // streamed — a local command's answer, or any other synthetic one —
        // gets its text emitted here, or it would be shown as nothing.
        let streamed = core::mem::take(&mut self.streamed_text);
        let turn_id = self.turn.clone();
        let mut events = Vec::new();
        for block in body.content {
            match block {
                ContentBlock::Text { text } if !streamed => match &turn_id {
                    Some(turn_id) => events.push(HarnessEvent::AssistantDelta {
                        turn_id: turn_id.clone(),
                        text,
                    }),
                    // Outside a turn the text has nothing to attach to —
                    // it is output the CLI produced on its own.
                    None => events.push(HarnessEvent::LocalCommandOutput { content: text }),
                },
                ContentBlock::ToolUse { id, name, input } => {
                    if let Some(turn_id) = &turn_id {
                        events.push(HarnessEvent::ToolStarted {
                            turn_id: turn_id.clone(),
                            call_id: id,
                            tool: name,
                            input,
                        });
                    }
                }
                _ => {}
            }
        }
        if turn_id.is_none() && events.is_empty() {
            tracing::debug!(
                "dropping an assistant message that arrived outside a turn with nothing to show"
            );
        }
        events
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

    fn on_stream_event(&mut self, body: &StreamEventBody) -> Vec<HarnessEvent> {
        let StreamEventBody::ContentBlockDelta {
            delta: StreamDelta::TextDelta { text },
        } = body
        else {
            return Vec::new();
        };
        let Some(turn_id) = self.turn_id("stream_event") else {
            return Vec::new();
        };
        // The complete message this delta belongs to must not repeat it.
        self.streamed_text = true;
        vec![HarnessEvent::AssistantDelta {
            turn_id,
            text: text.clone(),
        }]
    }

    /// The subscription rate-limit gauge. `rejected` is the account usage
    /// limit proper, and unlike `api_retry` it names both the window and
    /// when it turns over.
    ///
    /// The SDK emits this frame whenever the numbers move, so a session
    /// sitting inside a limit sees it repeatedly; [`Self::limited`]
    /// remembers what was announced so the conversation carries one line per
    /// limit instead of one per update. A status back below `rejected` is
    /// the limit ending, which clears that memory — the next refusal is a
    /// new limit and is announced again.
    fn on_rate_limit(&mut self, info: &RateLimitInfo) -> Vec<HarnessEvent> {
        if info.status != RateLimitStatus::Rejected {
            tracing::debug!(status = ?info.status, "dropping a non-blocking rate-limit update");
            self.limited = None;
            return Vec::new();
        }
        self.announce(info.window())
    }

    /// One announcement per limit, whichever signal named it.
    ///
    /// Three things report the same limit — the retry that precedes it, the
    /// `rate_limit_event` that names it, and the plan snapshot taken after
    /// every turn — and the conversation wants the fact once.
    fn announce(&mut self, window: UsageWindow) -> Vec<HarnessEvent> {
        if self.limited.as_ref() == Some(&window) {
            return Vec::new();
        }
        self.limited = Some(window.clone());
        vec![HarnessEvent::UsageLimited { window }]
    }

    /// The limit a plan snapshot describes, if it describes one.
    ///
    /// The third signal of issue #244, and the only one that is not an
    /// error: the `/usage` answer flycod takes after every turn says a
    /// window is spent and when it turns over, which is exactly a limit even
    /// though no turn has been refused yet. Reported through the same
    /// [`Self::announce`] as the other two, so a limit the stream is about
    /// to refuse a turn over is not announced twice.
    pub fn on_plan_usage(&mut self, windows: &[UsageWindow]) -> Vec<HarnessEvent> {
        if let Some(window) = flyco_core::blocking_window(windows) {
            return self.announce(window.clone());
        }
        // Every window has something left in it, so whatever limit was in
        // force has ended and the next one is news again.
        self.limited = None;
        Vec::new()
    }

    fn on_system(&mut self, body: &SystemBody) -> Vec<HarnessEvent> {
        match body {
            // An API retry whose cause is the account rate limit. This
            // arrives before `rate_limit_event` and names neither the window
            // nor a reset time, so it is the early half of the same signal:
            // the conversation says the plan is out, and the pause waits for
            // the frame that says which window and until when.
            SystemBody::ApiRetry { error } if error.as_deref() == Some("rate_limit") => {
                self.announce(UsageWindow::new(None, None, 100, None))
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
            SystemBody::Status {
                compact_result: Some(CompactResult::Success),
                ..
            } => vec![HarnessEvent::ContextCompacted],
            SystemBody::Status {
                compact_result: Some(CompactResult::Failed),
                compact_error,
            } => vec![HarnessEvent::ContextCompactionFailed {
                error: compact_error
                    .clone()
                    .unwrap_or_else(|| "Claude Code reported that compaction failed".to_owned()),
            }],
            SystemBody::LocalCommandOutput { content } => {
                vec![HarnessEvent::LocalCommandOutput {
                    content: content.clone(),
                }]
            }
            SystemBody::Status {
                compact_result: None,
                ..
            }
            | SystemBody::Other => Vec::new(),
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
    use flyco_core::wire::UsageWindow;
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
    fn assistant_tool_use_blocks_start_tools() {
        let mut normalizer = in_turn();
        let events = normalizer.normalize(&sdk("assistant_tool_use.json"));
        assert!(
            events.contains(&HarnessEvent::ToolStarted {
                turn_id: "turn-1".to_owned(),
                call_id: "toolu_01Ab".to_owned(),
                tool: "Read".to_owned(),
                input: serde_json::json!({ "file_path": "/srv/work/src/main.rs" }),
            }),
            "expected a tool_started among {events:?}"
        );
    }

    #[test]
    fn a_complete_message_repeats_text_its_deltas_already_carried() {
        let mut normalizer = in_turn();
        let _ = normalizer.normalize(&sdk("stream_event_text_delta.json"));
        // The text block of the complete message is the same prose the
        // delta already streamed: emitting it again would write every
        // character twice.
        let events = normalizer.normalize(&sdk("assistant_tool_use.json"));
        assert_eq!(
            events,
            vec![HarnessEvent::ToolStarted {
                turn_id: "turn-1".to_owned(),
                call_id: "toolu_01Ab".to_owned(),
                tool: "Read".to_owned(),
                input: serde_json::json!({ "file_path": "/srv/work/src/main.rs" }),
            }]
        );
    }

    #[test]
    fn a_message_whose_text_never_streamed_emits_it_here() {
        // A synthetic message — a local command's answer is the common
        // one — arrives complete with no deltas in front of it. Emitting
        // nothing would show the command's output as a blank.
        let mut normalizer = in_turn();
        let events = normalizer.normalize(&sdk("assistant_tool_use.json"));
        assert_eq!(
            events[0],
            HarnessEvent::AssistantDelta {
                turn_id: "turn-1".to_owned(),
                text: "I'll read the entry point first.".to_owned(),
            }
        );
    }

    #[test]
    fn an_out_of_turn_message_emits_its_text_as_command_output() {
        let mut normalizer = Normalizer::new();
        let events = normalizer.normalize(&sdk("assistant_tool_use.json"));
        assert_eq!(
            events[0],
            HarnessEvent::LocalCommandOutput {
                content: "I'll read the entry point first.".to_owned(),
            }
        );
    }

    #[test]
    fn a_local_commands_answer_is_output_not_a_turn() {
        let mut normalizer = in_turn();
        assert_eq!(
            normalizer.normalize(&sdk("system_local_command_output.json")),
            vec![HarnessEvent::LocalCommandOutput {
                content: "| Window | Used |\n|---|---|\n| 5-hour | 26% |".to_owned(),
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

    /// The early half of the signal: the SDK is retrying because the account
    /// is out, and it says nothing about which window or until when.
    #[test]
    fn a_rate_limit_retry_is_the_usage_limit_signal() {
        let mut normalizer = in_turn();
        let events = normalizer.normalize(&sdk("system_api_retry_rate_limit.json"));
        assert_eq!(
            events,
            vec![HarnessEvent::UsageLimited {
                window: UsageWindow::new(None, None, 100, None)
            }]
        );
        // A window nobody can place in time reads as the plan itself, which
        // is the only true thing left to call it.
        let [HarnessEvent::UsageLimited { window }] = events.as_slice() else {
            panic!("the retry is a usage limit: {events:?}");
        };
        assert_eq!(window.label, "Plan");
    }

    #[test]
    fn a_rejected_rate_limit_event_names_the_window_and_its_reset() {
        let mut normalizer = in_turn();
        assert_eq!(
            normalizer.normalize(&sdk("rate_limit_event_rejected.json")),
            vec![HarnessEvent::UsageLimited {
                window: UsageWindow::new(Some(300), None, 100, Some(1_787_000_000))
            }]
        );
    }

    /// A per-model weekly bucket, which is what the plan's own rings call
    /// `Weekly (Opus)`: the limit and the ring have to be the same name, or
    /// the paused state names a window the reader cannot find above the
    /// composer.
    #[test]
    fn a_rejected_per_model_weekly_bucket_is_named_after_the_model() {
        let mut normalizer = in_turn();
        let events = normalizer.normalize(&sdk("rate_limit_event_rejected_weekly_opus.json"));
        let [HarnessEvent::UsageLimited { window }] = events.as_slice() else {
            panic!("a rejected weekly bucket is a usage limit: {events:?}");
        };
        assert_eq!(window.label, "Weekly (Opus)");
        assert_eq!(window.window_minutes, Some(7 * 24 * 60));
        assert_eq!(window.resets_at_unix, Some(1_787_568_000));
    }

    #[test]
    fn a_rate_limit_warning_is_not_a_usage_limit() {
        let mut normalizer = in_turn();
        assert_eq!(
            normalizer.normalize(&sdk("rate_limit_event_warning.json")),
            NOTHING
        );
    }

    /// One line per limit, however many times the vendor repeats itself.
    ///
    /// The SDK re-emits `rate_limit_event` whenever the numbers move, and the
    /// plan snapshot is taken after every turn, so a session sitting inside a
    /// five-hour limit would otherwise narrate it over and over.
    #[test]
    fn a_limit_already_announced_is_not_announced_again() {
        let mut normalizer = in_turn();
        assert_eq!(
            normalizer
                .normalize(&sdk("rate_limit_event_rejected.json"))
                .len(),
            1
        );
        assert_eq!(
            normalizer.normalize(&sdk("rate_limit_event_rejected.json")),
            NOTHING
        );
        // …and the snapshot taken after the refused turn says the same thing
        // about the same window, which is not a second limit.
        assert_eq!(
            normalizer.on_plan_usage(&[UsageWindow::new(
                Some(300),
                None,
                100,
                Some(1_787_000_000)
            )]),
            NOTHING
        );
    }

    /// The third signal of issue #244: the `/usage` answer, not an error.
    ///
    /// A window the CLI reports spent with a reset time is the limit, whether
    /// or not a turn has been refused yet — and the window that unblocks the
    /// account *last* is the one to wait for.
    #[test]
    fn a_plan_snapshot_with_a_spent_window_is_a_usage_limit() {
        let mut normalizer = in_turn();
        let weekly = UsageWindow::new(Some(10_080), None, 100, Some(1_787_568_000));
        assert_eq!(
            normalizer.on_plan_usage(&[
                UsageWindow::new(Some(300), None, 100, Some(1_787_000_000)),
                weekly.clone(),
            ]),
            vec![HarnessEvent::UsageLimited { window: weekly }]
        );

        // Room again in every window: the limit is over, and the next one is
        // news rather than a repeat.
        assert_eq!(
            normalizer.on_plan_usage(&[UsageWindow::new(Some(300), None, 4, Some(1_787_100_000))]),
            NOTHING
        );
        assert_eq!(
            normalizer
                .normalize(&sdk("rate_limit_event_rejected.json"))
                .len(),
            1
        );
    }

    #[test]
    fn compaction_status_is_reported_outside_a_turn() {
        let mut normalizer = Normalizer::new();
        assert_eq!(
            normalizer.normalize(&sdk("system_status_compact_success.json")),
            vec![HarnessEvent::ContextCompacted]
        );
        assert_eq!(
            normalizer.normalize(&sdk("system_status_compact_failed.json")),
            vec![HarnessEvent::ContextCompactionFailed {
                error: "context window could not be summarized".to_owned()
            }]
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
