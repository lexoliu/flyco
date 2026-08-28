//! Auth DTOs served by the control plane.
//!
//! Flyco has exactly one identity provider (GitHub) and two credential
//! shapes: an opaque browser session cookie, and a bearer API key for the
//! REST API. These types describe what crosses the wire in both cases; the
//! credentials themselves never appear in a response body except for the one
//! moment a key is minted ([`CreatedApiKey::token`]).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::id::{ApiKeyId, UserId};

/// The authenticated caller behind a request.
///
/// Produced by the control plane's authenticator from either credential
/// shape, and returned verbatim by `GET /v1/me`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CurrentUser {
    /// The flyco user this request acts as.
    pub id: UserId,
    /// The caller's GitHub login, cached at sign-in.
    pub login: String,
}

/// Where the browser must be sent to begin a GitHub OAuth code flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AuthorizeUrl {
    /// Fully-formed `https://github.com/login/oauth/authorize` URL, including
    /// the single-use `state` this control plane will accept back.
    pub authorize_url: String,
}

/// Request body of `POST /v1/api-keys`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CreateApiKey {
    /// Human-readable name, shown in the key list so a key can be recognised
    /// before it is revoked.
    pub label: String,
}

/// A freshly minted API key.
///
/// This is the only representation that ever carries the plaintext
/// [`token`](Self::token): the control plane stores nothing but its hash, so
/// a key that is not saved here is gone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CreatedApiKey {
    /// Identifier used to revoke the key later.
    pub id: ApiKeyId,
    /// Label supplied at creation.
    pub label: String,
    /// The plaintext key, returned exactly once.
    pub token: String,
    /// Creation time, seconds since the Unix epoch.
    pub created_at_unix: u64,
}

/// One row of `GET /v1/api-keys`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ApiKeySummary {
    /// Identifier used to revoke the key.
    pub id: ApiKeyId,
    /// Label supplied at creation.
    pub label: String,
    /// Creation time, seconds since the Unix epoch.
    pub created_at_unix: u64,
    /// Last time the key authenticated a request, if it ever has.
    pub last_used_unix: Option<u64>,
}
