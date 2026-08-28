//! Session lifecycle.
//!
//! The state machine is enforced here so the control plane can never
//! record an impossible transition — an invalid one is an error at the
//! point of the bug, not a corrupt row discovered later.

use serde::{Deserialize, Serialize};

/// Lifecycle state of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
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

#[cfg(test)]
mod tests {
    use super::*;

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
