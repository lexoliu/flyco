//! `flyco handoff` session imports.
//!
//! A handoff is three durable facts around one row: the provenance the
//! sender declared at create ([`flyco_core::LocalHandoff`]), the two
//! payloads in object storage (a `git diff --binary` patch and the
//! harness-native transcript), and the [`flyco_core::HandoffManifest`]
//! `complete` verified them against. The patch rides the existing
//! `workdirs/` object a resume already replays; the transcript gets its
//! own `handoffs/` prefix because nothing else reads it — the daemon
//! writes it to disk for the agent, not into a stream.
//!
//! `completed_at_unix IS NULL` is the provisioning gate: the session's
//! machine is reserved at create but no job is queued until `complete`
//! verifies the uploads, so a daemon can never clone before the patch it
//! is supposed to apply exists.

use sha2::{Digest as _, Sha256};
use skyzen::sql;
use skyzen_services::{Db, Storage};

use crate::clock::now_unix;
use crate::error::ApiError;
use crate::sessions::IdleSession;
use flyco_core::{SessionState, UserId};

/// Prefix the handoff transcript lives under.
const ROOT: &str = "handoffs";

/// Media type of a stored transcript: harness-native session history
/// (JSONL for Claude and Codex, a single JSON document for Devin).
pub const CONTENT_TYPE: &str = "application/octet-stream";

/// How long a pending handoff may sit before the sweep fails it.
///
/// The upload is a client on the user's own network, so the clock is far
/// more generous than [`flyco_core::PROVISION_DEADLINE_SECS`] — an hour
/// covers a large patch on a slow uplink, and a handoff older than that
/// is one whose sender went away.
pub const HANDOFF_DEADLINE_SECS: u64 = 60 * 60;

fn transcript_key(session: flyco_core::SessionId) -> String {
    format!("{ROOT}/{session}/transcript")
}

/// One `handoffs` row.
#[derive(Debug, skyzen::FromRow)]
pub struct HandoffRow {
    /// The cloud session the handoff feeds.
    pub session_id: flyco_core::SessionId,
    /// Which harness the local session ran under.
    pub source_harness: flyco_core::HarnessKind,
    /// The harness-native session id the sender handed off.
    pub harness_session_id: String,
    /// The commit the cloud checkout rewinds to before the patch applies.
    pub base_commit: String,
    /// The sender's local workdir, for the prompt's path map.
    pub local_workdir: String,
    /// SHA-256 and size `complete` recorded for the patch object.
    pub patch_sha256: Option<String>,
    /// See [`Self::patch_sha256`].
    pub patch_bytes: Option<i64>,
    /// SHA-256 and size `complete` recorded for the transcript object.
    pub transcript_sha256: Option<String>,
    /// See [`Self::transcript_sha256`].
    pub transcript_bytes: Option<i64>,
    /// When the row was created.
    pub created_at_unix: u64,
    /// When `complete` verified the uploads; `NULL` while pending.
    pub completed_at_unix: Option<u64>,
}

/// Records the provenance a `CreateSession.source` declared, pending.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn create_pending(
    db: &Db,
    session: flyco_core::SessionId,
    handoff: &flyco_core::LocalHandoff,
) -> Result<(), ApiError> {
    let session_id = session;
    let source_harness = handoff.harness;
    let harness_session_id = handoff.session_id.as_str();
    let base_commit = handoff.base_commit.as_str();
    let local_workdir = handoff.local_workdir.as_str();
    let now = now_unix();
    sql!(
        db,
        "INSERT INTO handoffs \
         (session_id, source_harness, harness_session_id, base_commit, local_workdir, \
          created_at_unix) \
         VALUES ({session_id}, {source_harness}, {harness_session_id}, {base_commit}, \
                 {local_workdir}, {now})"
    )
    .execute()
    .await?;
    Ok(())
}

/// The handoff row of one session, when it is one.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn find(db: &Db, session: flyco_core::SessionId) -> Result<Option<HandoffRow>, ApiError> {
    let session_id = session;
    Ok(
        sql!(db, "SELECT * FROM handoffs WHERE session_id = {session_id}")
            .fetch_optional()
            .await?,
    )
}

