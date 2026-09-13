//! Auth DTOs served by the control plane.
//!
//! Flyco has exactly one identity provider (GitHub) and one credential
//! channel: `Authorization: Bearer`. Three kinds of token travel it — an
//! `fs_` browser session token, an `fk_` API key, and an
//! [`fd_`](DAEMON_TOKEN_PREFIX) daemon token — and these types describe
//! what crosses the wire around them. A credential itself never appears in
//! a response body except for the one moment it is minted
//! ([`CreatedApiKey::token`], [`DaemonToken::token`]).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::id::{ApiKeyId, CliSessionId, SessionId, UserId};

/// Marks a session daemon's credential.
///
/// A daemon token authenticates exactly one session's `flycod` against the
/// daemon-scoped routes of *that* session. It lives in `flyco_core` rather
/// than in the control plane because the daemon checks its own
/// configuration against this prefix at startup: a token of the wrong kind
/// is a provisioning bug, and the daemon says so before it dials out.
pub const DAEMON_TOKEN_PREFIX: &str = "fd_";

/// A freshly minted daemon token.
///
/// Response of `POST /v1/sessions/{id}/daemon-token`. Minting again
/// replaces the previous token, so a session has at most one live daemon
/// credential and re-pairing revokes the old one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DaemonToken {
    /// The session this token authenticates a daemon for.
    pub session: SessionId,
    /// The plaintext token, returned exactly once.
    pub token: String,
}

/// Fewest concurrent sessions a user may be limited to.
pub const SESSION_CAP_MIN: u32 = 1;

/// Most concurrent sessions a user may be allowed.
pub const SESSION_CAP_MAX: u32 = 100;

/// Concurrent-session cap a new account starts with.
pub const SESSION_CAP_DEFAULT: u32 = 5;

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
    /// How many sessions this user may hold at once, counting everything
    /// that is not archived.
    pub session_cap: u32,
}

/// Request body of `PATCH /v1/me`.
///
/// Every field is optional; an omitted field is left as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct UpdateMe {
    /// New concurrent-session cap, within
    /// [`SESSION_CAP_MIN`]..=[`SESSION_CAP_MAX`].
    pub session_cap: Option<u32>,
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

/// Request body of `POST /v1/cli-sessions`.
///
/// Opens a `flyco login` attempt: the CLI prints the returned
/// `authorize_url`, the user approves it in a browser already signed in to
/// flyco, and the CLI polls until the key is minted.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
pub struct CreateCliSession {
    /// The machine the CLI runs on — becomes part of the issued key's
    /// label, so the key list reads `flyco-cli on <hostname>`. Omitted on
    /// machines that cannot name themselves.
    #[serde(default)]
    pub hostname: Option<String>,
}

/// Response of `POST /v1/cli-sessions`: a pending sign-in attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CliSession {
    /// Identifier the poll and the approval page both name.
    pub id: CliSessionId,
    /// Shared secret the poll presents as `?s=`. Proves the poller is the
    /// same CLI that opened the attempt — it is not the key, only the
    /// right to collect the key once one exists.
    pub poll_token: String,
    /// Where the user approves: the PWA's `/cli/authorize` page with this
    /// attempt's id. Opened locally when the CLI can open a browser,
    /// printed otherwise.
    pub authorize_url: String,
    /// Seconds since the Unix epoch when the attempt stops answering.
    pub expires_at_unix: u64,
}

/// Response of `GET /v1/cli-sessions/{id}?s=…` once the user approved.
///
/// Returned exactly once: the read that answers `200` consumes the record,
/// so a replayed poll finds `410` rather than a second copy of the key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CliSessionKey {
    /// Identifier used to revoke the key later — `flyco logout` passes it
    /// to `DELETE /v1/api-keys/{id}`.
    pub key_id: ApiKeyId,
    /// The plaintext `fk_` API key.
    pub key: String,
}
