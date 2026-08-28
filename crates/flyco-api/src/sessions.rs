//! The `sessions` table.
//!
//! Ownership is enforced in the `WHERE` clause of every read: a session
//! belonging to somebody else is indistinguishable from one that does not
//! exist, so the API never confirms that an id is real to a caller who has
//! no business knowing.

use flyco_core::{
    BudgetConfig, HarnessKind, RepoSlug, SessionDetail, SessionId, SessionState, SessionSummary,
    UserId,
};
use serde::Deserialize;
use skyzen_services::Db;

use crate::budgets;
use crate::clock::now_unix;
use crate::error::ApiError;
use crate::sql::{decode_enum, encode_enum, from_column, to_column};

/// The session a caller may still archive, and its budget.
#[derive(Debug, Deserialize)]
struct SessionRow {
    id: String,
    harness: String,
    repo: String,
    state: String,
    budget_id: String,
    created_at_unix: i64,
    last_active_unix: i64,
}

#[derive(Debug, Deserialize)]
struct CountRow {
    live: i64,
}

impl SessionRow {
    fn into_summary(self) -> Result<SessionSummary, ApiError> {
        Ok(SessionSummary {
            id: self
                .id
                .parse()
                .map_err(|_| ApiError::CorruptRecord("sessions.id is not a UUID"))?,
            harness: decode_enum::<HarnessKind>(&self.harness, "sessions.harness")?,
            repo: self
                .repo
                .parse::<RepoSlug>()
                .map_err(|_| ApiError::CorruptRecord("sessions.repo is not `owner/name`"))?,
            state: decode_enum::<SessionState>(&self.state, "sessions.state")?,
            created_at_unix: from_column(self.created_at_unix, "sessions.created_at_unix")?,
            last_active_unix: from_column(self.last_active_unix, "sessions.last_active_unix")?,
        })
    }

    fn budget(&self) -> Result<flyco_core::BudgetId, ApiError> {
        self.budget_id
            .parse()
            .map_err(|_| ApiError::CorruptRecord("sessions.budget_id is not a UUID"))
    }
}

async fn detail_from(db: &Db, row: SessionRow) -> Result<SessionDetail, ApiError> {
    let budget = budgets::view(db, row.budget()?).await?;
    Ok(SessionDetail {
        summary: row.into_summary()?,
        budget,
    })
}

/// How many sessions the user currently holds that still occupy their cap.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn live_count(db: &Db, user: UserId) -> Result<u32, ApiError> {
    let row: CountRow = db
        .query("SELECT COUNT(*) AS live FROM sessions WHERE user_id = ? AND state != ?")
        .bind(user.to_string())
        .bind(encode_enum(&SessionState::Archived)?)
        .fetch_one()
        .await?;

    u32::try_from(row.live)
        .map_err(|_| ApiError::CorruptRecord("a user holds an implausible number of sessions"))
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

    db.query(
        "INSERT INTO sessions \
         (id, user_id, harness, repo, state, budget_id, created_at_unix, last_active_unix) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(user.to_string())
    .bind(encode_enum(&harness)?)
    .bind(repo.as_str().to_owned())
    .bind(encode_enum(&SessionState::Provisioning)?)
    .bind(budget_id.to_string())
    .bind(to_column(now))
    .bind(to_column(now))
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
    let rows: Vec<SessionRow> = db
        .query(
            "SELECT id, harness, repo, state, budget_id, created_at_unix, last_active_unix \
             FROM sessions WHERE user_id = ? ORDER BY created_at_unix DESC, id DESC",
        )
        .bind(user.to_string())
        .fetch_all()
        .await?;

    rows.into_iter().map(SessionRow::into_summary).collect()
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
    let from = decode_enum::<SessionState>(&row.state, "sessions.state")?;
    let next = from
        .transition(to)
        .map_err(|error| ApiError::InvalidTransition {
            from: error.from,
            to: error.to,
        })?;

    db.query("UPDATE sessions SET state = ?, last_active_unix = ? WHERE id = ? AND user_id = ?")
        .bind(encode_enum(&next)?)
        .bind(to_column(now_unix()))
        .bind(id.to_string())
        .bind(user.to_string())
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
    let row: CountRow = db
        .query("SELECT COUNT(*) AS live FROM sessions WHERE id = ? AND user_id = ?")
        .bind(session.to_string())
        .bind(user.to_string())
        .fetch_one()
        .await?;
    Ok(row.live > 0)
}

async fn load(db: &Db, user: UserId, id: SessionId) -> Result<SessionRow, ApiError> {
    db.query(
        "SELECT id, harness, repo, state, budget_id, created_at_unix, last_active_unix \
         FROM sessions WHERE id = ? AND user_id = ?",
    )
    .bind(id.to_string())
    .bind(user.to_string())
    .fetch_optional()
    .await?
    .ok_or(ApiError::SessionNotFound)
}
