//! The `approvals` table.
//!
//! Approvals reach a user through the session that raised them, so every
//! query here joins `sessions` on `user_id` rather than trusting a caller's
//! claim about which approval is theirs.

use flyco_core::{
    ApprovalId, ApprovalState, ApprovalView, SessionId, UserId, wire::ApprovalDecision,
    wire::ApprovalPayload,
};
use skyzen::sql;
use skyzen_services::Db;

use crate::clock::now_unix;
use crate::error::ApiError;

/// The columns every read on this path projects.
#[derive(Debug, skyzen::FromRow)]
struct ApprovalRow {
    id: ApprovalId,
    session_id: SessionId,
    /// What the agent asked for, kept as a JSON document in a text column.
    #[row(json)]
    payload: ApprovalPayload,
    state: ApprovalState,
    created_at_unix: u64,
}

impl From<ApprovalRow> for ApprovalView {
    fn from(row: ApprovalRow) -> Self {
        Self {
            id: row.id,
            session: row.session_id,
            payload: row.payload,
            state: row.state,
            created_at_unix: row.created_at_unix,
        }
    }
}

/// Raises an approval against a session.
///
/// The daemon relay is a later milestone; until it exists this is the write
/// path the tests use to put an approval in front of a user.
///
/// # Errors
///
/// Returns [`ApiError`] if the payload cannot be encoded or the insert fails.
pub async fn raise(
    db: &Db,
    session: SessionId,
    payload: &ApprovalPayload,
) -> Result<ApprovalId, ApiError> {
    let id = ApprovalId::generate();
    let encoded = serde_json::to_string(payload)
        .map_err(|_| ApiError::CorruptRecord("an approval payload failed to encode"))?;

    sql!(
        db,
        "INSERT INTO approvals (id, session_id, payload, state, created_at_unix, decided_at_unix) \
         VALUES ({id}, {session}, {encoded}, {ApprovalState::Pending}, {now_unix()}, NULL)"
    )
    .execute()
    .await?;

    Ok(id)
}

/// Lists the caller's approvals, newest first, optionally narrowed to one
/// session and one state.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails or a stored row is malformed.
pub async fn list(
    db: &Db,
    user: UserId,
    session: Option<SessionId>,
    state: Option<ApprovalState>,
) -> Result<Vec<ApprovalView>, ApiError> {
    // A NULL filter matches everything, which keeps one prepared statement
    // for all four combinations instead of assembling SQL per request.
    let rows: Vec<ApprovalRow> = sql!(
        db,
        "SELECT a.id, a.session_id, a.payload, a.state, a.created_at_unix \
         FROM approvals a JOIN sessions s ON s.id = a.session_id \
         WHERE s.user_id = {user} \
         AND ({session} IS NULL OR a.session_id = {session}) \
         AND ({state} IS NULL OR a.state = {state}) \
         ORDER BY a.created_at_unix DESC, a.id DESC"
    )
    .fetch_all()
    .await?;

    Ok(rows.into_iter().map(Into::into).collect())
}

/// Records the user's decision on one of their pending approvals.
///
/// The `UPDATE` is guarded on the row still being pending, so two
/// simultaneous decisions cannot both win: D1 has no transactions, and the
/// conditional write is what makes "decided exactly once" true rather than
/// merely likely.
///
/// # Errors
///
/// Returns [`ApiError::ApprovalNotFound`] if the approval is not the
/// caller's, or [`ApiError::ApprovalAlreadyDecided`] if it has been decided.
pub async fn decide(
    db: &Db,
    user: UserId,
    id: ApprovalId,
    decision: ApprovalDecision,
) -> Result<ApprovalView, ApiError> {
    let current = load(db, user, id).await?;
    let next = current
        .state
        .decide(decision)
        .map_err(|error| ApiError::ApprovalAlreadyDecided { state: error.state })?;

    let result = sql!(
        db,
        "UPDATE approvals SET state = {next}, decided_at_unix = {now_unix()} \
         WHERE id = {id} AND state = {ApprovalState::Pending}"
    )
    .execute()
    .await?;

    if result.rows_written == 0 {
        return Err(ApiError::ApprovalAlreadyDecided {
            state: load(db, user, id).await?.state,
        });
    }

    load(db, user, id).await
}

/// Loads an approval by the session that raised it.
///
/// The daemon-scoped counterpart of the user-scoped [`load`]: a daemon
/// token proves which *session* is calling, never which user, so the
/// ownership clause is the session rather than a join back to `users`.
///
/// # Errors
///
/// Returns [`ApiError::ApprovalNotFound`] if the approval does not exist or
/// belongs to another session.
pub async fn find_for_session(
    db: &Db,
    session: SessionId,
    id: ApprovalId,
) -> Result<ApprovalView, ApiError> {
    let row: Option<ApprovalRow> = sql!(
        db,
        "SELECT id, session_id, payload, state, created_at_unix \
         FROM approvals WHERE id = {id} AND session_id = {session}"
    )
    .fetch_optional()
    .await?;

    Ok(row.ok_or(ApiError::ApprovalNotFound)?.into())
}

async fn load(db: &Db, user: UserId, id: ApprovalId) -> Result<ApprovalView, ApiError> {
    let row: Option<ApprovalRow> = sql!(
        db,
        "SELECT a.id, a.session_id, a.payload, a.state, a.created_at_unix \
         FROM approvals a JOIN sessions s ON s.id = a.session_id \
         WHERE a.id = {id} AND s.user_id = {user}"
    )
    .fetch_optional()
    .await?;

    Ok(row.ok_or(ApiError::ApprovalNotFound)?.into())
}
