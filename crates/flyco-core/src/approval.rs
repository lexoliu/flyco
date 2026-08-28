//! Approvals: the user decisions flyco enforces itself.
//!
//! An approval is raised by the daemon, shown in flyco's own UI, and
//! decided exactly once. The "exactly once" rule lives here rather than in
//! a handler, so no caller can record a second decision over the first.

use serde::{Deserialize, Serialize};

use crate::id::{ApprovalId, SessionId};
use crate::wire::{ApprovalDecision, ApprovalPayload};

/// Where an approval is in its short life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    /// Raised, waiting for the user.
    Pending,
    /// The user allowed the action.
    Approved,
    /// The user refused the action.
    Denied,
}

/// An approval was decided twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("this approval was already {state:?}")]
pub struct AlreadyDecided {
    /// The decision that already stands.
    pub state: ApprovalState,
}

impl ApprovalState {
    /// Applies the user's decision.
    ///
    /// # Errors
    ///
    /// Returns [`AlreadyDecided`] if this approval has been decided before —
    /// a decision is final, and a second one is a conflict rather than an
    /// overwrite.
    pub const fn decide(self, decision: ApprovalDecision) -> Result<Self, AlreadyDecided> {
        match self {
            Self::Pending => Ok(Self::from_decision(decision)),
            decided => Err(AlreadyDecided { state: decided }),
        }
    }

    /// The state a decision puts an approval into.
    #[must_use]
    pub const fn from_decision(decision: ApprovalDecision) -> Self {
        match decision {
            ApprovalDecision::Approved => Self::Approved,
            ApprovalDecision::Denied => Self::Denied,
        }
    }

    /// Whether the user still has to act on this approval.
    #[must_use]
    pub const fn is_pending(self) -> bool {
        matches!(self, Self::Pending)
    }
}

/// An approval as the API serves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ApprovalView {
    /// Identifier used to decide it.
    pub id: ApprovalId,
    /// Session that raised it.
    pub session: SessionId,
    /// What is being approved.
    pub payload: ApprovalPayload,
    /// Whether it is still waiting, and what the user decided if not.
    pub state: ApprovalState,
    /// When it was raised, seconds since the Unix epoch.
    pub created_at_unix: u64,
}

/// Request body of `POST /v1/approvals/{id}/decision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DecideApproval {
    /// What the user decided.
    pub decision: ApprovalDecision,
}

#[cfg(test)]
mod tests {
    use super::{AlreadyDecided, ApprovalState};
    use crate::wire::ApprovalDecision;

    #[test]
    fn a_pending_approval_takes_the_users_decision() {
        assert_eq!(
            ApprovalState::Pending.decide(ApprovalDecision::Approved),
            Ok(ApprovalState::Approved)
        );
        assert_eq!(
            ApprovalState::Pending.decide(ApprovalDecision::Denied),
            Ok(ApprovalState::Denied)
        );
    }

    #[test]
    fn a_decision_is_final() {
        for settled in [ApprovalState::Approved, ApprovalState::Denied] {
            assert_eq!(
                settled.decide(ApprovalDecision::Approved),
                Err(AlreadyDecided { state: settled })
            );
        }
    }

    #[test]
    fn states_use_the_tokens_the_schema_stores() {
        for (state, token) in [
            (ApprovalState::Pending, "pending"),
            (ApprovalState::Approved, "approved"),
            (ApprovalState::Denied, "denied"),
        ] {
            assert_eq!(
                serde_json::to_value(state).expect("serialize"),
                serde_json::Value::String(token.to_owned())
            );
        }
    }
}
