//! The single error type the auth handlers return.
//!
//! Every variant knows the RFC 9457 document it renders as, so a client sees
//! one error shape across the whole API. Server-side failures deliberately
//! describe themselves only in the log: the response says the status and
//! nothing that would leak internals.

use flyco_core::{ApprovalState, Problem, SessionState};
use skyzen::{Response, StatusCode};
use skyzen_services::{DbError, KvError, StorageError};

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

    /// The memory node does not exist, or belongs to somebody else.
    #[error("memory node not found", status = StatusCode::NOT_FOUND)]
    MemoryNodeNotFound,

    /// This deployment has no VAPID key pair, so it cannot send push.
    #[error(
        "this flyco deployment is not configured for web push",
        status = StatusCode::NOT_IMPLEMENTED
    )]
    PushUnconfigured,

    /// The push subscription does not exist, or belongs to somebody else.
    #[error("push subscription not found", status = StatusCode::NOT_FOUND)]
    PushSubscriptionNotFound,

    /// The browser sent a subscription flyco cannot use.
    #[error("this push subscription is unusable: {0}", status = StatusCode::UNPROCESSABLE_ENTITY)]
    InvalidPushSubscription(&'static str),

    /// The provider account does not exist, or belongs to somebody else.
    #[error("provider account not found", status = StatusCode::NOT_FOUND)]
    ProviderAccountNotFound,

    /// The harness account does not exist, or belongs to somebody else.
    #[error("harness account not found", status = StatusCode::NOT_FOUND)]
    HarnessAccountNotFound,

    /// The machine exists but the provider has not named it yet.
    ///
    /// It is still being created, so there is nothing to act on. Distinct
    /// from a missing machine: retrying later succeeds.
    #[error(
        "this session's machine is still being created",
        status = StatusCode::CONFLICT
    )]
    MachineNotReady,

    /// The provider refused or could not complete the operation.
    #[error("the provider could not complete this: {0}", status = StatusCode::BAD_GATEWAY)]
    Provisioning(String),

    /// The session has not been given a machine yet.
    ///
    /// Distinct from a destroyed one: nothing was ever provisioned.
    #[error("this session has no machine", status = StatusCode::NOT_FOUND)]
    MachineNotFound,

    /// Unlinking would strand machines still running on the account.
    #[error(
        "{sessions} session(s) still run on this account; archive them before unlinking",
        status = StatusCode::CONFLICT
    )]
    ProviderInUse {
        /// How many sessions still hold a machine there.
        sessions: u64,
    },

    /// Flyco has no driver for this provider yet.
    ///
    /// Linking credentials flyco cannot act on would leave a user holding an
    /// account that silently fails at the first provision, so the refusal
    /// happens where the mistake is made.
    #[error(
        "flyco cannot provision on {provider} yet",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    ProviderUnsupported {
        /// The provider named by the submitted credentials.
        provider: &'static str,
    },

    /// The provider itself rejected the credentials.
    #[error(
        "the provider rejected these credentials: {reason}",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    ProviderRejectedCredentials {
        /// What the provider said, so the user can fix it.
        reason: String,
    },

    /// No daemon has reported this session's working tree yet.
    ///
    /// Distinct from "the tree is clean": nothing has looked. Answering
    /// `dirty: false` would be inventing a fact the control plane does not
    /// have, and archiving on the strength of it is exactly the mistake the
    /// route exists to prevent.
    #[error(
        "no daemon has reported this session's working tree yet",
        status = StatusCode::NOT_FOUND
    )]
    RepoStatusUnknown,

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

    /// The session is not running, so it cannot be driven.
    #[error(
        "this session is {state:?}, and only an active session can be driven",
        status = StatusCode::CONFLICT
    )]
    SessionNotActive {
        /// The state the session is actually in.
        state: SessionState,
    },

    /// The submitted repository is not `owner/name`.
    #[error(
        "`{0}` is not a GitHub repository in `owner/name` form",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    InvalidRepo(String),

    /// An environment variable name is not one a shell can export.
    #[error(
        "`{0}` is not an environment variable name: use letters, digits and `_`, not starting with a digit",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    InvalidEnvKey(String),

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

    /// A message with nothing in it was sent to an agent.
    #[error(
        "a message to an agent cannot be empty",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    EmptyMessage,

    /// A pagination cursor was not one this API issued.
    #[error("`{0}` is not a page cursor from this API", status = StatusCode::BAD_REQUEST)]
    InvalidCursor(String),

    /// A path parameter that must be a UUID was not one.
    #[error("`{0}` is not a valid identifier", status = StatusCode::BAD_REQUEST)]
    MalformedId(String),

    /// A transcript stream key is not a single safe path segment.
    #[error(
        "`{0}` is not a transcript stream key: use letters, digits, `.`, `_`, and `-` only",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    InvalidStreamKey(String),

    /// A transcript batch sequence number is wider than the key format.
    #[error(
        "batch sequence {seq} is beyond the largest a transcript key can address",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    BatchSeqOutOfRange {
        /// The sequence number that was submitted.
        seq: u64,
    },

    /// A transcript batch with this sequence number is already stored.
    ///
    /// Batches are immutable: overwriting one would silently reorder the
    /// transcript, so a repeat is a conflict rather than an update.
    #[error(
        "transcript batch {seq} is already stored and batches are immutable",
        status = StatusCode::CONFLICT
    )]
    BatchAlreadyStored {
        /// The sequence number that was submitted again.
        seq: u64,
    },

    /// The presented daemon token does not pair with this session.
    #[error("the presented credential is not this session's daemon token", status = StatusCode::UNAUTHORIZED)]
    InvalidDaemonCredential,

    /// The live relay is not available on this build of the control plane.
    #[error(
        "this control plane does not host session relays: {0}",
        status = StatusCode::NOT_IMPLEMENTED
    )]
    RelayUnavailable(&'static str),

    /// The session's Durable Object could not be reached, or refused.
    #[error("the session room failed: {0}", status = StatusCode::BAD_GATEWAY)]
    Room(String),

    /// Object storage failed.
    #[error("object storage failed: {0}")]
    Storage(#[from] StorageError),

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
            Self::MemoryNodeNotFound => "memory-node-not-found",
            Self::PushUnconfigured => "push-unconfigured",
            Self::PushSubscriptionNotFound => "push-subscription-not-found",
            Self::InvalidPushSubscription(_) => "invalid-push-subscription",
            Self::ProviderAccountNotFound => "provider-account-not-found",
            Self::HarnessAccountNotFound => "harness-account-not-found",
            Self::MachineNotFound => "machine-not-found",
            Self::MachineNotReady => "machine-not-ready",
            Self::Provisioning(_) => "provisioning-failed",
            Self::ProviderInUse { .. } => "provider-in-use",
            Self::ProviderUnsupported { .. } => "provider-unsupported",
            Self::ProviderRejectedCredentials { .. } => "provider-rejected-credentials",
            Self::RepoStatusUnknown => "repo-status-unknown",
            Self::SessionNotActive { .. } => "session-not-active",
            Self::SessionCapReached { .. } => "session-cap-reached",
            Self::InvalidTransition { .. } => "invalid-session-transition",
            Self::ApprovalAlreadyDecided { .. } => "approval-already-decided",
            Self::InvalidRepo(_) => "invalid-repo",
            Self::InvalidEnvKey(_) => "invalid-env-key",
            Self::InvalidBudget => "invalid-budget",
            Self::InvalidSessionCap { .. } => "invalid-session-cap",
            Self::EmptyMessage => "empty-message",
            Self::InvalidCursor(_) => "invalid-cursor",
            Self::MalformedId(_) => "malformed-id",
            Self::InvalidStreamKey(_) => "invalid-stream-key",
            Self::BatchSeqOutOfRange { .. } => "batch-seq-out-of-range",
            Self::BatchAlreadyStored { .. } => "batch-already-stored",
            Self::InvalidDaemonCredential => "invalid-daemon-credential",
            Self::RelayUnavailable(_) => "relay-unavailable",
            Self::Room(_) => "session-room-unavailable",
            Self::Github(_) => "github-unavailable",
            Self::CorruptRecord(_)
            | Self::ServiceMissing(_)
            | Self::Kv(_)
            | Self::Db(_)
            | Self::Storage(_)
            | Self::Crypto(_) => "internal",
        }
    }

    /// The RFC 6750 challenge this failure must carry, if any.
    pub(crate) const fn challenge(&self) -> Option<Challenge> {
        match self {
            Self::MissingCredential => Some(Challenge::Bearer),
            Self::InvalidCredential | Self::InvalidDaemonCredential => {
                Some(Challenge::InvalidToken)
            }
            _ => None,
        }
    }

    /// Whether this failure's explanation must stay in the log.
    ///
    /// Anything that broke on flyco's side describes itself only to the
    /// operator. The one exception is a deliberate, documented refusal —
    /// [`RelayUnavailable`](Self::RelayUnavailable) — where the whole point
    /// of the status code is to tell the caller which capability this build
    /// does not have.
    fn is_opaque(&self) -> bool {
        !matches!(self, Self::RelayUnavailable(_))
            && skyzen::HttpError::status(self).is_server_error()
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

        let detail = if self.is_opaque() {
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
