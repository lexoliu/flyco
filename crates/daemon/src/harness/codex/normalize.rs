//! App-server notifications → [`HarnessEvent`].
//!
//! The app-server stream is a superset of what flyco models. This module
//! maps the notifications flyco has a normalized shape for and logs the
//! rest at `debug`. Unrecognized `type` tags on items are dropped the same
//! way: Codex adds item kinds on its own release schedule, and a daemon that
//! failed on the first unknown one would break every time `OpenAI` shipped a
//! feature.

use flyco_core::{ContextWindow, HarnessEvent, UsageReport, UsageWindow};
use serde::Deserialize;
use serde_json::Value;

use super::protocol::{RateLimitSnapshot, method};

/// Live turn and usage state the driver needs to stamp events.
#[derive(Debug)]
pub struct Normalizer {
    /// The turn currently in flight, as the app-server names it.
    turn_id: Option<String>,
    /// Last usage snapshot, applied when the turn completes.
    usage: UsageReport,
}

impl Normalizer {
    /// A fresh normalizer with no turn open.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            turn_id: None,
            usage: UsageReport {
                input_tokens: 0,
                output_tokens: 0,
                context: None,
                estimated_cost: None,
            },
        }
    }

    /// The turn currently in flight, if any.
    #[must_use]
    pub fn turn_id(&self) -> Option<&str> {
        self.turn_id.as_deref()
    }

    /// The most recent usage snapshot.
    #[must_use]
    pub const fn usage(&self) -> UsageReport {
        self.usage
    }

    /// Interprets one notification. Unknown methods produce no events.
    pub fn on_notification(&mut self, method_name: &str, params: &Value) -> Vec<HarnessEvent> {
        match method_name {
            method::TURN_STARTED => self.on_turn_started(params),
            method::TURN_COMPLETED => self.on_turn_completed(params).into_iter().collect(),
            method::AGENT_MESSAGE_DELTA => self.on_agent_delta(params).into_iter().collect(),
            method::ITEM_STARTED => self.on_item_started(params).into_iter().collect(),
            method::ITEM_COMPLETED => self.on_item_completed(params).into_iter().collect(),
            method::TOKEN_USAGE => {
                self.on_token_usage(params);
                Vec::new()
            }
            method::ERROR => Self::on_error(params).into_iter().collect(),
            other => {
                tracing::debug!(
                    method = other,
                    "dropping an unmodeled app-server notification"
                );
                Vec::new()
            }
        }
    }

    fn on_turn_started(&mut self, params: &Value) -> Vec<HarnessEvent> {
        let Some(turn_id) = turn_id_of(params) else {
            tracing::warn!("turn/started named no turn id");
            return Vec::new();
        };
        self.turn_id = Some(turn_id.clone());
        vec![HarnessEvent::TurnStarted { turn_id }]
    }

    fn on_turn_completed(&mut self, params: &Value) -> Option<HarnessEvent> {
        let turn_id = turn_id_of(params).or_else(|| self.turn_id.clone())?;
        self.turn_id = None;
        let status = params
            .pointer("/turn/status")
            .and_then(Value::as_str)
            .unwrap_or("completed");
        match status {
            "completed" => Some(HarnessEvent::TurnCompleted {
                turn_id,
                usage: self.usage,
            }),
            "interrupted" | "failed" => Some(HarnessEvent::TurnFailed {
                turn_id,
                error: status.to_owned(),
            }),
            other => {
                tracing::warn!(status = other, "turn/completed named an unknown status");
                Some(HarnessEvent::TurnFailed {
                    turn_id,
                    error: other.to_owned(),
                })
            }
        }
    }

    fn on_agent_delta(&self, params: &Value) -> Option<HarnessEvent> {
        let turn_id = self.turn_id.clone()?;
        let text = params.get("delta").and_then(Value::as_str)?;
        Some(HarnessEvent::AssistantDelta {
            turn_id,
            text: text.to_owned(),
        })
    }

    fn on_item_started(&self, params: &Value) -> Option<HarnessEvent> {
        let turn_id = self.turn_id.clone()?;
        let item = params.get("item")?;
        let item_type = item.get("type").and_then(Value::as_str)?;
        match item_type {
            "commandExecution" | "mcpToolCall" | "dynamicToolCall" => {
                Some(HarnessEvent::ToolStarted {
                    turn_id,
                    call_id: item_id(item),
                    tool: tool_name(item, item_type),
                    input: item.get("command").cloned().unwrap_or_else(|| item.clone()),
                })
            }
            _ => None,
        }
    }

    fn on_item_completed(&self, params: &Value) -> Option<HarnessEvent> {
        let item = params.get("item")?;
        let item_type = item.get("type").and_then(Value::as_str)?;
        match item_type {
            "commandExecution" | "mcpToolCall" | "dynamicToolCall" => {
                let turn_id = self.turn_id.clone()?;
                let status = item.get("status").and_then(Value::as_str);
                Some(HarnessEvent::ToolCompleted {
                    turn_id,
                    call_id: item_id(item),
                    ok: status != Some("failed") && status != Some("declined"),
                })
            }
            "contextCompaction" => Some(HarnessEvent::ContextCompacted),
            _ => None,
        }
    }

    fn on_token_usage(&mut self, params: &Value) {
        let Some(usage) = params.get("tokenUsage") else {
            return;
        };
        let total = usage.get("total").unwrap_or(usage);
        self.usage.input_tokens =
            u64_field(total, "inputTokens").unwrap_or(self.usage.input_tokens);
        self.usage.output_tokens =
            u64_field(total, "outputTokens").unwrap_or(self.usage.output_tokens);
        if let Some(size) = u64_field(usage, "modelContextWindow").filter(|size| *size > 0) {
            let used = u64_field(total, "totalTokens").unwrap_or_else(|| {
                self.usage
                    .input_tokens
                    .saturating_add(self.usage.output_tokens)
            });
            self.usage.context = Some(ContextWindow {
                used_tokens: used,
                size_tokens: size,
            });
        }
        if let Some(micros) = u64_field(usage, "estimatedUsageUsdMicros")
            .or_else(|| u64_field(total, "estimatedUsageUsdMicros"))
        {
            self.usage.estimated_cost = Some(flyco_core::Usd::from_micros(micros));
        }
    }

    fn on_error(params: &Value) -> Option<HarnessEvent> {
        let will_retry = params
            .get("willRetry")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if will_retry {
            tracing::info!("the app-server is retrying a turn internally");
            return None;
        }
        let info = params
            .pointer("/error/codexErrorInfo")
            .and_then(Value::as_str)
            .unwrap_or("");
        if info.eq_ignore_ascii_case("UsageLimitExceeded")
            || info.eq_ignore_ascii_case("rateLimitExceeded")
            || info.eq_ignore_ascii_case("SessionBudgetExceeded")
        {
            return Some(HarnessEvent::UsageLimited {
                resets_at_unix: None,
            });
        }
        None
    }
}

