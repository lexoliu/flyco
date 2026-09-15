//! The row-read budget, unit and end to end.
//!
//! `charge_reads` is exercised directly for its ledger arithmetic, and the
//! room's `GET /internal/events` is driven past the cap for the refusal a
//! runaway follower would actually meet: `429`, at a cost of one ledger
//! row per attempt rather than the page it asked for.

use flyco_core::SessionId;
use skyzen::durable::DurableObject as _;
use skyzen::{Body, Method, Request};
use skyzen_services::durable::DurableDb;
use skyzen_test::mock::InMemoryDurableDb;

use crate::error::ApiError;
use crate::room::{HEADER_INTERNAL, HEADER_SESSION, INTERNAL, SessionRoom};
use crate::row_budget::{self, ROWS_PER_DAY, charge_reads};

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

/// The rows today's ledger says the object has read.
async fn spent(db: &DurableDb) -> u64 {
    db.query("SELECT COALESCE(SUM(rows_read), 0) FROM row_budget")
        .fetch_scalar::<u64>()
        .await
        .expect("the ledger reads")
}

#[skyzen::test]
async fn charges_accumulate_against_the_day() {
    let db = object().await;
    charge_reads(&db, 100).await.expect("under the cap");
    charge_reads(&db, 50).await.expect("still under");
    assert_eq!(spent(&db).await, 150, "the ledger summed the charges");
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
    assert_eq!(spent(&db).await, ROWS_PER_DAY + 9);
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
async fn a_room_past_its_budget_answers_429() {
    let session = SessionId::generate();
    let mut object = SessionRoom;
    let backend = InMemoryDurableDb::in_memory()
        .await
        .expect("an in-memory database");
    let read = || {
        let mut request = Request::new(Body::empty());
        *request.method_mut() = Method::GET;
        *request.uri_mut() = "https://session-room.flyco.invalid/internal/events?after=0"
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
        request
            .extensions_mut()
            .insert(DurableDb::new(backend.clone()));
        request
    };

    // Each page bills its 501-row bound and the refusal lands on the call
    // *after* the ledger crosses, so the cap needs one extra attempt past
    // the arithmetic — the count a follower makes of a session in minutes
    // when it has gone wrong.
    let attempts = usize::try_from(ROWS_PER_DAY / 501 + 2).expect("a small count");
    let mut refused = false;
    for _ in 0..attempts {
        let response = object.fetch().go(read()).await.expect("the room answered");
        match response.status().as_u16() {
            200 => {}
            429 => {
                refused = true;
                break;
            }
            status => panic!("an events read answered {status}"),
        }
    }
    assert!(refused, "the room refused once its day was spent");
}
