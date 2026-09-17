//! The reconcile watermark on `budgets`.
//!
//! `view` is the read behind every session-detail route; these tests count
//! the statements it issues to prove a ledger that has not moved costs a
//! read zero writes.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use flyco_core::{BudgetId, BudgetStage, SessionId, SpendKind, Usd};
use skyzen::sql;
use skyzen_services::{
    BatchStatement, Db, DbBackend, DbDialect, DbError, DbExecResult, DbTransaction, DbValue, Row,
};

use crate::budgets;
use crate::testing::{migrate, seed_session, seed_user};

/// A [`DbBackend`] that counts the statements that write.
///
/// `query` serves row-returning reads; `execute` and `execute_batch` are the
/// only paths a write can take, so a read that issues no write leaves the
/// counter at zero.
#[derive(Clone)]
struct CountingDb {
    inner: Db,
    writes: Arc<AtomicUsize>,
}

/// An in-memory database whose writes are counted, and the counter.
async fn counted_db() -> (Db, Arc<AtomicUsize>) {
    let inner = Db::connect_sqlite_memory()
        .await
        .expect("an in-memory database");
    let writes = Arc::new(AtomicUsize::new(0));
    (
        Db::new(CountingDb {
            inner,
            writes: writes.clone(),
        }),
        writes,
    )
}

impl DbBackend for CountingDb {
    fn dialect(&self) -> DbDialect {
        self.inner.dialect()
    }

    async fn query(&self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DbError> {
        let mut statement = self.inner.query(query);
        for param in params {
            statement = statement.bind(param.clone());
        }
        let rows = statement.fetch_all::<Row>().await?;
        Ok(DbExecResult {
            rows_read: u64::try_from(rows.len()).expect("a row count fits in u64"),
            rows: rows.into_iter().map(Row::into_value).collect(),
            rows_written: 0,
        })
    }

    async fn execute(&self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DbError> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        let mut statement = self.inner.query(query);
        for param in params {
            statement = statement.bind(param.clone());
        }
        statement.execute().await
    }

    async fn begin(&self) -> Result<DbTransaction, DbError> {
        self.inner.begin().await
    }

    async fn execute_batch(
        &self,
        statements: Vec<BatchStatement>,
    ) -> Result<Vec<DbExecResult>, DbError> {
        self.writes.fetch_add(statements.len(), Ordering::Relaxed);
        self.inner.execute_batch(statements).await
    }
}

async fn budget_of(db: &Db, session: SessionId) -> BudgetId {
    sql!(db, "SELECT budget_id FROM sessions WHERE id = {session}")
        .fetch_scalar()
        .await
        .expect("the session's budget")
}

#[skyzen::test]
async fn a_view_on_an_unchanged_ledger_writes_nothing() {
    let (db, writes) = counted_db().await;
    migrate(&db).await;
    let user = seed_user(&db).await;
    let session = seed_session(&db, &user).await;
    let budget = budget_of(&db, session).await;

    budgets::record(
        &db,
        budget,
        SpendKind::Compute,
        Usd::from_dollars(6),
        "machine time",
    )
    .await
    .expect("record spend");

    let first = budgets::view(&db, budget).await.expect("the first view");
    assert_eq!(first.spent, Usd::from_dollars(6));
    assert_eq!(first.stage, BudgetStage::Notice50);

    writes.store(0, Ordering::Relaxed);
    let second = budgets::view(&db, budget).await.expect("the cached view");
    assert_eq!(second, first);
    assert_eq!(
        writes.load(Ordering::Relaxed),
        0,
        "a ledger that has not moved folds to no write"
    );
}

#[skyzen::test]
async fn a_new_ledger_row_is_folded_by_the_next_view() {
    let (db, writes) = counted_db().await;
    migrate(&db).await;
    let user = seed_user(&db).await;
    let session = seed_session(&db, &user).await;
    let budget = budget_of(&db, session).await;

    budgets::record(
        &db,
        budget,
        SpendKind::Compute,
        Usd::from_dollars(6),
        "machine time",
    )
    .await
    .expect("record spend");
    budgets::view(&db, budget)
        .await
        .expect("fold the first read");

    // A row appended without a reconcile — what the metering sweep's batch
    // leaves behind between its insert and the fold that follows it.
    db.execute_batch(vec![budgets::metered_statement(
        budget,
        SpendKind::Storage,
        Usd::from_dollars(1),
        "disk",
        60,
        "meter:test",
    )])
    .await
    .expect("append a ledger row");

    let folded = budgets::view(&db, budget)
        .await
        .expect("the next view folds it");
    assert_eq!(folded.spent, Usd::from_dollars(7));

    writes.store(0, Ordering::Relaxed);
    budgets::view(&db, budget).await.expect("the cached view");
    assert_eq!(writes.load(Ordering::Relaxed), 0);
}

#[skyzen::test]
async fn raising_the_limit_replays_and_prunes_spent_signals() {
    let (db, _writes) = counted_db().await;
    migrate(&db).await;
    let user = seed_user(&db).await;
    let session = seed_session(&db, &user).await;
    let budget = budget_of(&db, session).await;

    budgets::record(
        &db,
        budget,
        SpendKind::Compute,
        Usd::from_dollars(10),
        "machine time",
    )
    .await
    .expect("spend the whole budget");
    assert_eq!(
        budgets::view(&db, budget).await.expect("exhausted").stage,
        BudgetStage::Exhausted
    );
    assert!(
        !budgets::pending_signals(&db)
            .await
            .expect("outbox")
            .is_empty(),
        "exhaustion queued the pause"
    );

    // $10 of $25 is below every threshold: the raised limit is a re-reading
    // of the same ledger, so the paused signal no longer describes the
    // budget and goes back out of the outbox.
    let raised = budgets::set_limit(&db, budget, Usd::from_dollars(25))
        .await
        .expect("raise the limit");
    assert_eq!(raised.stage, BudgetStage::Ok);
    assert_eq!(raised.spent, Usd::from_dollars(10));
    assert!(
        budgets::pending_signals(&db)
            .await
            .expect("outbox")
            .is_empty(),
        "the un-crossed thresholds left the outbox"
    );
}
