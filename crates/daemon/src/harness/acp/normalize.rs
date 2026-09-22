//! `session/update` notifications, normalized.
//!
//! ACP streams a conversation as [`SessionUpdate`]s — message chunks, tool
//! calls, plans, usage meters — and flyco's transcript speaks
//! [`flyco_core::HarnessEvent`]. This module is the whole of the
//! translation: what a turn identifier is when the protocol has none, how a
//! tool call's title becomes a tool name, which updates flyco renders and
//! which it holds as state. Kept apart from the driver for the same reason
//! Codex's was: the mapping is the part worth testing, and a test that
//! needs no agent process is a test that runs everywhere.

use std::collections::BTreeSet;

use aither_acp::{
    ConfigOptionValue, ConfigSelectOptions, ContentBlock, Plan, PlanEntryStatus, SessionUpdate,
    StopReason, ToolCallStatus, UsageUpdate,
};
use flyco_core::{ContextWindow, HarnessEvent, ModelOption, UsageReport, UsageWindow, Usd};
use serde_json::{Value, json};

/// What one `session/update` became, in flyco's vocabulary.
///
/// Most updates are transcript events; a few are reports that travel their
/// own channel — the command palette reaches the browser as
/// [`SessionOutput::Commands`](crate::harness::SessionOutput), not as an
/// event — so the two are kept distinct here rather than re-split by the
/// caller.
#[derive(Debug, Default)]
pub struct Normalized {
    /// Transcript events.
    pub events: Vec<HarnessEvent>,
    /// A fresh command palette, when the update carried one.
    pub commands: Option<Vec<flyco_core::HarnessCommand>>,
    /// A fresh model list, when the update's config options carried one.
    pub models: Option<Vec<ModelOption>>,
    /// The model the session now reports running on, when it changed.
    pub model: Option<String>,
    /// The agent's current mode id, when it changed.
    pub mode: Option<String>,
}

/// The stateful half of the mapping.
///
/// Updates arrive unordered relative to turns — a `usage_update` has no
/// turn to key on — so what they describe is accumulated here and consulted
/// when a turn completes or `/context` asks.
#[derive(Debug, Default)]
pub struct Normalizer {
    /// The last plan the agent published, rendered, for change detection —
    /// `Plan` carries no `PartialEq`, so what is compared is what would be
    /// shown.
    plan: Option<String>,
    /// The window fill the agent last reported.
    usage: Option<UsageUpdate>,
    /// Plan-usage windows already announced as exhausted.
    ///
    /// A window at a hundred percent is announced once: the app-server
    /// reports the same snapshot on every refresh, and a transcript that
    /// repeated "the 5-hour window is spent" on each would be noise, not
    /// information. A label drops out of the set when the window reports
    /// room again, so a limit struck twice is still announced twice.
    announced_limits: BTreeSet<String>,
}