/// What the daemon asks for after cloning: the base commit to rewind to
/// and whether a transcript waits — or `None` for a session that is not a
/// handoff.
///
/// A pending row answers `None` on purpose: the daemon only ever runs
/// after `complete` released provisioning, and a row it could half-see is
/// one the sweep should have caught.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn view_for_daemon(
    db: &Db,
    session: flyco_core::SessionId,
) -> Result<Option<flyco_core::HandoffView>, ApiError> {
    let Some(row) = find(db, session).await? else {
        return Ok(None);
    };
    if row.completed_at_unix.is_none() {
        return Ok(None);
    }
    let Some(patch_sha256) = row.patch_sha256 else {
        return Err(ApiError::CorruptRecord(
            "a completed handoff recorded no patch checksum",
        ));
    };
    Ok(Some(flyco_core::HandoffView {
        base_commit: row.base_commit,
        patch_sha256,
        has_transcript: row.transcript_sha256.is_some(),
    }))
}

/// Proves an upload or completion is pointed at the caller's own pending
/// handoff: the session must be theirs and its row still unfinished.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] when the session is not the
/// caller's, or [`ApiError::HandoffNotPending`] when its handoff is
/// already complete or never existed.
pub async fn require_pending(
    db: &Db,
    user: UserId,
    session: flyco_core::SessionId,
) -> Result<HandoffRow, ApiError> {
    crate::sessions::find(db, user, session).await?;
    let Some(row) = find(db, session).await? else {
        return Err(ApiError::HandoffNotPending);
    };
    if row.completed_at_unix.is_some() {
        return Err(ApiError::HandoffNotPending);
    }
    Ok(row)
}

/// Stores (or replaces) the handoff transcript.
///
/// # Errors
///
/// Returns [`ApiError::Storage`] if the store fails.
pub async fn put_transcript(
    storage: &Storage,
    session: flyco_core::SessionId,
    body: Vec<u8>,
) -> Result<(), ApiError> {
    storage.put(&transcript_key(session), body).await?;
    tracing::debug!(%session, "stored a handoff transcript");
    Ok(())
}

/// Records what an upload route actually received — the SHA-256 and byte
/// count of the body as it landed — so `complete` verifies the manifest
/// against fact rather than against the sender's own claim.
///
/// # Errors
///
/// Returns [`ApiError::HandoffNotPending`] if a completion raced the
/// upload and won, or [`ApiError`] if the database fails.
pub async fn record_patch(
    db: &Db,
    session: flyco_core::SessionId,
    body: &[u8],
) -> Result<(), ApiError> {
    record_upload(db, session, "patch", body).await
}

/// See [`record_patch`].
///
/// # Errors
///
/// Returns [`ApiError::HandoffNotPending`] if a completion raced the
/// upload and won, or [`ApiError`] if the database fails.
pub async fn record_transcript(
    db: &Db,
    session: flyco_core::SessionId,
    body: &[u8],
) -> Result<(), ApiError> {
    record_upload(db, session, "transcript", body).await
}

async fn record_upload(
    db: &Db,
    session: flyco_core::SessionId,
    object: &str,
    body: &[u8],
) -> Result<(), ApiError> {
    let session_id = session;
    let sha256 = hex::encode(Sha256::digest(body));
    let bytes = i64::try_from(body.len()).unwrap_or(i64::MAX);
    // The object and its record are separate writes; a crash between them
    // leaves a stray object the next upload overwrites, and `complete`
    // refuses a row that never recorded one. The `completed_at_unix`
    // guard keeps a late re-upload from rewriting a finished handoff's
    // checksums under it.
    let changed = match object {
        "patch" => {
            sql!(
                db,
                "UPDATE handoffs SET patch_sha256 = {sha256}, patch_bytes = {bytes} \
                 WHERE session_id = {session_id} AND completed_at_unix IS NULL"
            )
        }
        _ => {
            sql!(
                db,
                "UPDATE handoffs SET transcript_sha256 = {sha256}, transcript_bytes = {bytes} \
                 WHERE session_id = {session_id} AND completed_at_unix IS NULL"
            )
        }
    }
    .execute()
    .await?;
    if changed.rows_written == 0 {
        return Err(ApiError::HandoffNotPending);
    }
    Ok(())
}

/// Reads the handoff transcript, if one was uploaded.
///
/// # Errors
///
/// Returns [`ApiError::Storage`] if the store fails.
pub async fn get_transcript(
    storage: &Storage,
    session: flyco_core::SessionId,
) -> Result<Option<Vec<u8>>, ApiError> {
    Ok(storage
        .get(&transcript_key(session))
        .await?
        .map(|object| object.body))
}

