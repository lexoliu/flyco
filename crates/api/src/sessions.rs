//! The `sessions` table.
//!
//! Ownership is enforced in the `WHERE` clause of every read: a session
//! belonging to somebody else is indistinguishable from one that does not
//! exist, so the API never confirms that an id is real to a caller who has
//! no business knowing.

use flyco_core::{
    ARCHIVE_AFTER_IDLE_SECS, BudgetConfig, BudgetId, HarnessKind, RepoSlug, SessionDetail,
    SessionId, SessionState, SessionSummary, UserId,
};
use skyzen::sql;
use skyzen_services::Db;

use crate::budgets;
use crate::clock::now_unix;
use crate::error::ApiError;

/// The session a caller may still archive, and its budget.
#[derive(Debug, skyzen::FromRow)]
struct SessionRow {
    id: SessionId,
    harness: HarnessKind,
    repo: RepoSlug,
    state: SessionState,
    budget_id: BudgetId,
    failure_reason: Option<String>,
    created_at_unix: u64,
    last_active_unix: u64,
}

impl From<SessionRow> for SessionSummary {
    fn from(row: SessionRow) -> Self {
        Self {
            id: row.id,
            harness: row.harness,
            repo: row.repo,
            state: row.state,
            created_at_unix: row.created_at_unix,
            last_active_unix: row.last_active_unix,
        }
    }
}

async fn detail_from(db: &Db, row: SessionRow) -> Result<SessionDetail, ApiError> {
    let budget = budgets::view(db, row.budget_id).await?;
    // The column outlives the state it explains — a retried session keeps the
    // sentence from the attempt before it until the next attempt clears it —
    // so the reason is reported only while the session is actually failed.
    let failure = (row.state == SessionState::Failed)
        .then(|| row.failure_reason.clone())
        .flatten();
    Ok(SessionDetail {
        summary: row.into(),
        budget,
        failure,
    })
}

/// How many sessions the user currently holds that still occupy their cap.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn live_count(db: &Db, user: UserId) -> Result<u32, ApiError> {
    // A session that holds no execution environment occupies no slot: an
    // archived one released it, and a failed one never got one. Counting
    // either would let a run of failed provisions lock a user out of their
    // own account.
    let archived = SessionState::Archived;
    let failed = SessionState::Failed;
    Ok(sql!(
        db,
        "SELECT COUNT(*) AS live FROM sessions \
         WHERE user_id = {user} AND state != {archived} AND state != {failed}"
    )
    .fetch_scalar()
    .await?)
}

/// Creates a session and the budget it accounts against.
///
/// The session starts in [`SessionState::Provisioning`] and stays there
/// until its daemon reaches the control plane. The machine it will run on is
/// reserved by [`crate::machines::reserve`] and built by the provisioning
/// queue; this function writes neither.
///
/// D1 has no transactions, so the caller's cap is checked in a separate
/// query first. Two simultaneous creates can therefore both pass a check at
/// the boundary; the cap is a spend guard, not a security boundary, and the
/// next read reports the true count.
///
/// # Errors
///
/// Returns [`ApiError::SessionCapReached`] when the caller is at their cap,
/// or a database error otherwise.
pub async fn create(
    db: &Db,
    user: UserId,
    cap: u32,
    harness: HarnessKind,
    repo: &RepoSlug,
    budget: BudgetConfig,
) -> Result<SessionDetail, ApiError> {
    let live = live_count(db, user).await?;
    if live >= cap {
        return Err(ApiError::SessionCapReached { cap });
    }

    let id = SessionId::generate();
    let budget_id = budgets::create(db, id, budget).await?;
    let now = now_unix();

    sql!(
        db,
        "INSERT INTO sessions \
         (id, user_id, harness, repo, state, budget_id, created_at_unix, last_active_unix) \
         VALUES ({id}, {user}, {harness}, {repo}, \
                 {SessionState::Provisioning}, {budget_id}, {now}, {now})"
    )
    .execute()
    .await?;

    find(db, user, id).await
}

/// Lists the caller's sessions, newest first.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails or a stored row is malformed.
pub async fn list(db: &Db, user: UserId) -> Result<Vec<SessionSummary>, ApiError> {
    let rows: Vec<SessionRow> = sql!(
        db,
        "SELECT id, harness, repo, state, budget_id, failure_reason, created_at_unix, \
         last_active_unix \
         FROM sessions WHERE user_id = {user} ORDER BY created_at_unix DESC, id DESC"
    )
    .fetch_all()
    .await?;

    Ok(rows.into_iter().map(Into::into).collect())
}

/// Loads one of the caller's sessions.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session does not exist or
/// belongs to somebody else.
pub async fn find(db: &Db, user: UserId, id: SessionId) -> Result<SessionDetail, ApiError> {
    let row = load(db, user, id).await?;
    detail_from(db, row).await
}