impl Normalizer {
    /// One notification's worth of transcript events and reports.
    ///
    /// `turn_id` is the driver's minted identifier for the turn in flight —
    /// ACP carries no turn ids of its own, and flyco's events all key on
    /// one, so the driver invents them (`turn-1`, `turn-2`, …) and every
    /// event a turn produces borrows its number.
    pub fn on_update(&mut self, update: &SessionUpdate, turn_id: &str) -> Normalized {
        let mut out = Normalized::default();
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                if let Some(text) = block_text(&chunk.content) {
                    out.events.push(HarnessEvent::AssistantDelta {
                        turn_id: turn_id.to_owned(),
                        text,
                    });
                }
            }
            // Reasoning is not rendered: flyco's transcript has no thinking
            // block, the same choice the Claude driver makes. Session-info
            // and unrecognised updates carry nothing flyco renders either.
            SessionUpdate::AgentThoughtChunk(_)
            | SessionUpdate::UserMessageChunk(_)
            | SessionUpdate::SessionInfoUpdate(_)
            | SessionUpdate::Other(_) => {}
            SessionUpdate::Plan(plan) => {
                let rendered = render_plan(plan);
                if self.plan.as_deref() != Some(rendered.as_str()) {
                    self.plan = Some(rendered.clone());
                    out.events
                        .push(HarnessEvent::LocalCommandOutput { content: rendered });
                }
            }
            SessionUpdate::ToolCall(call) => {
                out.events.push(HarnessEvent::ToolStarted {
                    turn_id: turn_id.to_owned(),
                    call_id: call.tool_call_id.clone(),
                    tool: tool_name(call.title.as_str(), call.kind),
                    input: call
                        .raw_input
                        .clone()
                        .unwrap_or_else(|| tool_call_input(call)),
                });
                // Agents report a finished call as its initial state too —
                // a call announced already completed still needs its
                // `ToolCompleted`, or the row spins for ever.
                if let Some(ok) = call.status.and_then(finished) {
                    out.events.push(HarnessEvent::ToolCompleted {
                        turn_id: turn_id.to_owned(),
                        call_id: call.tool_call_id.clone(),
                        ok,
                    });
                }
            }
            SessionUpdate::ToolCallUpdate(update) => {
                if let Some(ok) = update.status.and_then(finished) {
                    out.events.push(HarnessEvent::ToolCompleted {
                        turn_id: turn_id.to_owned(),
                        call_id: update.tool_call_id.clone(),
                        ok,
                    });
                }
            }
            SessionUpdate::AvailableCommandsUpdate(update) => {
                out.commands = Some(
                    update
                        .available_commands
                        .iter()
                        .map(|command| flyco_core::HarnessCommand {
                            name: command.name.clone(),
                            description: command.description.clone(),
                            argument_hint: command.input.as_ref().map(|input| input.hint.clone()),
                        })
                        .collect(),
                );
            }
            SessionUpdate::CurrentModeUpdate(update) => {
                out.mode = Some(update.current_mode_id.clone());
            }
            SessionUpdate::ConfigOptionUpdate(update) => {
                let (models, model) = read_model_option(&update.config_options);
                out.models = models;
                out.model = model;
            }
            SessionUpdate::UsageUpdate(usage) => {
                self.usage = Some(usage.clone());
            }
        }
        out
    }

    /// The turn-completed or turn-failed event a `session/prompt` answer
    /// means, with the usage the agent has last reported folded in.
    #[must_use]
    pub fn on_prompt_result(&self, stop_reason: StopReason, turn_id: &str) -> HarnessEvent {
        match stop_reason {
            StopReason::EndTurn | StopReason::Cancelled => HarnessEvent::TurnCompleted {
                turn_id: turn_id.to_owned(),
                usage: self.usage_report(),
            },
            reason => HarnessEvent::TurnFailed {
                turn_id: turn_id.to_owned(),
                error: format!("the agent ended the turn with stop reason `{reason:?}`"),
            },
        }
    }

    /// The usage report a turn's completion carries.
    ///
    /// ACP's `usage_update` is a window fill plus an optional cumulative
    /// cost; it does not count input and output tokens separately, so those
    /// counters are zero here and the gauge and the cost are the real data.
    fn usage_report(&self) -> UsageReport {
        UsageReport {
            input_tokens: 0,
            output_tokens: 0,
            context: self.usage.as_ref().map(|usage| ContextWindow {
                used_tokens: usage.used,
                size_tokens: usage.size,
            }),
            estimated_cost: self
                .usage
                .as_ref()
                .and_then(|usage| usage.cost.as_ref())
                .map(|cost| {
                    #[expect(
                        clippy::cast_possible_truncation,
                        clippy::cast_sign_loss,
                        reason = "a turn's spend in cents is non-negative and far inside u64"
                    )]
                    Usd::from_cents((cost.amount * 100.0).round().max(0.0) as u64)
                }),
        }
    }

    /// The window fill the agent last reported, for `/context`.
    #[must_use]
    pub fn window(&self) -> Option<ContextWindow> {
        self.usage.as_ref().map(|usage| ContextWindow {
            used_tokens: usage.used,
            size_tokens: usage.size,
        })
    }

    /// The plan-usage windows an account answer carries, plus a
    /// `UsageLimited` event for each window that has newly run out.
    ///
    /// The windows are read tolerantly rather than from one vendor's
    /// schema: any object in the answer holding a `usedPercent` number is a
    /// window, its `windowDurationMins` and `resetsAt` are read when
    /// present, and its scope is the key it was found under. Codex's
    /// `account/rateLimits/read` answers `{rateLimits: {primary: …,
    /// secondary: …}}` in exactly this shape; an agent that answers in
    /// another shape reports no windows rather than a wrong one.
    pub fn on_plan_usage(&mut self, result: &Value) -> Vec<HarnessEvent> {
        let windows = usage_windows(result);
        let mut events = Vec::new();
        for window in &windows {
            if window.is_exhausted() {
                if self.announced_limits.insert(window.label.clone()) {
                    events.push(HarnessEvent::UsageLimited {
                        window: window.clone(),
                    });
                }
            } else {
                self.announced_limits.remove(&window.label);
            }
        }
        events
    }
}

