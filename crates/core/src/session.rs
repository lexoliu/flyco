//! Session lifecycle.
//!
//! The state machine is enforced here so the control plane can never
//! record an impossible transition — an invalid one is an error at the
//! point of the bug, not a corrupt row discovered later.

use serde::{Deserialize, Serialize};

use crate::budget::BudgetView;
use crate::harness::{HarnessKind, UsageReport};
use crate::id::SessionId;
use crate::money::Usd;
use crate::repo::RepoSlug;

/// Lifecycle state of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "sql", derive(skyzen::Column))]
pub enum SessionState {
    /// A machine is being provisioned; code is not yet fetched.
    Provisioning,
    /// The daemon is connected and the harness can run turns.
    Active,
    /// Paused by budget exhaustion or by the user; machine kept.
    Paused,
    /// Compute was reclaimed (spot eviction); disk kept, resumable.
    Interrupted,
    /// Archived: turns kept, execution environment released. Terminal.
    Archived,
}

/// An invalid session state transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid session transition: {from:?} -> {to:?}")]
pub struct SessionTransitionError {
    /// State the session was in.
    pub from: SessionState,
    /// State the caller tried to move it to.
    pub to: SessionState,
}

impl SessionState {
    /// Validates and performs a transition.
    ///
    /// # Errors
    ///
    /// Returns [`SessionTransitionError`] when the move is not part of the
    /// lifecycle graph.
    pub const fn transition(self, to: Self) -> Result<Self, SessionTransitionError> {
        let allowed = matches!(
            (self, to),
            (
                Self::Provisioning | Self::Paused,
                Self::Active | Self::Archived
            ) | (
                Self::Active,
                Self::Paused | Self::Interrupted | Self::Archived
            ) | (Self::Interrupted, Self::Provisioning | Self::Archived)
        );
        if allowed {
            Ok(to)
        } else {
            Err(SessionTransitionError { from: self, to })
        }
    }

    /// Whether the session still holds an execution environment.
    #[must_use]
    pub const fn holds_environment(self) -> bool {
        !matches!(self, Self::Archived)
    }
}

/// Request body of `POST /v1/sessions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CreateSession {
    /// Which coding harness drives the session.
    pub harness: HarnessKind,
    /// Repository to work in, `owner/name`. Untyped here because it is
    /// untrusted input; the control plane parses it into a
    /// [`RepoSlug`](crate::repo::RepoSlug) and rejects anything else.
    pub repo: String,
    /// Spending limit for the whole session.
    pub budget_limit: Usd,
    /// Whether to use interruptible spot capacity. Spot is the default
    /// because it is the cheaper option and flyco handles eviction.
    #[serde(default = "default_spot")]
    pub spot: bool,
}

const fn default_spot() -> bool {
    true
}

/// A session in a list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SessionSummary {
    /// Identifier.
    pub id: SessionId,
    /// Which coding harness drives it.
    pub harness: HarnessKind,
    /// Repository it works in.
    pub repo: RepoSlug,
    /// Where it is in its lifecycle.
    pub state: SessionState,
    /// When it was created, seconds since the Unix epoch.
    pub created_at_unix: u64,
    /// Last time anything happened on it, seconds since the Unix epoch.
    pub last_active_unix: u64,
}

/// A single session, with its budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SessionDetail {
    /// Everything a list entry carries.
    #[serde(flatten)]
    pub summary: SessionSummary,
    /// Budget accounting as of this request.
    pub budget: BudgetView,
}

/// Request body of `POST /v1/sessions/{id}/messages`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SendMessage {
    /// What to say to the agent. A leading `!` is the terminal escape the
    /// UI documents; the control plane forwards the text either way and the
    /// daemon decides.
    pub text: String,
}

/// One turn of a session, as the history list renders it.
///
/// Folded out of the event stream the session's room records rather than
/// read from a table of its own: that stream is what survives a machine and
/// what a browser replays, so a turn list built from anything else would
/// disagree with the conversation shown beside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TurnSummary {
    /// Harness-native turn identifier, which is what the transcript keys on.
    pub turn_id: String,
    /// When the turn started, seconds since the Unix epoch.
    pub started_at_unix: u64,
    /// When it finished, if it has.
    pub completed_at_unix: Option<u64>,
    /// The opening of the user message that began the turn, for the list.
    pub prompt_excerpt: String,
    /// Token accounting after the turn, when it completed.
    pub usage: Option<UsageReport>,
}

/// One page of `GET /v1/sessions/{id}/turns`.
///
/// The cursor is opaque: it encodes a position in the session's recorded
/// event stream, and a client that stores it must hand it back unread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TurnPage {
    /// The turns, oldest first.
    pub turns: Vec<TurnSummary>,
    /// Cursor to pass as `cursor` for the next page, or `None` at the end.
    pub next_cursor: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spot_defaults_to_on_when_the_client_omits_it() {
        let request: CreateSession = serde_json::from_str(
            r#"{"harness":"claude_code","repo":"lexoliu/flyco","budget_limit":10000000}"#,
        )
        .expect("deserialize");
        assert!(request.spot);
    }

    #[test]
    fn states_use_the_tokens_the_schema_stores() {
        for (state, token) in [
            (SessionState::Provisioning, "provisioning"),
            (SessionState::Active, "active"),
            (SessionState::Paused, "paused"),
            (SessionState::Interrupted, "interrupted"),
            (SessionState::Archived, "archived"),
        ] {
            assert_eq!(
                serde_json::to_value(state).expect("serialize"),
                serde_json::Value::String(token.to_owned())
            );
        }
    }

    #[test]
    fn happy_path_lifecycle() {
        let s = SessionState::Provisioning;
        let s = s.transition(SessionState::Active).expect("provisioned");
        let s = s.transition(SessionState::Interrupted).expect("evicted");
        let s = s.transition(SessionState::Provisioning).expect("resuming");
        let s = s.transition(SessionState::Active).expect("resumed");
        let s = s.transition(SessionState::Paused).expect("budget pause");
        let s = s.transition(SessionState::Archived).expect("archive");
        assert!(!s.holds_environment());
    }

    #[test]
    fn archived_is_terminal() {
        for to in [
            SessionState::Provisioning,
            SessionState::Active,
            SessionState::Paused,
            SessionState::Interrupted,
        ] {
            assert!(SessionState::Archived.transition(to).is_err());
        }
    }

    #[test]
    fn cannot_skip_provisioning_when_resuming_interrupted() {
        assert!(
            SessionState::Interrupted
                .transition(SessionState::Active)
                .is_err()
        );
    }
}