/// Marks a pending handoff complete, answering whether this call did it.
///
/// The manifest the sender declares must match what the upload routes
/// actually received — the recorded SHA-256 and byte count of each object,
/// not the sender's claim about them.
///
/// Idempotent on an identical manifest: a retried `complete` after a lost
/// response finds the row already completed under the same checksums and
/// answers `false` rather than conflicted — the caller must not enqueue
/// provisioning again, or a replay would start a second machine. A
/// *different* manifest against a completed row is refused — the payloads
/// are immutable once provisioning was released, or a second upload could
/// silently swap what the machine boots into.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] when the session is not a
/// handoff, [`ApiError::HandoffObjectMissing`] when an object was never
/// uploaded, [`ApiError::HandoffChecksumMismatch`] when what was uploaded
/// is not what the manifest declares, [`ApiError::HandoffNotPending`]
/// when the handoff already completed under a different manifest, or
/// [`ApiError`] if the database fails.
pub async fn complete(
    db: &Db,
    session: flyco_core::SessionId,
    manifest: &flyco_core::HandoffManifest,
) -> Result<bool, ApiError> {
    let Some(row) = find(db, session).await? else {
        return Err(ApiError::SessionNotFound);
    };
    // "The same manifest" means all four facts: a different byte count is
    // a different claim about the object, not a retry of the same one.
    let same = row.patch_sha256.as_deref() == Some(manifest.patch_sha256.as_str())
        && row.patch_bytes == Some(manifest.patch_bytes.cast_signed())
        && row.transcript_sha256.as_deref() == Some(manifest.transcript_sha256.as_str())
        && row.transcript_bytes == Some(manifest.transcript_bytes.cast_signed());
    if row.completed_at_unix.is_some() {
        return if same {
            Ok(false)
        } else {
            Err(ApiError::HandoffNotPending)
        };
    }
    if row.patch_sha256.is_none() {
        return Err(ApiError::HandoffObjectMissing { object: "patch" });
    }
    if row.transcript_sha256.is_none() {
        return Err(ApiError::HandoffObjectMissing {
            object: "transcript",
        });
    }
    if row.patch_sha256.as_deref() != Some(manifest.patch_sha256.as_str())
        || row.patch_bytes != Some(manifest.patch_bytes.cast_signed())
    {
        return Err(ApiError::HandoffChecksumMismatch { object: "patch" });
    }
    if row.transcript_sha256.as_deref() != Some(manifest.transcript_sha256.as_str())
        || row.transcript_bytes != Some(manifest.transcript_bytes.cast_signed())
    {
        return Err(ApiError::HandoffChecksumMismatch {
            object: "transcript",
        });
    }

    let session_id = session;
    let now = now_unix();
    // Zero rows written is a racing `complete` that won — and it could
    // only have won with a manifest verified against the same recorded
    // checksums this call just matched, so it is the replay case.
    let changed = sql!(
        db,
        "UPDATE handoffs SET completed_at_unix = {now} \
         WHERE session_id = {session_id} AND completed_at_unix IS NULL"
    )
    .execute()
    .await?;
    Ok(changed.rows_written != 0)
}

/// Pending handoffs whose upload window has closed.
///
/// Selected separately from [`crate::sessions::stalled_provisions`], which
/// leaves pending handoffs alone: a handoff's clock is the sender's
/// upload, not a machine build.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn abandoned(db: &Db, at_unix: u64) -> Result<Vec<IdleSession>, ApiError> {
    let cutoff = at_unix.saturating_sub(HANDOFF_DEADLINE_SECS);
    // Only sessions still waiting: one the user archived mid-upload left
    // `provisioning` already and the sweep has nothing to fail.
    let provisioning = SessionState::Provisioning;
    Ok(sql!(
        db,
        "SELECT sessions.id AS id, sessions.user_id AS user_id FROM handoffs \
         JOIN sessions ON sessions.id = handoffs.session_id \
         WHERE handoffs.completed_at_unix IS NULL AND handoffs.created_at_unix <= {cutoff} \
         AND sessions.state = {provisioning}"
    )
    .fetch_all()
    .await?)
}