/// Whether a tool-call status is a finished one, and if so whether it
/// succeeded.
const fn finished(status: ToolCallStatus) -> Option<bool> {
    match status {
        ToolCallStatus::Completed => Some(true),
        ToolCallStatus::Failed => Some(false),
        ToolCallStatus::Pending | ToolCallStatus::InProgress => None,
    }
}

/// The text one content block contributes to the transcript.
///
/// Text passes through; everything else becomes a marker in square
/// brackets rather than vanishing — a transcript that skips an image the
/// agent produced reads as if it never happened, and a marker at least
/// says something was there.
fn block_text(block: &ContentBlock) -> Option<String> {
    let text = match block {
        ContentBlock::Text(content) => content.text.clone(),
        ContentBlock::Image(image) => {
            format!(
                "[image: {}]",
                image.uri.as_deref().unwrap_or(&image.mime_type)
            )
        }
        ContentBlock::Audio(audio) => format!("[audio: {}]", audio.mime_type),
        ContentBlock::ResourceLink(link) => {
            let title = link.title.as_deref().unwrap_or(&link.name);
            format!("[{title}]({})", link.uri)
        }
        ContentBlock::Resource(resource) => {
            let embedded = &resource.resource;
            embedded
                .text
                .clone()
                .unwrap_or_else(|| format!("[resource: {}]", embedded.uri))
        }
    };
    if text.is_empty() { None } else { Some(text) }
}

/// What flyco calls the tool a `ToolCall` describes.
///
/// The title is the human answer — "Edit `src/main.rs`" — and the kind is
/// the fallback for a call that arrived without one: `execute` reads better
/// in a transcript than an empty name.
pub(super) fn tool_name(title: &str, kind: Option<aither_acp::ToolKind>) -> String {
    if !title.is_empty() {
        return title.to_owned();
    }
    let kind = match kind.unwrap_or_default() {
        aither_acp::ToolKind::Read => "read",
        aither_acp::ToolKind::Edit => "edit",
        aither_acp::ToolKind::Delete => "delete",
        aither_acp::ToolKind::Move => "move",
        aither_acp::ToolKind::Search => "search",
        aither_acp::ToolKind::Execute => "execute",
        aither_acp::ToolKind::Think => "think",
        aither_acp::ToolKind::Fetch => "fetch",
        aither_acp::ToolKind::SwitchMode => "switch mode",
        aither_acp::ToolKind::Other => "tool",
    };
    kind.to_owned()
}

/// A `ToolCall` without `rawInput`, restated as input JSON.
///
/// Locations and diffs are the content a call carries when it names no
/// arguments — rendered into the input field so the transcript row has
/// something to show.
pub(super) fn tool_call_input(call: &aither_acp::ToolCall) -> Value {
    json!({
        "locations": call.locations.iter().map(|location| {
            location.line.map_or_else(
                || location.path.display().to_string(),
                |line| format!("{}:{line}", location.path.display()),
            )
        }).collect::<Vec<_>>(),
        "content": call.content.iter().map(|content| match content {
            aither_acp::ToolCallContent::Content { content } => block_text(content)
                .unwrap_or_else(|| "[content]".to_owned()),
            aither_acp::ToolCallContent::Diff(diff) => format!("diff {}", diff.path.display()),
            aither_acp::ToolCallContent::Terminal { terminal_id } => {
                format!("terminal {terminal_id}")
            }
        }).collect::<Vec<_>>(),
    })
}

