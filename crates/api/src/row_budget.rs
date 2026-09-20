//! The per-object row-read budget that keeps one object from spending the
//! whole account's Durable Object quota.
//!
//! Cloudflare bills SQLite row reads at the account level — the free plan
//! allows five million a day across every object in it — and one runaway
//! follower once burned the whole day's allowance in ninety minutes (the
//! gap-fill loop fixed in #288 re-read one 500-row page every 400 ms). The
//! platform offers no per-object cap, so each object carries its own: a
//! `row_budget` ledger row per UTC day that [`charge_reads`] debits. A spent
//! object answers `429` until the day rolls over.
//!
//! The cap is sized far past anything a real workload reaches — it exists
//! to stop a bug, not to shape traffic — so only reads that can touch many
//! rows are billed, and they are billed what they read. [`check_budget`]
//! runs before such a read — a spent object is refused for the ledger's
//! one row rather than the page it asked for — and [`debit_reads`] bills
//! the rows that came back, so an empty page costs the check and nothing
//! else. [`charge_reads`] is the two in one call, for a read whose size
//! only exists in its result — a cursor feed. Keyed lookups and one-row
//! reads are not billed: at a handful of rows each they cannot move the
//! account's total no matter how often a broken caller repeats them.

use skyzen::sql;
use skyzen_services::DurableDb;

use crate::clock::now_unix;
use crate::error::ApiError;

/// The ledger table every durable object's schema carries for
/// [`charge_reads`] to bill against — one row per UTC day.
pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS row_budget (\
     day       INTEGER PRIMARY KEY, \
     rows_read INTEGER NOT NULL)";

/// Rows a single object may read in a UTC day before it starts refusing.
///
/// Sized for a bound no real workload reaches: a page read bills the rows
/// it returned, so the cap is two hundred full transcript pages of one
/// session in a day. The incident that motivated it read 6.7 million rows
/// of one room — sixty times the cap — inside ninety minutes.
pub(crate) const ROWS_PER_DAY: u64 = 100_000;

/// The unit the ledger keys on, in the seconds `now_unix` counts.
const SECONDS_PER_DAY: u64 = 86_400;

/// Reads the object's ledger for today and refuses once it is spent.
///
/// Call it before a read that can touch many rows: a spent object is then
/// refused for the one row this check costs rather than the read it asked
/// for — the case a runaway follower makes hot. What the read returned is
/// debited by [`debit_reads`] after it. The ledger comes from [`SCHEMA`],
/// so the caller's `ensure_schema` must have run first.
///
/// Check and debit are separate statements, never transactional with the
/// read they guard: the budget is a circuit breaker for runaway callers,
/// and the rows that slip past between the two are noise, not correctness.
///
/// # Errors
///
/// Returns [`ApiError::RowBudgetExceeded`] when the day's ledger is already
/// spent, and [`ApiError::Room`] when the ledger itself cannot be read.
pub async fn check_budget(db: &DurableDb) -> Result<(), ApiError> {
    let day = now_unix() / SECONDS_PER_DAY;
    let spent: Option<u64> = sql!(db, "SELECT rows_read FROM row_budget WHERE day = {day}")
        .fetch_scalar_optional()
        .await
        .map_err(|error| ApiError::Room(error.to_string()))?;
    if spent.unwrap_or(0) >= ROWS_PER_DAY {
        return Err(ApiError::RowBudgetExceeded);
    }
    Ok(())
}

/// Debits `rows` against the object's ledger for today.
///
/// Called after a read [`check_budget`] allowed, with the count the
/// platform reported. A read that came back empty debits nothing — and
/// writes nothing, so an idle page costs the check's ledger read alone.
///
/// # Errors
///
/// Returns [`ApiError::Room`] when the ledger cannot be written.
pub async fn debit_reads(db: &DurableDb, rows: u64) -> Result<(), ApiError> {
    if rows == 0 {
        return Ok(());
    }
    let day = now_unix() / SECONDS_PER_DAY;
    sql!(
        db,
        "INSERT INTO row_budget (day, rows_read) VALUES ({day}, {rows}) \
         ON CONFLICT (day) DO UPDATE SET rows_read = rows_read + excluded.rows_read"
    )
    .execute()
    .await
    .map_err(|error| ApiError::Room(error.to_string()))?;
    Ok(())
}

/// Bills `rows` against the object's ledger for today and refuses once it
/// is spent.
///
/// [`check_budget`] and [`debit_reads`] in one call, for the reads whose
/// size is only known in the result. The ledger comes from [`SCHEMA`], so
/// the caller's `ensure_schema` must have run first.
///
/// # Errors
///
/// Returns [`ApiError::RowBudgetExceeded`] when the day's ledger is already
/// spent, and [`ApiError::Room`] when the ledger itself cannot be read or
/// written.
pub async fn charge_reads(db: &DurableDb, rows: u64) -> Result<(), ApiError> {
    check_budget(db).await?;
    debit_reads(db, rows).await
}
