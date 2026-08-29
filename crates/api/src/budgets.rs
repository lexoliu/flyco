//! The `budgets` table and its append-only `spend_events` ledger.
//!
//! The ledger is the truth and [`flyco_core::BudgetState`] is the only thing
//! allowed to interpret it: totals and thresholds are produced by replaying
//! every event through the engine, never by arithmetic in SQL. The
//! `budgets` row caches the result so a reader that does not need exactness
//! can have it in one query, and the cache is refreshed on every replay.

use flyco_core::{
    BudgetConfig, BudgetId, BudgetState, BudgetView, SessionId, SpendEvent, SpendEventId,
    SpendKind, Usd,
};
use skyzen::sql;
use skyzen_services::Db;

use crate::clock::now_unix;
use crate::error::ApiError;

/// One accrual, as the ledger stores it.
#[derive(Debug, skyzen::FromRow)]
struct SpendRow {
    kind: SpendKind,
    amount_micros: Usd,
}

impl From<SpendRow> for SpendEvent {
    fn from(row: SpendRow) -> Self {
        Self {
            kind: row.kind,
            amount: row.amount_micros,
        }
    }
}

/// Creates the budget a session accounts against.
///
/// # Errors
///
/// Returns [`ApiError`] if the insert fails.
pub async fn create(
    db: &Db,
    session: SessionId,
    config: BudgetConfig,
) -> Result<BudgetId, ApiError> {
    let id = BudgetId::generate();
    let state = BudgetState::new(config);

    sql!(
        db,
        "INSERT INTO budgets (id, session_id, limit_micros, spent_micros, stage) \
         VALUES ({id}, {session}, {config.limit()}, 0, {state.stage()})"
    )
    .execute()
    .await?;

    Ok(id)
}

/// Replays the ledger for `budget` and refreshes the cached totals.
///
/// # Errors
///
/// Returns [`ApiError::CorruptRecord`] if the budget row is missing or
/// holds a limit the engine rejects, or a database error otherwise.
pub async fn view(db: &Db, budget: BudgetId) -> Result<BudgetView, ApiError> {
    let limit: Option<Usd> = sql!(db, "SELECT limit_micros FROM budgets WHERE id = {budget}")
        .fetch_scalar_optional()
        .await?;
    let limit = limit.ok_or(ApiError::CorruptRecord(
        "a session points at a budget that does not exist",
    ))?;
    let config = BudgetConfig::new(limit)
        .map_err(|_| ApiError::CorruptRecord("budgets.limit_micros is zero"))?;

    let events: Vec<SpendRow> = sql!(
        db,
        "SELECT kind, amount_micros FROM spend_events \
         WHERE budget_id = {budget} ORDER BY at_unix, id"
    )
    .fetch_all()
    .await?;

    let mut state = BudgetState::new(config);
    for event in events {
        // Signals were delivered when the spend was first recorded; a replay
        // reconstructs the totals, it does not re-announce them.
        let _ = state.apply(event.into());
    }

    sql!(
        db,
        "UPDATE budgets SET spent_micros = {state.spent()}, stage = {state.stage()} \
         WHERE id = {budget}"
    )
    .execute()
    .await?;

    Ok(state.into())
}

/// Appends one accrual to the ledger.
///
/// Nothing in M2b prices machine time yet; this is the write path the
/// scheduled pricing worker and the session Durable Object both use, and
/// what the budget tests drive.
///
/// # Errors
///
/// Returns [`ApiError`] if the insert fails.
pub async fn record(
    db: &Db,
    budget: BudgetId,
    kind: SpendKind,
    amount: Usd,
    detail: &str,
) -> Result<(), ApiError> {
    sql!(
        db,
        "INSERT INTO spend_events (id, budget_id, kind, amount_micros, at_unix, detail) \
         VALUES ({SpendEventId::generate()}, {budget}, {kind}, {amount}, {now_unix()}, {detail})"
    )
    .execute()
    .await?;

    Ok(())
}