/// A plan rendered as a Markdown task list.
///
/// The transcript has no plan block, so a plan update becomes a
/// [`LocalCommandOutput`](HarnessEvent::LocalCommandOutput) — what a slash
/// command's answer looks like — rather than being folded into the turn's
/// prose it does not belong to.
fn render_plan(plan: &Plan) -> String {
    use std::fmt::Write as _;
    let mut out = String::from("**Plan**\n");
    for entry in &plan.entries {
        let mark = match entry.status {
            PlanEntryStatus::Completed => "[x]",
            PlanEntryStatus::InProgress | PlanEntryStatus::Pending => "[ ]",
        };
        let _ = writeln!(out, "- {mark} {}", entry.content);
    }
    out
}

/// Reads a config-option set for the model picker.
///
/// The `model`-category option's select values are the model list; the
/// `thought_level`-category option's values are the effort levels every
/// model shares (Codex's `reasoning_effort`), which is how both reach
/// [`ModelOption`] without the driver knowing either vendor.
pub(super) fn read_model_option(
    options: &[aither_acp::ConfigOption],
) -> (Option<Vec<ModelOption>>, Option<String>) {
    let mut models: Option<Vec<ModelOption>> = None;
    let mut current = None;
    let mut efforts = Vec::new();
    let mut default_effort = None;
    for option in options {
        match option.category.as_deref() {
            Some("model") => {
                current = match &option.current_value {
                    Some(ConfigOptionValue::Selected(value)) => Some(value.clone()),
                    _ => None,
                };
                if let Some(select) = &option.options {
                    models = Some(
                        select_values(select)
                            .into_iter()
                            .map(|(value, name, description)| ModelOption {
                                is_default: current.as_deref() == Some(value.as_str()),
                                id: value,
                                label: name,
                                description,
                                efforts: Vec::new(),
                                default_effort: None,
                            })
                            .collect(),
                    );
                }
            }
            Some("thought_level") => {
                if let Some(select) = &option.options {
                    efforts = select_values(select)
                        .into_iter()
                        .map(|(value, _, _)| value)
                        .collect();
                }
                if let Some(ConfigOptionValue::Selected(value)) = &option.current_value {
                    default_effort = Some(value.clone());
                }
            }
            _ => {}
        }
    }
    // The effort list belongs to every model: Codex names the levels once,
    // for the session, not per row.
    if let Some(list) = models.as_mut()
        && !efforts.is_empty()
    {
        for model in list.iter_mut() {
            model.efforts.clone_from(&efforts);
            model.default_effort.clone_from(&default_effort);
        }
    }
    (models, current)
}

/// Every selectable value of a [`ConfigSelectOptions`], flattened.
///
/// Grouped and flat are the same list either way — a group header only
/// adds a label — so both spellings collect into the one shape.
fn select_values(options: &ConfigSelectOptions) -> Vec<(String, String, String)> {
    let values = |option: &aither_acp::ConfigSelectOption| {
        (
            option.value.clone(),
            option.name.clone(),
            option.description.clone().unwrap_or_default(),
        )
    };
    match options {
        ConfigSelectOptions::Flat(list) => list.iter().map(values).collect(),
        ConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter().map(values))
            .collect(),
    }
}

/// Every usage window a JSON answer describes, wherever it nested them.
fn usage_windows(result: &Value) -> Vec<UsageWindow> {
    let mut windows = Vec::new();
    collect_windows(result, None, &mut windows);
    windows
}