impl Default for Normalizer {
    fn default() -> Self {
        Self::new()
    }
}

fn turn_id_of(params: &Value) -> Option<String> {
    params
        .pointer("/turn/id")
        .and_then(Value::as_str)
        .or_else(|| params.get("turnId").and_then(Value::as_str))
        .map(str::to_owned)
}

fn item_id(item: &Value) -> String {
    item.get("id")
        .and_then(Value::as_str)
        .or_else(|| item.get("processId").and_then(Value::as_str))
        .unwrap_or("item")
        .to_owned()
}

fn tool_name(item: &Value, item_type: &str) -> String {
    item.get("tool")
        .and_then(Value::as_str)
        .or_else(|| item.get("name").and_then(Value::as_str))
        .unwrap_or(item_type)
        .to_owned()
}

fn u64_field(value: &Value, field: &str) -> Option<u64> {
    value.get(field).and_then(Value::as_u64)
}

/// Shape of a server→client approval request, as flycod needs it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalParams {
    /// Thread the approval belongs to.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Turn the approval belongs to.
    #[serde(default)]
    pub turn_id: Option<String>,
    /// Nested item, when the server wraps the payload.
    #[serde(default)]
    pub item: Option<Value>,
    /// Command string, when present at the top level.
    #[serde(default)]
    pub command: Option<String>,
}