/// Moves a session to a new lifecycle state.
///
/// The move is validated by [`SessionState::transition`], so the table can
/// never record a state the domain model does not allow.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is not the caller's,
/// or [`ApiError::InvalidTransition`] if the lifecycle forbids the move.
pub async fn transition(
    db: &Db,
    user: UserId,
    id: SessionId,
    to: SessionState,
) -> Result<SessionDetail, ApiError> {
    let row = load(db, user, id).await?;
    let next = row
        .state
        .transition(to)
        .map_err(|error| ApiError::InvalidTransition {
            from: error.from,
            to: error.to,
        })?;

    sql!(
        db,
        "UPDATE sessions SET state = {next}, last_active_unix = {now_unix()} \
         WHERE id = {id} AND user_id = {user}"
    )
    .execute()
    .await?;

    find(db, user, id).await
}

/// Whether `user` owns `session`, used to scope approvals.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn is_owned_by(db: &Db, user: UserId, session: SessionId) -> Result<bool, ApiError> {
    let owned: u32 = sql!(
        db,
        "SELECT COUNT(*) AS live FROM sessions WHERE id = {session} AND user_id = {user}"
    )
    .fetch_scalar()
    .await?;
    Ok(owned > 0)
}

/// The lifecycle state of one of the caller's sessions.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session does not exist or
/// belongs to somebody else.
pub async fn state_of(db: &Db, user: UserId, id: SessionId) -> Result<SessionState, ApiError> {
    Ok(load(db, user, id).await?.state)
}

/// Refuses unless the session is running.
///
/// What every route that *drives* a session checks first. A provisioning
/// session has no daemon to hear the command, a paused one is stopped on
/// purpose, and an archived one has no machine at all — so the refusal names
/// the state rather than letting the command disappear into a room nobody is
/// listening to.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is not the caller's,
/// or [`ApiError::SessionNotActive`] if it is not [`SessionState::Active`].
pub async fn require_active(db: &Db, user: UserId, id: SessionId) -> Result<(), ApiError> {
    let state = state_of(db, user, id).await?;
    if state == SessionState::Active {
        Ok(())
    } else {
        Err(ApiError::SessionNotActive { state })
    }
}

async fn load(db: &Db, user: UserId, id: SessionId) -> Result<SessionRow, ApiError> {
    sql!(
        db,
        "SELECT id, harness, repo, state, budget_id, failure_reason, created_at_unix, \
         last_active_unix \
         FROM sessions WHERE id = {id} AND user_id = {user}"
    )
    .fetch_optional()
    .await?
    .ok_or(ApiError::SessionNotFound)
}

// ── The provisioning queue's own reads and writes ──
//
// A queue job carries no user, and it does not need one: the job was
// enqueued by a handler that had already proved the caller owns the session,
// so re-scoping these statements by `user_id` would only be a second copy of
// a check that already happened. They are therefore the one path in this
// module that reads and writes a session by id alone, and they say so.

/// What the provisioning queue needs to know about the session it is
/// building a machine for.
#[derive(Debug, Clone, skyzen::FromRow)]
pub struct ProvisioningTarget {
    /// Whose session it is, which is whose harness account funds it.
    pub user_id: UserId,
    /// Which harness the machine's daemon will drive.
    pub harness: HarnessKind,
    /// Where the session is in its lifecycle right now.
    pub state: SessionState,
}

/// Reads the session a provisioning job names.
///
/// `None` means the row is gone, which is a job to drop rather than a job to
/// retry.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn provisioning_target(
    db: &Db,
    id: SessionId,
) -> Result<Option<ProvisioningTarget>, ApiError> {
    Ok(sql!(
        db,
        "SELECT user_id, harness, state FROM sessions WHERE id = {id}"
    )
    .fetch_optional()
    .await?)
}

/// Records that provisioning gave up, and why.
///
/// The reason is stored rather than only logged: the user is the one who has
/// to act on it — change region, ask for a quota increase, pick another
/// machine type — and a session that fails silently is the state this whole
/// path exists to prevent.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is gone, or
/// [`ApiError::InvalidTransition`] if it is not in a state that can fail.
pub async fn fail(db: &Db, id: SessionId, reason: &str) -> Result<(), ApiError> {
    let state = provisioning_target(db, id)
        .await?
        .ok_or(ApiError::SessionNotFound)?
        .state;
    let next =
        state
            .transition(SessionState::Failed)
            .map_err(|error| ApiError::InvalidTransition {
                from: error.from,
                to: error.to,
            })?;

    sql!(
        db,
        "UPDATE sessions SET state = {next}, failure_reason = {reason.to_owned()}, \
         last_active_unix = {now_unix()} WHERE id = {id}"
    )
    .execute()
    .await?;

    tracing::warn!(session = %id, %reason, "a session's machine could not be provisioned");
    Ok(())
}