fn collect_windows(value: &Value, scope: Option<&str>, windows: &mut Vec<UsageWindow>) {
    match value {
        Value::Object(map) => {
            if let Some(percent) = map
                .get("usedPercent")
                .or_else(|| map.get("used_percent"))
                .and_then(Value::as_f64)
            {
                let minutes = map
                    .get("windowDurationMins")
                    .or_else(|| map.get("window_duration_mins"))
                    .and_then(Value::as_u64)
                    .map(|mins| u32::try_from(mins).unwrap_or(u32::MAX));
                let resets = map
                    .get("resetsAt")
                    .or_else(|| map.get("resets_at"))
                    .and_then(|value| {
                        value.as_i64().or_else(|| {
                            // ISO 8601 answers land here too; a string that
                            // does not parse is simply a window with no
                            // known end.
                            value.as_str().and_then(|text| {
                                time::OffsetDateTime::parse(
                                    text,
                                    &time::format_description::well_known::Rfc3339,
                                )
                                .ok()
                                .map(time::OffsetDateTime::unix_timestamp)
                            })
                        })
                    });
                #[expect(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "a reported fill is a percentage; out-of-range agents lose their window rather than crash it"
                )]
                windows.push(UsageWindow::new(
                    minutes,
                    scope,
                    percent.round().clamp(0.0, 100.0) as u8,
                    resets,
                ));
                return;
            }
            for (key, child) in map {
                collect_windows(child, Some(key), windows);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_windows(item, scope, windows);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use aither_acp::{ContentChunk, SessionUpdate, StopReason, TextContent, UsageUpdate};
    use flyco_core::HarnessEvent;
    use serde_json::json;

    use super::Normalizer;

    fn chunk(text: &str) -> SessionUpdate {
        SessionUpdate::AgentMessageChunk(ContentChunk {
            content: aither_acp::ContentBlock::Text(TextContent {
                text: text.to_owned(),
                annotations: None,
                meta: None,
            }),
            message_id: None,
            meta: None,
        })
    }

    #[test]
    fn a_text_chunk_is_an_assistant_delta() {
        let mut normalizer = Normalizer::default();
        let out = normalizer.on_update(&chunk("hello"), "turn-1");
        assert_eq!(
            out.events,
            [HarnessEvent::AssistantDelta {
                turn_id: "turn-1".to_owned(),
                text: "hello".to_owned(),
            }]
        );
    }

    #[test]
    fn an_end_turn_completes_the_turn_with_the_known_fill() {
        let mut normalizer = Normalizer::default();
        normalizer.on_update(
            &SessionUpdate::UsageUpdate(UsageUpdate {
                used: 10_000,
                size: 200_000,
                cost: None,
                meta: None,
            }),
            "turn-1",
        );
        let event = normalizer.on_prompt_result(StopReason::EndTurn, "turn-1");
        let HarnessEvent::TurnCompleted { usage, .. } = event else {
            panic!("expected TurnCompleted, got {event:?}");
        };
        assert_eq!(
            usage.context.expect("a fill was reported").size_tokens,
            200_000
        );
    }

    #[test]
    fn a_refusal_fails_the_turn() {
        let normalizer = Normalizer::default();
        assert!(matches!(
            normalizer.on_prompt_result(StopReason::Refusal, "turn-1"),
            HarnessEvent::TurnFailed { .. }
        ));
    }

    #[test]
    fn a_spent_window_is_announced_once() {
        let mut normalizer = Normalizer::default();
        let answer = json!({
            "rateLimits": {
                "primary": {"usedPercent": 100, "windowDurationMins": 300, "resetsAt": 1_787_100_000},
                "secondary": {"usedPercent": 12, "windowDurationMins": 10080},
            }
        });
        let events = normalizer.on_plan_usage(&answer);
        assert_eq!(events.len(), 1, "only the exhausted window announces");
        let again = normalizer.on_plan_usage(&answer);
        assert!(again.is_empty(), "the same reading must not repeat");
        let cleared = normalizer.on_plan_usage(&json!({
            "rateLimits": {"primary": {"usedPercent": 40, "windowDurationMins": 300}}
        }));
        assert!(cleared.is_empty());
    }
}
