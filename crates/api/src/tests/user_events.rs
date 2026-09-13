//! The per-user event stream, driven the way the Worker drives it.
//!
//! `UserEvents` is exercised through its real `fetch` surface — the
//! publish route every session-room call fans out to and the SSE stream
//! `GET /v1/events` proxies — against skyzen's SQLite-backed
//! [`InMemoryDurableDb`]. What these tests pin down is the buffer's
//! contract: a first connect opens at the live tail, a reconnect resumes
//! strictly after its cursor, and a `?session=` filter narrows the one
//! stream down to one session.

use std::time::Duration;

use flyco_core::wire::{MessageOrigin, SessionEvent};
use flyco_core::{ClientEvent, SessionId, UserId};
use futures_util::StreamExt as _;
use skyzen::durable::DurableObject as _;
use skyzen::http_kit::sse::SseStream;
use skyzen::{Body, Method, Request};
use skyzen_services::durable::DurableDb;
use skyzen_test::mock::InMemoryDurableDb;

use crate::room::{EmittedEvent, HEADER_INTERNAL, INTERNAL};
use crate::user_events::{HEADER_USER, PublishEvents, UserEvents};

/// How long a test waits for the stream to hand an event over.
const PATIENCE: Duration = Duration::from_secs(10);

/// How long a quiet stream is watched before a test accepts that nothing
/// is coming. Two full feed ticks plus margin.
const QUIET: Duration = Duration::from_millis(400);

// ── The harness ──

/// The object, its storage, and the stream a test opened on it.
struct Buffer {
    user: UserId,
    object: UserEvents,
    db: InMemoryDurableDb,
}

impl Buffer {
    async fn open() -> Self {
        Self {
            user: UserId::generate(),
            object: UserEvents,
            db: InMemoryDurableDb::in_memory()
                .await
                .expect("an in-memory database"),
        }
    }

