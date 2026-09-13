//! `Idempotency-Key` deduplication for `POST /v1/sessions`.
//!
//! A session create is not naturally idempotent: retrying one whose
//! response was lost provisions a second billed machine. A client that
//! presents a key gets the create claimed under `(user_id, key)` before any
//! session row exists — the claim is the `PRIMARY KEY` insert itself, so
//! two concurrent requests under one key cannot both pass — and the claim
//! is bound to the session the moment one exists, so a replay of the same
//! request reads that session back rather than opening another.
//!
//! A row whose `session_id` is still `NULL` marks a create in flight:
//! another request under the same key is a conflict the caller must
//! reconcile (its session may exist any moment). Rows older than
//! [`TTL_SECONDS`] stop counting — the documented dedup window — and are
//! deleted lazily by the next claim rather than by a sweeper.

use flyco_core::{SessionId, UserId};
use skyzen::sql;
use skyzen_services::Db;

use crate::clock::now_unix;
use crate::error::ApiError;

/// How long a presented key deduplicates, in seconds: one day.
pub const TTL_SECONDS: u64 = 24 * 60 * 60;

/// The most a caller may put in the header.
///
/// Unbounded keys would let a request write an unbounded row; the cap is
/// comfortably past anything a UUID, a ULID, or a request id generator
/// produces.
pub const MAX_KEY_CHARS: usize = 255;

/// What claiming a key found.
#[derive(Debug)]
pub enum Claim {
    /// Nobody else holds this key: the caller performs the create, then
    /// binds the outcome — [`FreshClaim::record`] on success,
    /// [`FreshClaim::release`] on failure.
    Fresh(FreshClaim),
    /// A create under this key already produced this session; the caller
    /// reads it back and answers with it.
    Committed(SessionId),
    /// A create under this key is in flight and has not named its session
    /// yet. The caller refuses with a conflict.
    InFlight,
}

/// A claim this request holds: the row exists and its `session_id` is still
/// `NULL` until [`Self::record`] writes it.
#[derive(Debug)]
pub struct FreshClaim {
    user: UserId,
    key: String,
}

/// Claims `key` for `user`, or reports what the key already names.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn claim(db: &Db, user: UserId, key: &str) -> Result<Claim, ApiError> {
    if key.is_empty() || key.chars().count() > MAX_KEY_CHARS {
        return Err(ApiError::InvalidIdempotencyKey { max: MAX_KEY_CHARS });
    }
    let cutoff = now_unix().saturating_sub(TTL_SECONDS);
    loop {
        let claimed = sql!(
            db,
            "INSERT OR IGNORE INTO idempotency_keys (user_id, key, created_at_unix) \
             VALUES ({user}, {key}, {now_unix()})"
        )
        .execute()
        .await?;
        if claimed.rows_written == 1 {
            return Ok(Claim::Fresh(FreshClaim {
                user,
                key: key.to_owned(),
            }));
        }

        let row: Option<HeldRow> = sql!(
            db,
            "SELECT session_id, created_at_unix FROM idempotency_keys \
             WHERE user_id = {user} AND key = {key}"
        )
        .fetch_optional()
        .await?;
        // The row exists by definition — the `INSERT OR IGNORE` above just
        // collided with it — so an absent row means a concurrent release
        // removed an in-flight claim whose create then failed. That is a
        // key nobody holds: retry the claim.
        let Some(held) = row else { continue };
        match held.session_id {
            Some(session) => return Ok(Claim::Committed(session)),
            // An in-flight claim past the window is one whose create died
            // between the claim and the record — the request was told to
            // reconcile, nobody ever could — so the key is free again.
            None if held.created_at_unix < cutoff => {
                sql!(
                    db,
                    "DELETE FROM idempotency_keys \
                     WHERE user_id = {user} AND key = {key} AND created_at_unix < {cutoff}"
                )
                .execute()
                .await?;
                continue;
            }
            None => return Ok(Claim::InFlight),
        }
    }
}

impl FreshClaim {
    /// Binds the claim to the session its create produced.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError`] if the database fails.
    pub async fn record(self, db: &Db, session: SessionId) -> Result<(), ApiError> {
        sql!(
            db,
            "UPDATE idempotency_keys SET session_id = {session} \
             WHERE user_id = {self.user} AND key = {self.key}"
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Gives the key back because the create it guarded failed: a retry
    /// under the same key must not be locked out by a request that never
    /// produced a session.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError`] if the database fails.
    pub async fn release(self, db: &Db) -> Result<(), ApiError> {
        sql!(
            db,
            "DELETE FROM idempotency_keys \
             WHERE user_id = {self.user} AND key = {self.key} AND session_id IS NULL"
        )
        .execute()
        .await?;
        Ok(())
    }
}

#[derive(Debug, skyzen::FromRow)]
struct HeldRow {
    session_id: Option<SessionId>,
    created_at_unix: u64,
}