impl ApprovalParams {
    /// Tool name shown in flyco's approval UI.
    #[must_use]
    pub fn tool(&self, method_name: &str) -> String {
        match method_name {
            method::COMMAND_APPROVAL => "commandExecution".to_owned(),
            method::FILE_CHANGE_APPROVAL => "fileChange".to_owned(),
            method::PERMISSIONS_APPROVAL => "permissions".to_owned(),
            other => other.to_owned(),
        }
    }

    /// Tool input shown in flyco's approval UI.
    #[must_use]
    pub fn input(&self) -> Value {
        if let Some(item) = &self.item {
            return item.clone();
        }
        if let Some(command) = &self.command {
            return Value::String(command.clone());
        }
        Value::Null
    }
}

/// The plan windows one rate-limit snapshot describes.
///
/// `primary` and `secondary` are positions, not names — the app-server does
/// not say which is the shorter — so both go through the same translation
/// and the label comes from the duration each one states.
#[must_use]
pub fn usage_windows(snapshot: RateLimitSnapshot) -> Vec<UsageWindow> {
    [snapshot.primary, snapshot.secondary]
        .into_iter()
        .flatten()
        .map(|window| {
            UsageWindow::new(
                window.window_duration_mins,
                None,
                window.used_percent,
                window.resets_at,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Normalizer, usage_windows};
    use crate::harness::codex::protocol::RateLimitsBody;
    use flyco_core::HarnessEvent;
    use serde_json::json;

    /// The `account/rateLimits/read` answer recorded from `codex-cli
    /// 0.153.4` on 2026-09-09, trimmed to the fields flycod reads.
    #[test]
    fn a_rate_limit_snapshot_becomes_the_windows_it_names() {
        let body: RateLimitsBody = serde_json::from_value(json!({
            "rateLimits": {
                "limitId": "codex",
                "limitName": null,
                "primary": {
                    "usedPercent": 0,
                    "windowDurationMins": 43_200,
                    "resetsAt": 1_791_582_264_i64
                },
                "secondary": null,
                "credits": { "hasCredits": false, "unlimited": false, "balance": "0" },
                "individualLimit": null,
                "spendControlReached": false,
                "planType": "free",
                "rateLimitReachedType": null
            }
        }))
        .expect("the recorded answer decodes");

        let windows = usage_windows(body.rate_limits);
        assert_eq!(windows.len(), 1);
        let window = &windows[0];
        assert_eq!(window.label, "Monthly");
        assert_eq!(window.used_percent, 0);
        assert_eq!(window.window_minutes, Some(43_200));
        assert_eq!(window.resets_at_unix, Some(1_791_582_264));
    }

    #[test]
    fn a_sparse_update_revises_one_window_and_keeps_the_other() {
        let read: RateLimitsBody = serde_json::from_value(json!({
            "rateLimits": {
                "primary": { "usedPercent": 12, "windowDurationMins": 300, "resetsAt": 1_789_002_000_i64 },
                "secondary": { "usedPercent": 40, "windowDurationMins": 10_080, "resetsAt": 1_789_570_800_i64 }
            }
        }))
        .expect("a read decodes");
        // What the app-server actually pushes: the window that moved, and
        // nothing about the one that did not.
        let update: RateLimitsBody = serde_json::from_value(json!({
            "rateLimits": {
                "primary": { "usedPercent": 13, "windowDurationMins": 300, "resetsAt": 1_789_002_000_i64 }
            }
        }))
        .expect("an update decodes");

        let windows = usage_windows(read.rate_limits.merged(update.rate_limits));
        assert_eq!(
            windows
                .iter()
                .map(|window| (window.label.as_str(), window.used_percent))
                .collect::<Vec<_>>(),
            vec![("5-hour", 13), ("Weekly", 40)]
        );
    }

    #[test]
    fn a_completed_context_compaction_is_reported_without_a_turn() {
        let mut normalizer = Normalizer::new();
        assert_eq!(
            normalizer.on_notification(
                super::method::ITEM_COMPLETED,
                &json!({ "item": { "id": "compact-1", "type": "contextCompaction" } }),
            ),
            vec![HarnessEvent::ContextCompacted]
        );
    }
}