    /// Builds one request the way the Worker would send it.
    fn request(&self, method: Method, path: &str, body: Option<Vec<u8>>) -> Request {
        let mut request = Request::new(body.map_or_else(Body::empty, Body::from));
        *request.method_mut() = method;
        *request.uri_mut() = format!("https://user-events.flyco.invalid{path}")
            .parse()
            .expect("a valid stream URL");
        for (name, value) in [
            (HEADER_INTERNAL, INTERNAL.to_owned()),
            (HEADER_USER, self.user.to_string()),
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
            .insert(DurableDb::new(self.db.clone()));
        request
    }

    /// Publishes one session's emitted events onto the buffer.
    async fn publish(&mut self, session: SessionId, events: &[EmittedEvent]) -> u16 {
        let body = PublishEvents {
            session,
            events: events.to_vec(),
        };
        let response = self
            .object
            .fetch()
            .go(self.request(
                Method::POST,
                "/internal/publish",
                Some(serde_json::to_vec(&body).expect("serialize")),
            ))
            .await
            .expect("the object answered");
        response.status().as_u16()
    }

    /// Opens the stream on a path — `""`, `"?after=2"`, `"?session=…"`.
    async fn stream(&mut self, query: &str) -> SseStream {
        let response = self
            .object
            .fetch()
            .go(self.request(Method::GET, &format!("/internal/stream{query}"), None))
            .await
            .expect("the object answered");
        assert_eq!(response.status().as_u16(), 200, "the stream opened");
        response.into_body().into_sse()
    }

    /// Calls a route with no internal marker: a request that arrived off
    /// the open internet.
    async fn unmarked(&mut self, method: Method, path: &str, body: Option<Vec<u8>>) -> u16 {
        let mut request = self.request(method, path, body);
        request.headers_mut().remove(HEADER_INTERNAL);
        let response = self
            .object
            .fetch()
            .go(request)
            .await
            .expect("the object answered");
        response.status().as_u16()
    }
}

/// Reads the next envelope the stream hands over, or fails on patience.
async fn next(stream: &mut SseStream) -> (Option<String>, SessionEvent) {
    let item = tokio_select_quiet(stream, PATIENCE)
        .await
        .expect("an event arrived in time")
        .expect("the stream is still open")
        .expect("a decodable SSE frame");
    let envelope: SessionEvent = item.data().expect("a session event envelope");
    (item.id().map(str::to_owned), envelope)
}

/// Asserts the stream hands nothing over for a whole quiet window.
async fn expect_quiet(stream: &mut SseStream) {
    if let Some(item) = tokio_select_quiet(stream, QUIET).await {
        panic!("the stream stayed quiet, then produced {item:?}");
    }
}

async fn tokio_select_quiet(
    stream: &mut SseStream,
    within: Duration,
) -> Option<Option<Result<skyzen::http_kit::sse::Event, skyzen::http_kit::sse::ParseError>>> {
    let item = stream.next();
    let quiet = futures_timer::Delay::new(within);
    futures_util::pin_mut!(item, quiet);
    match futures_util::future::select(item, quiet).await {
        futures_util::future::Either::Left((item, _)) => Some(item),
        futures_util::future::Either::Right(_) => None,
    }
}

/// One emitted event carrying the given text, sequenced when the caller
/// is playing a room that recorded it.
fn emitted(seq: Option<u64>, text: &str) -> EmittedEvent {
    EmittedEvent {
        seq,
        event: ClientEvent::UserMessage {
            text: text.to_owned(),
            origin: MessageOrigin::User,
        },
    }
}

// ── Fresh connects and reconnects ──

/// A stream opened without a cursor starts at the live tail: the buffer
/// is a reconnect window, not history, and the past it holds is the
/// session's own `events?after=` pages' job to serve.
#[skyzen::test]
async fn a_fresh_stream_starts_at_the_live_tail() {
    let mut buffer = Buffer::open().await;
    let session = SessionId::generate();
    buffer
        .publish(session, &[emitted(Some(1), "old"), emitted(Some(2), "older")])
        .await;

    let mut stream = buffer.stream("").await;

    buffer
        .publish(session, &[emitted(Some(3), "live")])
        .await;
    let (id, envelope) = next(&mut stream).await;
    assert_eq!(id.as_deref(), Some("3"), "the SSE id is the buffer position");
    assert_eq!(envelope.session, session);
    assert_eq!(envelope.seq, Some(3));
    assert_eq!(
        envelope.event,
        ClientEvent::UserMessage {
            text: "live".to_owned(),
            origin: MessageOrigin::User,
        }
    );
    expect_quiet(&mut stream).await;
}

/// A reconnect resumes strictly after the position it names — nothing it
/// already saw, nothing it skipped.
#[skyzen::test]
async fn a_reconnect_resumes_strictly_after_its_cursor() {
    let mut buffer = Buffer::open().await;
    let session = SessionId::generate();
    buffer
        .publish(
            session,
            &[
                emitted(Some(1), "seen"),
                emitted(Some(2), "missed"),
                emitted(Some(3), "also missed"),
            ],
        )
        .await;

    let mut stream = buffer.stream("?after=1").await;

    let (id, first) = next(&mut stream).await;
    assert_eq!(id.as_deref(), Some("2"));
    assert_eq!(first.seq, Some(2));
    let (id, second) = next(&mut stream).await;
    assert_eq!(id.as_deref(), Some("3"));
    assert_eq!(second.seq, Some(3));
    expect_quiet(&mut stream).await;
}

/// `?session=` narrows the one stream to one session: every other
/// session's rows stay buffered for whoever is following them.
#[skyzen::test]
async fn a_stream_filtered_to_a_session_skips_the_others() {
    let mut buffer = Buffer::open().await;
    let followed = SessionId::generate();
    let other = SessionId::generate();

    let mut stream = buffer.stream(&format!("?session={followed}")).await;

    buffer
        .publish(other, &[emitted(Some(1), "someone else's")])
        .await;
    buffer
        .publish(followed, &[emitted(Some(1), "mine")])
        .await;

    let (id, envelope) = next(&mut stream).await;
    assert_eq!(id.as_deref(), Some("2"), "the buffer position counts every row");
    assert_eq!(envelope.session, followed);
    assert_eq!(
        envelope.event,
        ClientEvent::UserMessage {
            text: "mine".to_owned(),
            origin: MessageOrigin::User,
        }
    );
    expect_quiet(&mut stream).await;
}

/// Rows without a session position — live-only facts the control plane
/// composed — still stream; `seq` is simply absent from their envelopes.
#[skyzen::test]
async fn an_unsequenced_event_streams_without_a_position() {
    let mut buffer = Buffer::open().await;
    let session = SessionId::generate();

    let mut stream = buffer.stream("").await;
    buffer
        .publish(
            session,
            &[EmittedEvent {
                seq: None,
                event: ClientEvent::MachineConnection { connected: false },
            }],
        )
        .await;

    let (_, envelope) = next(&mut stream).await;
    assert_eq!(envelope.seq, None);
    assert_eq!(
        envelope.event,
        ClientEvent::MachineConnection { connected: false }
    );
}

/// A route reached without the Worker's internal marker is refused:
/// these routes answer Worker→object calls, not the internet.
#[skyzen::test]
async fn a_call_without_the_internal_marker_is_refused() {
    let mut buffer = Buffer::open().await;
    assert_eq!(
        buffer
            .unmarked(Method::GET, "/internal/stream", None)
            .await,
        502
    );
    let publish = PublishEvents {
        session: SessionId::generate(),
        events: vec![emitted(Some(1), "smuggled")],
    };
    assert_eq!(
        buffer
            .unmarked(
                Method::POST,
                "/internal/publish",
                Some(serde_json::to_vec(&publish).expect("serialize")),
            )
            .await,
        502
    );
}
