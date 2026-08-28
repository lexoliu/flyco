//! The single error type the auth handlers return.
//!
//! Every variant knows the RFC 9457 document it renders as, so a client sees
//! one error shape across the whole API. Server-side failures deliberately
//! describe themselves only in the log: the response says the status and
//! nothing that would leak internals.

use flyco_core::{ApprovalState, Problem, SessionState};
use skyzen::{Response, StatusCode};
use skyzen_services::{DbError, KvError};

use crate::crypto::CryptoError;
use crate::github::GithubError;
use crate::problem::{self, Challenge};

/// Detail returned for any failure that is flyco's fault rather than the
/// caller's.
const SERVER_DETAIL: &str = "The control plane failed to handle this request.";

/// Every way an auth request can fail.
#[skyzen::error(status = StatusCode::INTERNAL_SERVER_ERROR)]
pub enum ApiError {
    /// The request carried no `Authorization` header.
    #[error("no credential was presented", status = StatusCode::UNAUTHORIZED)]
    MissingCredential,

    /// The presented bearer token is unknown, expired, or not a flyco token.
    #[error("the presented credential is not valid", status = StatusCode::UNAUTHORIZED)]
    InvalidCredential,

    /// The `state` echoed back by GitHub is unknown, already consumed, or
    /// past its ten-minute lifetime.
    #[error("the OAuth `state` parameter is unknown or expired", status = StatusCode::BAD_REQUEST)]
    UnknownOauthState,

    /// The caller asked to revoke a key that is not theirs, or does not exist.
    #[error("api key not found", status = StatusCode::NOT_FOUND)]
    ApiKeyNotFound,

    /// The session does not exist, or belongs to somebody else. The two are
    /// deliberately indistinguishable.
    #[error("session not found", status = StatusCode::NOT_FOUND)]
    SessionNotFound,

    /// The approval does not exist, or belongs to somebody else.
    #[error("approval not found", status = StatusCode::NOT_FOUND)]
    ApprovalNotFound,

