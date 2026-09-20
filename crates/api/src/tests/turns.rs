//! `GET /v1/sessions/{id}/turns`'s walk over the room's event stream.
//!
//! The fold's own cases are unit-tested beside it in `crate::turns`; what
//! these pin is the walk's cost — issue #351 billed every event page's
//! 501-row bound whether or not the tail held it, up to eight pages a
//! call. The walk is driven against a real [`SessionRoom`] on storage the
//! test can read back, so the ledger's movement is observed, not assumed.

use core::future::Future;

use flyco_core::wire::EventPage;
use flyco_core::{
    ClientEvent, ContextWindow, HarnessEvent, MessageOrigin, SessionId, UsageReport, Usd,
};
use skyzen::durable::DurableObject as _;
use skyzen::{Method, Request};
use skyzen_services::durable::DurableDb;
use skyzen_test::mock::InMemoryDurableDb;

use crate::error::ApiError;
use crate::room::{EVENT_PAGE_LIMIT, SessionRoom};
use crate::testing::{room_request, rows_billed};
use crate::turns::walk;

/// A session room on storage the test can inspect: the walk under test
/// reads this object's event pages, and the ledger it debits is a table
/// here rather than behind a namespace.
struct Room {
    session: SessionId,
    object: SessionRoom,
    db: InMemoryDurableDb,
}

impl Room {
    async fn open() -> Self {
        Self {
            session: SessionId::generate(),
            object: SessionRoom,
            db: InMemoryDurableDb::in_memory()
                .await
                .expect("an in-memory database"),
        }
    }

    /// One Worker→room call against this room's storage, injected where
    /// the simulator would put it.
    fn request(&self, method: Method, path: &str, body: Option<Vec<u8>>) -> Request {
        let mut request = room_request(self.session, method, path, body);
        request
            .extensions_mut()
            .insert(DurableDb::new(self.db.clone()));
        request
    }

    /// Records one event the way a control-plane broadcast does.
    async fn record(&mut self, event: &ClientEvent) {
        let request = self.request(
            Method::POST,
            "/internal/broadcast",
            Some(serde_json::to_vec(event).expect("serialize")),
        );
        let response = self
            .object
            .fetch()
            .go(request)
            .await
            .expect("the room answered");
        assert_eq!(response.status().as_u16(), 200, "a broadcast is stored");
    }

    /// The page read the walk is handed: `GET /internal/events`.
    ///
    /// The returned future owns its request and router rather than
    /// borrowing the room, so the walk can hold it across pages.
    fn events(&mut self, after: u64) -> impl Future<Output = Result<EventPage, ApiError>> + use<> {
        let router = self.object.fetch();
        let request = self.request(
            Method::GET,
            &format!("/internal/events?after={after}"),
            None,
        );
        async move {
            let response = router.go(request).await.expect("the room answered");
            assert!(
                response.status().is_success(),
                "the room refused a page it should answer"
            );
            let body = response
                .into_body()
                .into_bytes()
                .await
                .expect("a readable body");
            serde_json::from_slice(&body)
                .map_err(|_| ApiError::Room("the room returned no event page".to_owned()))
        }
    }

    /// The rows the room's ledger says it read today.
    async fn spent(&self) -> u64 {
        rows_billed(&DurableDb::new(self.db.clone())).await
    }
}

fn usage() -> UsageReport {
    UsageReport {
        input_tokens: 12,
        output_tokens: 34,
        context: Some(ContextWindow {
            used_tokens: 1_000,
            size_tokens: 200_000,
        }),
        estimated_cost: Some(Usd::from_cents(7)),
    }
}

#[skyzen::test]
async fn a_short_tail_bills_the_rows_the_walk_read() {
    let mut room = Room::open().await;
    // One turn's worth of stream: the message that opened it, its start,
    // and its completion.
    for event in [
        ClientEvent::UserMessage {
            text: "audit the relay".to_owned(),
            origin: MessageOrigin::User,
        },
        ClientEvent::Harness {
            event: HarnessEvent::TurnStarted {
                turn_id: "t-1".to_owned(),
            },
        },
        ClientEvent::Harness {
            event: HarnessEvent::TurnCompleted {
                turn_id: "t-1".to_owned(),
                usage: usage(),
            },
        },
    ] {
        room.record(&event).await;
    }

    let page = walk(0, 10, |after| room.events(after))
        .await
        .expect("a page of turns");

    assert_eq!(page.turns.len(), 1, "the short tail folds to one turn");
    assert!(page.next_cursor.is_none(), "the tail is fully read");
    assert_eq!(
        room.spent().await,
        3,
        "the ledger moved by the rows read, not the page's bound"
    );
}

#[skyzen::test]
async fn a_long_tail_stops_at_the_row_bound_with_a_cursor() {
    let mut room = Room::open().await;
    // A tail a page and a half long: one open turn's unbroken deltas.
    let rows = usize::try_from(EVENT_PAGE_LIMIT).expect("a usize") + 100;
    for _ in 0..rows {
        room.record(&ClientEvent::Harness {
            event: HarnessEvent::AssistantDelta {
                turn_id: "t-1".to_owned(),
                text: "…".to_owned(),
            },
        })
        .await;
    }

    let page = walk(0, 10, |after| room.events(after))
        .await
        .expect("a page of turns");

    assert_eq!(
        page.next_cursor.as_deref(),
        Some("500"),
        "the walk stopped at the row bound, a full page in"
    );
    assert_eq!(
        room.spent().await,
        u64::from(EVENT_PAGE_LIMIT) + 1,
        "one page read, bound plus the lookahead row — not eight pages of bound"
    );
}

/// A room that claims more to read without carrying an event could not
/// advance the cursor; the walk refuses it instead of asking again forever.
#[skyzen::test]
async fn a_page_with_more_and_no_events_is_refused() {
    let mut calls = 0_u32;
    let error = walk(0, 10, |_after| {
        calls += 1;
        core::future::ready(Ok(EventPage {
            events: Vec::new(),
            more: true,
        }))
    })
    .await
    .expect_err("an empty page that claims more");

    assert!(
        matches!(error, ApiError::Room(_)),
        "refused as a room fault, got {error:?}"
    );
    assert_eq!(calls, 1, "the walk stopped at the first such page");
}
