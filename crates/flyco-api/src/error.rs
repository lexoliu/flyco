//! The single error type the auth handlers return.
//!
//! Skyzen maps an [`HttpError`](skyzen::HttpError) status straight onto the
//! response and hides the message for 5xx, so each variant only has to name
//! the status that is actually correct. Nothing here degrades into a
//! best-effort success.

use skyzen::StatusCode;
use skyzen_services::{DbError, KvError};

use crate::crypto::CryptoError;
use crate::github::GithubError;

/// Every way an auth request can fail.
#[skyzen::error(status = StatusCode::INTERNAL_SERVER_ERROR)]
pub enum ApiError {
    /// The `state` echoed back by GitHub is unknown, already consumed, or
    /// past its ten-minute lifetime.
    #[error("the OAuth `state` parameter is unknown or expired", status = StatusCode::BAD_REQUEST)]
    UnknownOauthState,

    /// The request carried no usable credential.
    #[error("authentication required", status = StatusCode::UNAUTHORIZED)]
    Unauthenticated,

    /// The caller asked to revoke a key that is not theirs, or does not exist.
    #[error("api key not found", status = StatusCode::NOT_FOUND)]
    ApiKeyNotFound,

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
