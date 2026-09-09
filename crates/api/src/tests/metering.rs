use flyco_core::{
    BudgetId, BudgetSignal, BudgetStage, CloudProviderKind, MachineSpec, MachineState, Runtime,
    SessionState, SpendKind, Usd,
};
use skyzen::sql;
use skyzen_services::Db;
use skyzen_test::TestContext;

use crate::budgets;
use crate::metering;
use crate::rooms::{NativeRooms, Rooms};
use crate::testing::{migrate, seed_provider_account, seed_session, seed_user};
use crate::{machines, sessions};

#[derive(Debug, skyzen::FromRow)]
struct LedgerRow {
    kind: SpendKind,
    amount_micros: Usd,
}

#[skyzen::test]
async fn scheduled_metering_records_compute_and_storage_once_and_pauses(_ctx: TestContext, db: Db) {
    migrate(&db).await;
    let user = seed_user(&db).await;
    let session = seed_session(&db, &user).await;
    let account = seed_provider_account(&db, user.id).await;
    let machine = machines::reserve(
        &db,
        session,
        account,
        &MachineSpec {
            provider: CloudProviderKind::Host,
            machine_type: "build.lexo.cool".to_owned(),
            runtime: Runtime::Container,
            region: "build.lexo.cool".to_owned(),
            spot: false,
            disk_gib: 64,
        },
    )
    .await
    .expect("reserve machine");
    let active = SessionState::Active;
    let running = MachineState::Running;
    let origin = 100_u64;
    sql!(
        db,
        "UPDATE sessions SET state = {active} WHERE id = {session}"
    )
    .execute()
    .await
    .expect("activate session");
    sql!(
        db,
        "UPDATE machines SET state = {running}, hourly_micros = 9500000, \
         storage_hourly_micros = 500000, compute_meter_started_at_unix = {origin}, \
         compute_metered_at_unix = {origin}, storage_meter_started_at_unix = {origin}, \
         storage_metered_at_unix = {origin} WHERE id = {machine}"
    )
    .execute()
    .await
    .expect("price machine");

    metering::accrue(&db, 3_700).await.expect("accrue one hour");
    metering::accrue(&db, 3_700)
        .await
        .expect("repeat the same cron safely");

    let budget: BudgetId = sql!(db, "SELECT budget_id FROM sessions WHERE id = {session}")
        .fetch_scalar()
        .await
        .expect("budget id");
    let view = budgets::view(&db, budget).await.expect("budget view");
    assert_eq!(view.spent, Usd::from_dollars(10));
    assert_eq!(view.stage, BudgetStage::Exhausted);
    let ledger: Vec<LedgerRow> = sql!(
        db,
        "SELECT kind, amount_micros FROM spend_events WHERE budget_id = {budget} \
         ORDER BY kind"
    )
    .fetch_all()
    .await
    .expect("ledger");
    assert_eq!(ledger.len(), 120, "two one-minute meters for one hour");
    assert_eq!(
        ledger.iter().map(|row| row.amount_micros).sum::<Usd>(),
        Usd::from_dollars(10)
    );
    assert!(ledger.iter().any(|row| row.kind == SpendKind::Compute));
    assert!(ledger.iter().any(|row| row.kind == SpendKind::Storage));
    assert!(
        budgets::pending_signals(&db)
            .await
            .expect("outbox")
            .iter()
            .any(|row| row.signal == BudgetSignal::Pause)
    );

    let rooms = Rooms::from_native(NativeRooms::new());
    metering::deliver(&db, &rooms).await.expect("deliver pause");
    assert_eq!(
        sessions::state_of(&db, user.id, session)
            .await
            .expect("state"),
        SessionState::Paused
    );
    assert!(
        budgets::pending_signals(&db)
            .await
            .expect("empty outbox")
            .is_empty()
    );
}
