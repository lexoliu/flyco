//! The row-read budget, unit and end to end.
//!
//! `check_budget`/`debit_reads`/`charge_reads` are exercised directly for
//! their ledger arithmetic, and the room's `GET /internal/events` for the
//! contract issue #351 pinned: an empty page leaves the ledger unwritten,
//! a page of N rows debits N, and a spent room is refused for the ledger's
//! one row rather than the page it asked for.

use flyco_core::{ClientEvent, MessageOrigin, SessionId};
use skyzen::durable::DurableObject as _;
use skyzen::{Body, Method, Request};
use skyzen_services::durable::DurableDb;
use skyzen_test::mock::InMemoryDurableDb;

use crate::error::ApiError;
use crate::room::{HEADER_INTERNAL, HEADER_SESSION, INTERNAL, SessionRoom};
use crate::row_budget::{self, ROWS_PER_DAY, charge_reads};
use crate::testing::rows_billed;

/// One object with its ledger schema applied.
async fn object() -> DurableDb {
    let db = DurableDb::new(
        InMemoryDurableDb::in_memory()
            .await
            .expect("an in-memory database"),
    );
    db.query(row_budget::SCHEMA)
        .execute()
        .await
        .expect("the ledger schema applies");
    db
}

/// One Worker→room call the way `rooms.rs` builds it: the internal and
/// session headers, and the object's storage injected where the simulator
/// would put it.
fn call(
    backend: &InMemoryDurableDb,
    session: SessionId,
    method: Method,
    path: &str,
    body: Option<Vec<u8>>,
) -> Request {
    let mut request = Request::new(body.map_or_else(Body::empty, Body::from));
    *request.method_mut() = method;
    *request.uri_mut() = format!("https://session-room.flyco.invalid{path}")
        .parse()
        .expect("a valid room URL");
    for (name, value) in [
        (HEADER_INTERNAL, INTERNAL.to_owned()),
        (HEADER_SESSION, session.to_string()),
    ] {
        request
            .headers_mut()
            .insert(name, value.parse().expect("a valid header"));
    }
    request.headers_mut().insert(
        skyzen::header::CONTENT_TYPE,
        skyzen::header::HeaderValue::from_static("application/json"),
    );
    request
        .extensions_mut()
        .insert(DurableDb::new(backend.clone()));
    request
}

#[skyzen::test]
async fn charges_accumulate_against_the_day() {
    let db = object().await;
    charge_reads(&db, 100).await.expect("under the cap");
    charge_reads(&db, 50).await.expect("still under");
    assert_eq!(rows_billed(&db).await, 150, "the ledger summed the charges");
}

#[skyzen::test]
async fn a_spent_day_refuses() {
    let db = object().await;
    charge_reads(&db, ROWS_PER_DAY)
        .await
        .expect("the charge that reaches the cap is allowed");
    let error = charge_reads(&db, 1)
        .await
        .expect_err("the next charge is refused");
    assert!(matches!(error, ApiError::RowBudgetExceeded));
}

#[skyzen::test]
async fn the_charge_that_crosses_the_cap_is_billed_once() {
    let db = object().await;
    charge_reads(&db, ROWS_PER_DAY - 1)
        .await
        .expect("under the cap");
    charge_reads(&db, 10)
        .await
        .expect("the crossing charge lands; the refusal comes next");
    assert_eq!(rows_billed(&db).await, ROWS_PER_DAY + 9);
    assert!(matches!(
        charge_reads(&db, 1).await.expect_err("over the cap"),
        ApiError::RowBudgetExceeded
    ));
}

#[skyzen::test]
async fn one_objects_budget_leaves_anothers_alone() {
    let broke = object().await;
    let fine = object().await;
    charge_reads(&broke, ROWS_PER_DAY)
        .await
        .expect("the charge that reaches the cap is allowed");
    assert!(charge_reads(&broke, 1).await.is_err());
    charge_reads(&fine, 1)
        .await
        .expect("a neighbour object's ledger is untouched");
}

#[skyzen::test]
async fn an_empty_events_page_leaves_the_ledger_unwritten() {
    let session = SessionId::generate();
    let mut object = SessionRoom;
    let backend = InMemoryDurableDb::in_memory()
        .await
        .expect("an in-memory database");

    let response = object
        .fetch()
        .go(call(
            &backend,
            session,
            Method::GET,
            "/internal/events?after=0",
            None,
        ))
        .await
        .expect("the room answered");
    assert_eq!(response.status().as_u16(), 200);

    // The refusal check costs one read of the ledger; with nothing in the
    // tail there is nothing to debit, so the ledger holds no row at all.
    let db = DurableDb::new(backend);
    let rows: u64 = db
        .query("SELECT COUNT(*) FROM row_budget")
        .fetch_scalar()
        .await
        .expect("the ledger reads");
    assert_eq!(rows, 0, "an empty page wrote nothing to the ledger");
}

#[skyzen::test]
async fn an_events_page_is_billed_the_rows_it_returned() {
    let session = SessionId::generate();
    let mut object = SessionRoom;
    let backend = InMemoryDurableDb::in_memory()
        .await
        .expect("an in-memory database");

    // Three events in the tail — a page whose bound is 501 rows but whose
    // read is three.
    for index in 0..3_u32 {
        let event = ClientEvent::UserMessage {
            text: format!("message {index}"),
            origin: MessageOrigin::User,
        };
        let response = object
            .fetch()
            .go(call(
                &backend,
                session,
                Method::POST,
                "/internal/broadcast",
                Some(serde_json::to_vec(&event).expect("serialize")),
            ))
            .await
            .expect("the room answered");
        assert_eq!(response.status().as_u16(), 200, "a broadcast is stored");
    }

    let response = object
        .fetch()
        .go(call(
            &backend,
            session,
            Method::GET,
            "/internal/events?after=0",
            None,
        ))
        .await
        .expect("the room answered");
    assert_eq!(response.status().as_u16(), 200);

    let db = DurableDb::new(backend);
    assert_eq!(
        rows_billed(&db).await,
        3,
        "the debit is the rows the page read, not its 501-row bound"
    );
}

#[skyzen::test]
async fn a_room_past_its_budget_answers_429() {
    let session = SessionId::generate();
    let mut object = SessionRoom;
    let backend = InMemoryDurableDb::in_memory()
        .await
        .expect("an in-memory database");
    let db = DurableDb::new(backend.clone());
    db.query(row_budget::SCHEMA)
        .execute()
        .await
        .expect("the ledger schema applies");
    charge_reads(&db, ROWS_PER_DAY)
        .await
        .expect("the charge that reaches the cap is allowed");

    // The refusal lands on the ledger's one row: the day is spent, so the
    // page's select never runs and nothing is debited past the cap.
    let response = object
        .fetch()
        .go(call(
            &backend,
            session,
            Method::GET,
            "/internal/events?after=0",
            None,
        ))
        .await
        .expect("the room answered");
    assert_eq!(response.status().as_u16(), 429);
    assert_eq!(
        rows_billed(&db).await,
        ROWS_PER_DAY,
        "a refused page debits nothing"
    );
}
