//! The `budgets` table and its append-only `spend_events` ledger.
//!
//! The ledger is the truth and [`flyco_core::BudgetState`] is the only thing
//! allowed to interpret it: totals and thresholds are produced by replaying
//! every event through the engine, never by arithmetic in SQL. The
//! `budgets` row caches the result, and `folded_events` records how many
//! ledger rows the last replay covered, so a read that finds the ledger's
//! length unmoved serves the cache without writing.

use flyco_core::{
    BudgetConfig, BudgetId, BudgetSignal, BudgetSignalId, BudgetStage, BudgetState, BudgetView,
    SessionId, SpendEvent, SpendEventId, SpendKind, Usd,
};
use skyzen::sql;
use skyzen_services::{BatchStatement, Db};

use crate::clock::now_unix;
use crate::error::ApiError;

/// One accrual, as the ledger stores it.
#[derive(Debug, skyzen::FromRow)]
struct SpendRow {
    kind: SpendKind,
    amount_micros: Usd,
}

#[derive(Debug, skyzen::FromRow)]
pub(crate) struct BudgetRow {
    session_id: SessionId,
    limit_micros: Usd,
    spent_micros: Usd,
    stage: BudgetStage,
    folded_events: i64,
    ledger_events: i64,
}

impl BudgetRow {
    /// The view the last replay left in the row itself.
    ///
    /// Only reached while `folded_events == ledger_events`; the ledger is
    /// append-only, so an unmoved count means the cached `spent_micros` and
    /// `stage` are exactly what a fresh replay would produce.
    fn cached(&self) -> Result<BudgetView, ApiError> {
        let config = BudgetConfig::new(self.limit_micros)
            .map_err(|_| ApiError::CorruptRecord("budgets.limit_micros is zero"))?;
        Ok(BudgetView {
            limit: config.limit(),
            spent: self.spent_micros,
            remaining: config.limit().saturating_sub(self.spent_micros),
            stage: self.stage,
        })
    }
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
        "INSERT INTO budgets \
         (id, session_id, limit_micros, spent_micros, stage, folded_events) \
         VALUES ({id}, {session}, {config.limit()}, 0, {state.stage()}, 0)"
    )
    .execute()
    .await?;

    Ok(id)
}

/// The cached totals for `budget`, replaying the ledger only if it moved.
///
/// Every spend lands in the ledger and every write path reconciles after
/// itself, so `folded_events` trails the ledger's length only between an
/// append and the reconcile that follows it — and a read in that gap folds
/// the new rows itself. When the count has not moved the cache is the
/// replay's answer and the read issues no write.
///
/// # Errors
///
/// Returns [`ApiError::CorruptRecord`] if the budget row is missing or
/// holds a limit the engine rejects, or a database error otherwise.
pub async fn view(db: &Db, budget: BudgetId) -> Result<BudgetView, ApiError> {
    let row = load(db, budget).await?;
    if row.folded_events == row.ledger_events {
        return row.cached();
    }
    fold(db, budget, row).await
}

/// The watermark that says "never folded": below every real ledger length.
///
/// What a budget is born with before its first replay, and what a limit
/// change resets it to in the same statement, so a read that lands between
/// the new limit and the reconcile that follows it folds under the new
/// limit rather than serving the old stage from the cache.
const UNFOLDED: i64 = -1;

/// Replays one budget, persists newly crossed thresholds in the delivery
/// outbox, and refreshes its cache.
///
/// `pub(crate)` for the metering sweep: it records a whole machine's
/// backlog of window events in one batch and reconciles once at the end,
/// because replaying the ledger per window is O(events) each and a busy
/// budget once outspent the cron's CPU budget before the sweep's other
/// legs could run.
pub(crate) async fn reconcile(db: &Db, budget: BudgetId) -> Result<BudgetView, ApiError> {
    fold(db, budget, load(db, budget).await?).await
}

/// The budget row plus the ledger's current length, in one statement.
pub(crate) async fn load(db: &Db, budget: BudgetId) -> Result<BudgetRow, ApiError> {
    sql!(
        db,
        "SELECT session_id, limit_micros, spent_micros, stage, folded_events, \
         (SELECT COUNT(*) FROM spend_events WHERE budget_id = budgets.id) AS ledger_events \
         FROM budgets WHERE id = {budget}"
    )
    .fetch_optional()
    .await?
    .ok_or(ApiError::CorruptRecord(
        "a session points at a budget that does not exist",
    ))
}

/// Replays the ledger for `budget` and writes back the result: the newly
/// crossed thresholds into the delivery outbox, and the refreshed cache —
/// including the folded count a later read compares the ledger against.
///
/// The write-back is one batch, and every statement in it is conditioned
/// on the limit the replay ran under still being the budget's limit. A
/// fold that lost that race — `set_limit` landed between its read and its
/// write — would otherwise stamp a stage derived from the old limit over
/// the one the limit change just produced, and the watermark would then
/// certify it until the ledger next moved. The loser writes nothing and
/// folds again from what is there now.
pub(crate) async fn fold(
    db: &Db,
    budget: BudgetId,
    mut row: BudgetRow,
) -> Result<BudgetView, ApiError> {
    loop {
        let (state, statements) = replay(db, budget, &row).await?;
        let results = db.execute_batch(statements).await?;
        // The refresh is last and `RETURNING`s the row it matched: rows
        // returned are the one signal every backend reports for a batch
        // statement, where a written-row count is what a backend happens
        // to expose.
        let refreshed = results.last().is_some_and(|result| !result.rows.is_empty());
        if refreshed {
            return Ok(state.into());
        }
        tracing::debug!(%budget, "the limit changed under a fold; folding again");
        row = load(db, budget).await?;
    }
}

