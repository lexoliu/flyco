//! The `sessions` table.
//!
//! Ownership is enforced in the `WHERE` clause of every read: a session
//! belonging to somebody else is indistinguishable from one that does not
//! exist, so the API never confirms that an id is real to a caller who has
//! no business knowing.

use flyco_core::{
    BudgetConfig, BudgetId, HarnessKind, RepoSlug, SessionDetail, SessionId, SessionState,
    SessionSummary, UserId,
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
    Ok(SessionDetail {
        summary: row.into(),
        budget,
    })
}

/// How many sessions the user currently holds that still occupy their cap.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn live_count(db: &Db, user: UserId) -> Result<u32, ApiError> {
    let archived = SessionState::Archived;
    Ok(sql!(
        db,
        "SELECT COUNT(*) AS live FROM sessions WHERE user_id = {user} AND state != {archived}"
    )
    .fetch_scalar()
    .await?)
}

/// Creates a session and the budget it accounts against.
///
/// The session starts in [`SessionState::Provisioning`]; nothing is
/// provisioned yet — that is a later milestone — so it simply waits there.
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
        "SELECT id, harness, repo, state, budget_id, created_at_unix, last_active_unix \
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
        "SELECT id, harness, repo, state, budget_id, created_at_unix, last_active_unix \
         FROM sessions WHERE id = {id} AND user_id = {user}"
    )
    .fetch_optional()
    .await?
    .ok_or(ApiError::SessionNotFound)
}