    /// The caller already holds as many live sessions as they may.
    #[error(
        "you already hold {cap} sessions, which is your limit; archive one first",
        status = StatusCode::CONFLICT
    )]
    SessionCapReached {
        /// The cap that was reached.
        cap: u32,
    },

    /// The requested lifecycle move is not part of the session state machine.
    #[error(
        "a session cannot move from {from:?} to {to:?}",
        status = StatusCode::CONFLICT
    )]
    InvalidTransition {
        /// State the session is in.
        from: SessionState,
        /// State the caller asked for.
        to: SessionState,
    },

    /// The approval already carries a decision, and a decision is final.
    #[error(
        "this approval was already decided as {state:?}",
        status = StatusCode::CONFLICT
    )]
    ApprovalAlreadyDecided {
        /// The decision that stands.
        state: ApprovalState,
    },

    /// The submitted repository is not `owner/name`.
    #[error(
        "`{0}` is not a GitHub repository in `owner/name` form",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    InvalidRepo(String),

    /// The submitted budget limit cannot fund anything.
    #[error(
        "a session budget must be greater than zero",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    InvalidBudget,

    /// The submitted session cap is outside the allowed range.
    #[error(
        "a session cap must be between {min} and {max}",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    InvalidSessionCap {
        /// Smallest cap the control plane accepts.
        min: u32,
        /// Largest cap the control plane accepts.
        max: u32,
    },

    /// A path parameter that must be a UUID was not one.
    #[error("`{0}` is not a valid identifier", status = StatusCode::BAD_REQUEST)]
    MalformedId(String),

    /// A stored row does not match the schema the control plane expects.
    #[error("stored record is inconsistent: {0}")]
    CorruptRecord(&'static str),

    /// A portable service the handler needs was never injected — a wiring
    /// bug in `Skyzen.toml`, not something a caller can provoke.
    #[error("required service `{0}` is not configured")]
    ServiceMissing(&'static str),

    /// GitHub could not be reached, or refused the request.
    #[error("GitHub call failed: {0}", status = StatusCode::BAD_GATEWAY)]
    Github(#[from] GithubError),

    /// The key-value store failed.
    #[error("key-value store failed: {0}")]
    Kv(#[from] KvError),

    /// The database failed.
    #[error("database failed: {0}")]
    Db(#[from] DbError),

    /// A cryptographic primitive failed.
    #[error("cryptography failed: {0}")]
    Crypto(#[from] CryptoError),
}

impl ApiError {
    /// The slug this failure is documented under, below
    /// [`TYPE_BASE`](flyco_core::problem::TYPE_BASE).
    const fn slug(&self) -> &'static str {
        match self {
            Self::MissingCredential => "missing-credential",
            Self::InvalidCredential => "invalid-credential",
            Self::UnknownOauthState => "unknown-oauth-state",
            Self::ApiKeyNotFound => "api-key-not-found",
            Self::SessionNotFound => "session-not-found",
            Self::ApprovalNotFound => "approval-not-found",
            Self::SessionCapReached { .. } => "session-cap-reached",
            Self::InvalidTransition { .. } => "invalid-session-transition",
            Self::ApprovalAlreadyDecided { .. } => "approval-already-decided",
            Self::InvalidRepo(_) => "invalid-repo",
            Self::InvalidBudget => "invalid-budget",
            Self::InvalidSessionCap { .. } => "invalid-session-cap",
            Self::MalformedId(_) => "malformed-id",
            Self::Github(_) => "github-unavailable",
            Self::CorruptRecord(_)
            | Self::ServiceMissing(_)
            | Self::Kv(_)
            | Self::Db(_)
            | Self::Crypto(_) => "internal",
        }
    }

    /// The RFC 6750 challenge this failure must carry, if any.
    pub(crate) const fn challenge(&self) -> Option<Challenge> {
        match self {
            Self::MissingCredential => Some(Challenge::Bearer),
            Self::InvalidCredential => Some(Challenge::InvalidToken),
            _ => None,
        }
    }

    /// The RFC 9457 document describing this failure.
    ///
    /// Server-side failures are logged in full and reported as a bare status.
    #[must_use]
    pub fn problem(&self) -> Problem {
        let status = skyzen::HttpError::status(self);
        let title = status.canonical_reason().unwrap_or("Error");

        if status.is_server_error() {
            tracing::error!(error = %self, "request failed");
        } else {
            tracing::debug!(error = %self, "rejected a request");
        }

        let detail = if status.is_server_error() {
            SERVER_DETAIL.to_owned()
        } else {
            self.to_string()
        };

        Problem::of_type(self.slug(), status.as_u16(), title, detail)
    }

    /// Renders this failure as a complete response.
    #[must_use]
    pub fn into_response(self) -> Response {
        problem::response(&self.problem(), self.challenge())
    }
}

#[cfg(test)]
mod tests {
    use super::ApiError;

    #[test]
    fn a_client_error_explains_itself() {
        let problem = ApiError::MalformedId("nope".to_owned()).problem();

        assert_eq!(problem.status, 400);
        assert_eq!(problem.kind, "https://flyco.dev/problems/malformed-id");
        assert_eq!(problem.title, "Bad Request");
        assert!(problem.detail.contains("nope"));
    }

    #[test]
    fn a_server_error_says_nothing_about_its_internals() {
        let problem = ApiError::CorruptRecord("users.id is not a UUID").problem();

        assert_eq!(problem.status, 500);
        assert_eq!(problem.kind, "https://flyco.dev/problems/internal");
        assert!(!problem.detail.contains("users.id"));
    }

    #[test]
    fn only_the_two_unauthorized_variants_carry_a_challenge() {
        assert!(ApiError::MissingCredential.challenge().is_some());
        assert!(ApiError::InvalidCredential.challenge().is_some());
        assert!(ApiError::ApiKeyNotFound.challenge().is_none());
    }
}