/// Runs the ledger through the engine under `row`'s limit and returns the
/// state with the batch that would record it: every newly crossed
/// threshold, the pruning of the ones the replay did not reach, and the
/// cache refresh last — each guarded on the limit the replay used.
async fn replay(
    db: &Db,
    budget: BudgetId,
    row: &BudgetRow,
) -> Result<(BudgetState, Vec<BatchStatement>), ApiError> {
    let config = BudgetConfig::new(row.limit_micros)
        .map_err(|_| ApiError::CorruptRecord("budgets.limit_micros is zero"))?;

    let events: Vec<SpendRow> = sql!(
        db,
        "SELECT kind, amount_micros FROM spend_events \
         WHERE budget_id = {budget} ORDER BY at_unix, id"
    )
    .fetch_all()
    .await?;
    let folded = i64::try_from(events.len()).expect("a spend ledger fits in i64");

    let mut statements = Vec::new();
    let mut state = BudgetState::new(config);
    for event in events {
        let signal = state.apply(event.into());
        if state.stage() > row.stage
            && let Some(signal) = signal
        {
            statements.push(
                BatchStatement::new(
                    "INSERT OR IGNORE INTO budget_signals \
                     (id, budget_id, session_id, signal, ordinal, delivered, created_at_unix) \
                     SELECT ?, ?, ?, ?, ?, 0, ? FROM budgets \
                     WHERE id = ? AND limit_micros = ?",
                )
                .bind(BudgetSignalId::generate())
                .bind(budget)
                .bind(row.session_id)
                .bind(signal)
                .bind(signal.ordinal())
                .bind(now_unix())
                .bind(budget)
                .bind(row.limit_micros),
            );
        }
    }

    // A raised limit un-crosses thresholds, and the outbox is keyed
    // `UNIQUE (budget_id, signal)` so a row left behind would silence that
    // threshold for the rest of the budget's life — including the pause.
    // Dropping the signals this replay did not reach keeps the outbox
    // describing the budget as it now stands, so re-spending re-announces.
    statements.push(
        BatchStatement::new(
            "DELETE FROM budget_signals \
             WHERE budget_id = ? AND ordinal > ? \
             AND EXISTS (SELECT 1 FROM budgets WHERE id = ? AND limit_micros = ?)",
        )
        .bind(budget)
        .bind(state.stage().ordinal())
        .bind(budget)
        .bind(row.limit_micros),
    );
    statements.push(
        BatchStatement::new(
            "UPDATE budgets \
             SET spent_micros = ?, stage = ?, folded_events = ? \
             WHERE id = ? AND limit_micros = ? \
             RETURNING id",
        )
        .bind(state.spent())
        .bind(state.stage())
        .bind(folded)
        .bind(budget)
        .bind(row.limit_micros),
    );
    Ok((state, statements))
}

/// Changes what a budget may spend, and replays the ledger against it.
///
/// The limit is the one part of a budget that is not append-only: the
/// ledger under it is untouched and every total is still derived from it,
/// so raising a limit is a re-reading of the same history rather than an
/// adjustment to it.
///
/// # Errors
///
/// Returns [`ApiError::InvalidBudget`] if the limit is zero, or a database
/// error if the budget cannot be written or replayed.
pub async fn set_limit(db: &Db, budget: BudgetId, limit: Usd) -> Result<BudgetView, ApiError> {
    let config = BudgetConfig::new(limit).map_err(|_| ApiError::InvalidBudget)?;
    // The watermark goes with the limit, in the same statement: a read
    // between here and the reconcile below folds under the new limit
    // rather than serving the old stage from the cache.
    sql!(
        db,
        "UPDATE budgets SET limit_micros = {config.limit()}, folded_events = {UNFOLDED} \
         WHERE id = {budget}"
    )
    .execute()
    .await?;
    reconcile(db, budget).await
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

/// The spend event one meter window contributes, held for a batch write.
///
/// The metering sweep writes a machine's windows through
/// [`Db::execute_batch`] — an atomic unit on every backend — rather than
/// one statement per window, because a machine's backlog after an
/// interruption is thousands of windows and each round trip is billed
/// against the cron's CPU budget. Reconciliation is the caller's, run once
/// after the batch rather than once per event, for the same reason.
///
/// A repeated key is an at-least-once scheduled delivery of the same window
/// and is ignored, so a batch retried after a crash records nothing twice.
#[must_use]
pub(crate) fn metered_statement(
    budget: BudgetId,
    kind: SpendKind,
    amount: Usd,
    detail: &str,
    at_unix: u64,
    meter_key: &str,
) -> BatchStatement {
    BatchStatement::new(
        "INSERT OR IGNORE INTO spend_events \
         (id, budget_id, kind, amount_micros, at_unix, detail, meter_key) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(SpendEventId::generate())
    .bind(budget)
    .bind(kind)
    .bind(amount)
    .bind(at_unix)
    .bind(detail)
    .bind(Some(meter_key.to_owned()))
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
