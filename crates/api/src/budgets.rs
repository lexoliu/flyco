//! The `budgets` table and its append-only `spend_events` ledger.
//!
//! The ledger is the truth and [`flyco_core::BudgetState`] is the only thing
//! allowed to interpret it: totals and thresholds are produced by replaying
//! every event through the engine, never by arithmetic in SQL. The
//! `budgets` row caches the result so a reader that does not need exactness
//! can have it in one query, and the cache is refreshed on every replay.

use flyco_core::{
    BudgetConfig, BudgetId, BudgetSignal, BudgetSignalId, BudgetStage, BudgetState, BudgetView,
    SessionId, SpendEvent, SpendEventId, SpendKind, Usd,
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

#[derive(Debug, skyzen::FromRow)]
struct BudgetRow {
    session_id: SessionId,
    limit_micros: Usd,
    stage: BudgetStage,
}

/// One threshold delivery waiting for the session room.
#[derive(Debug, Clone, Copy, skyzen::FromRow)]
pub struct PendingSignal {
    /// Durable outbox identifier.
    pub id: BudgetSignalId,
    /// Session that crossed the threshold.
    pub session_id: SessionId,
    /// Signal the daemon and browser must receive.
    pub signal: BudgetSignal,
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
    reconcile(db, budget).await
}

/// Replays one budget, persists newly crossed thresholds in the delivery
/// outbox, and refreshes its cache.
async fn reconcile(db: &Db, budget: BudgetId) -> Result<BudgetView, ApiError> {
    let row: BudgetRow = sql!(
        db,
        "SELECT session_id, limit_micros, stage FROM budgets WHERE id = {budget}"
    )
    .fetch_optional()
    .await?
    .ok_or(ApiError::CorruptRecord(
        "a session points at a budget that does not exist",
    ))?;
    let config = BudgetConfig::new(row.limit_micros)
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
        let signal = state.apply(event.into());
        if state.stage() > row.stage
            && let Some(signal) = signal
        {
            let id = BudgetSignalId::generate();
            let ordinal = signal.ordinal();
            sql!(
                db,
                "INSERT OR IGNORE INTO budget_signals \
                 (id, budget_id, session_id, signal, ordinal, delivered, created_at_unix) \
                 VALUES ({id}, {budget}, {row.session_id}, {signal}, {ordinal}, 0, {now_unix()})"
            )
            .execute()
            .await?;
        }
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
    record_at(db, budget, kind, amount, detail, now_unix(), None).await
}

/// Appends one uniquely keyed metering window.
///
/// A repeated key is an at-least-once scheduled delivery of the same window
/// and is ignored. Reconciliation still runs, so a failure after the ledger
/// insert but before its threshold outbox write repairs itself on retry.
///
/// # Errors
///
/// Returns [`ApiError`] if the ledger, outbox, or budget cache cannot be
/// read or written.
pub async fn record_metered(
    db: &Db,
    budget: BudgetId,
    kind: SpendKind,
    amount: Usd,
    detail: &str,
    at_unix: u64,
    meter_key: &str,
) -> Result<(), ApiError> {
    record_at(db, budget, kind, amount, detail, at_unix, Some(meter_key)).await
}

async fn record_at(
    db: &Db,
    budget: BudgetId,
    kind: SpendKind,
    amount: Usd,
    detail: &str,
    at_unix: u64,
    meter_key: Option<&str>,
) -> Result<(), ApiError> {
    sql!(
        db,
        "INSERT OR IGNORE INTO spend_events \
         (id, budget_id, kind, amount_micros, at_unix, detail, meter_key) \
         VALUES ({SpendEventId::generate()}, {budget}, {kind}, {amount}, {at_unix}, \
                 {detail}, {meter_key.map(str::to_owned)})"
    )
    .execute()
    .await?;
    let _ = reconcile(db, budget).await?;
    Ok(())
}

/// Records that a reclaimed machine was replaced by a restart of itself.
///
/// A zero-amount entry, and the zero is the honest number: the machine that
/// comes back is the machine that went, on the same disk at the same price,
/// so nothing is charged *for the replacement*. What the ledger gains is
/// the line that explains the shape of the bill around it — the compute
/// meter stopped and restarted while the storage meter never paused,
/// because the disk was kept and billed throughout the gap. Without it the
/// user reads a session that quietly stopped consuming compute for four
/// minutes and has nothing to attribute it to.
///
/// Keyed on the machine and the instant the provider announced the
/// reclamation, so an at-least-once queue delivering the same recovery
/// twice writes one line, and a session reclaimed twice writes two.
///
/// # Errors
///
/// Returns [`ApiError`] if the session has no budget, or the ledger cannot
/// be written.
pub async fn record_replacement(
    db: &Db,
    session: SessionId,
    machine: flyco_core::MachineId,
    reclaimed_at_unix: u64,
    machine_type: &str,
) -> Result<(), ApiError> {
    let budget: BudgetId = sql!(db, "SELECT budget_id FROM sessions WHERE id = {session}")
        .fetch_scalar_optional()
        .await?
        .ok_or(ApiError::SessionNotFound)?;
    let detail = format!(
        "spot capacity on {machine_type} was reclaimed at {reclaimed_at_unix}; machine {machine} restarted on the same disk"
    );
    let key = format!("spot-recovery:{machine}:{reclaimed_at_unix}");
    record_at(
        db,
        budget,
        SpendKind::Compute,
        Usd::ZERO,
        &detail,
        reclaimed_at_unix,
        Some(&key),
    )
    .await
}

/// Reads every undelivered threshold in durable creation order.
///
/// # Errors
///
/// Returns [`ApiError`] if the outbox cannot be read.
pub async fn pending_signals(db: &Db) -> Result<Vec<PendingSignal>, ApiError> {
    Ok(sql!(
        db,
        "SELECT id, session_id, signal FROM budget_signals \
         WHERE delivered = 0 ORDER BY created_at_unix, ordinal, id"
    )
    .fetch_all()
    .await?)
}

/// Marks an outbox signal delivered after its room accepted the command.
///
/// # Errors
///
/// Returns [`ApiError`] if the outbox cannot be updated.
pub async fn mark_delivered(db: &Db, id: BudgetSignalId) -> Result<(), ApiError> {
    sql!(
        db,
        "UPDATE budget_signals SET delivered = 1 WHERE id = {id}"
    )
    .execute()
    .await?;
    Ok(())
}