/// Pauses an active session because its compute budget is exhausted.
///
/// Repeating the call is intentional: the budget-signal outbox is delivered
/// at least once, so a cron retry may see the durable pause already written.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session disappeared, or
/// [`ApiError::InvalidTransition`] when a non-active session is asked to
/// enter the budget-paused state.
pub async fn pause_for_budget(db: &Db, id: SessionId) -> Result<(), ApiError> {
    let state: SessionState = sql!(db, "SELECT state FROM sessions WHERE id = {id}")
        .fetch_scalar_optional()
        .await?
        .ok_or(ApiError::SessionNotFound)?;
    if state == SessionState::Paused {
        return Ok(());
    }
    let next =
        state
            .transition(SessionState::Paused)
            .map_err(|error| ApiError::InvalidTransition {
                from: error.from,
                to: error.to,
            })?;
    sql!(
        db,
        "UPDATE sessions SET state = {next}, last_active_unix = {now_unix()} WHERE id = {id}"
    )
    .execute()
    .await?;
    Ok(())
}

/// Puts a session back into [`SessionState::Provisioning`] and clears the
/// reason the previous attempt left behind.
///
/// The one move `POST /v1/sessions/{id}/resume` makes durable before the job
/// is enqueued, so a browser that reloads immediately sees a session on its
/// way back rather than the state it was resumed out of.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is not the caller's,
/// or [`ApiError::InvalidTransition`] if it is not resumable.
pub async fn resume(db: &Db, user: UserId, id: SessionId) -> Result<SessionDetail, ApiError> {
    let row = load(db, user, id).await?;
    let next = row
        .state
        .transition(SessionState::Provisioning)
        .map_err(|error| ApiError::InvalidTransition {
            from: error.from,
            to: error.to,
        })?;

    sql!(
        db,
        "UPDATE sessions SET state = {next}, failure_reason = NULL, \
         last_active_unix = {now_unix()} WHERE id = {id} AND user_id = {user}"
    )
    .execute()
    .await?;

    find(db, user, id).await
}

/// Records the harness-native session id a later resume must reopen.
///
/// Written when the daemon announces `Started`, and kept across archive so
/// a rebuilt machine continues the same conversation rather than opening a
/// fresh one.
///
/// # Errors
///
/// Returns [`ApiError`] if the write fails.
pub async fn record_harness_session(
    db: &Db,
    id: SessionId,
    harness_session_id: &str,
) -> Result<(), ApiError> {
    sql!(
        db,
        "UPDATE sessions SET harness_session_id = {harness_session_id}, \
         last_active_unix = {now_unix()} WHERE id = {id}"
    )
    .execute()
    .await?;
    Ok(())
}

/// The harness-native session id recorded for this session, if any.
///
/// # Errors
///
/// Returns [`ApiError`] if the read fails.
pub async fn harness_session_id(db: &Db, id: SessionId) -> Result<Option<String>, ApiError> {
    #[derive(Debug, skyzen::FromRow)]
    struct Row {
        harness_session_id: Option<String>,
    }

    let row: Option<Row> = sql!(
        db,
        "SELECT harness_session_id FROM sessions WHERE id = {id}"
    )
    .fetch_optional()
    .await?;
    Ok(row.and_then(|row| row.harness_session_id))
}

/// Marks a provisioning session active because its daemon has arrived.
///
/// A session goes live when its daemon greets the control plane, not when a
/// provider's API call returns: a machine that exists is not an agent that
/// is ready. The greeting itself is a `Hello` frame validated inside the
/// session's room — and a Durable Object cannot reach D1, so the durable
/// half of it happens here, in the Worker, on the very upgrade the daemon
/// sends that frame down.
///
/// Silently a no-op for a session that is already past provisioning: a
/// daemon reconnects after every eviction, every redeploy and every dropped
/// socket, and none of those is a lifecycle event.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn daemon_arrived(db: &Db, id: SessionId) -> Result<(), ApiError> {
    let provisioning = SessionState::Provisioning;
    let active = SessionState::Active;
    let written = sql!(
        db,
        "UPDATE sessions SET state = {active}, last_active_unix = {now_unix()} \
         WHERE id = {id} AND state = {provisioning}"
    )
    .execute()
    .await?;

    if written.rows_written > 0 {
        tracing::info!(session = %id, "a session went live: its daemon reached the control plane");
    }
    Ok(())
}

/// A session idle long enough that flyco archives it automatically.
#[derive(Debug, skyzen::FromRow)]
pub struct IdleSession {
    /// Identifier.
    pub id: SessionId,
    /// Owner, so archive can destroy their machine.
    pub user_id: UserId,
}

/// Sessions that have sat idle past [`ARCHIVE_AFTER_IDLE_SECS`] and still
/// hold an environment.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn idle_since(db: &Db, at_unix: u64) -> Result<Vec<IdleSession>, ApiError> {
    let cutoff = at_unix.saturating_sub(ARCHIVE_AFTER_IDLE_SECS);
    let active = SessionState::Active;
    let paused = SessionState::Paused;
    let interrupted = SessionState::Interrupted;
    Ok(sql!(
        db,
        "SELECT id, user_id FROM sessions \
         WHERE last_active_unix <= {cutoff} \
         AND (state = {active} OR state = {paused} OR state = {interrupted})"
    )
    .fetch_all()
    .await?)
}
