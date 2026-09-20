//! Following a session's events.
//!
//! One mechanism for the three commands that watch a session: the catch-up
//! side reads `GET /v1/sessions/{id}/events?after=` pages, the live side is
//! `GET /v1/events?session={id}` — the one per-user SSE stream filtered to
//! this session. A reconnect resumes the stream at the `Last-Event-ID` it
//! last saw; a gap in the session's own `seq` — which the reconnect buffer
//! can drop — is filled from the session's recorded history before the
//! consumer sees the newer event.
//!
//! The two sides produce different documents and the difference is kept:
//! a live item is a [`SessionEvent`] envelope, a replayed one is the
//! [`StoredEvent`] row itself, because the room replays variants this build
//! may not know and only the raw value preserves them.

use flyco_core::SessionId;
use flyco_core::wire::{ClientEvent, EventPage, SessionEvent, StoredEvent};
use futures_util::StreamExt as _;

use crate::client::Api;
use crate::{Failure, Outcome};

/// How long the SSE may go without a byte before the follower assumes the
/// connection died silently and reconnects.
///
/// The stream heartbeats a comment line well inside this; a whole window
/// of quiet means the bytes stopped, not that nothing happened.
const SILENCE: core::time::Duration = core::time::Duration::from_secs(60);

/// Shortest wait before a reconnect attempt.
const BACKOFF_MIN: core::time::Duration = core::time::Duration::from_secs(1);

/// Longest wait between reconnect attempts.
const BACKOFF_MAX: core::time::Duration = core::time::Duration::from_secs(60);

/// How long a stream must hold before its drop resets the reconnect
/// ladder.
///
/// A stream that dies moments after opening was never really established —
/// most often the route accepts `GET /v1/events` and closes the body — and
/// its reconnect climbs the ladder rather than re-arming at the floor;
/// without that, a server that hangs up on every attach is re-requested at
/// line rate forever. One that outlived this was a real drop, and a real
/// drop deserves a prompt reconnect. The daemon's `ATTACH_STABLE` rule,
/// `crates/daemon/src/control/wire.rs`.
const STABLE: core::time::Duration = core::time::Duration::from_secs(10);

/// How many `429`s one connect waits out before the refusal is the
/// caller's to report.
const REFUSALS: u32 = 3;

/// The most pages one walk fetches before it calls the walk stuck.
///
/// A server that answers `more` forever — or hands back a cursor that
/// never moves — must cost a bounded number of requests, never an
/// unbounded walk at line rate.
pub const MAX_PAGES: u32 = 200;

/// The wait before the `attempt`-th reconnect.
///
/// Capped exponential without jitter: one CLI process follows a stream for
/// one person, so there is no herd to spread, and the cap keeps a long
/// outage from turning into an hour of silence.
fn backoff(attempt: u32) -> core::time::Duration {
    BACKOFF_MIN
        .saturating_mul(1_u32 << attempt.min(6))
        .min(BACKOFF_MAX)
}

/// The refusal every bounded page walk ends in: the cap was reached, or a
/// page answered `more` without advancing the cursor.
#[must_use]
pub fn paging_stalled() -> Failure {
    Failure::transport("the control plane kept paging without advancing")
}

/// One item off a session's event flow.
///
/// A consumer reading `.event()` gets a typed [`ClientEvent`] in both
/// cases; a consumer serializing the item gets the API's own document —
/// the envelope live, the stored row on replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// A live event off the SSE stream.
    Live(SessionEvent),
    /// A row of recorded history, filling a gap the stream left.
    Replayed(StoredEvent),
}

impl Item {
    /// The event, typed.
    ///
    /// `None` on a replayed row carrying a variant this build does not
    /// know — the room stores the JSON verbatim for exactly that case, and
    /// the raw document is still in the item for any consumer that prints.
    #[must_use]
    pub fn event(&self) -> Option<ClientEvent> {
        match self {
            Self::Live(envelope) => Some(envelope.event.clone()),
            Self::Replayed(stored) => {
                serde_json::from_value::<ClientEvent>(stored.event.clone()).ok()
            }
        }
    }

    /// The session-side sequence position, when the event has one.
    #[must_use]
    pub const fn session_seq(&self) -> Option<u64> {
        match self {
            Self::Live(envelope) => envelope.seq,
            Self::Replayed(stored) => Some(stored.seq),
        }
    }
}

/// A live tail of one session's events.
///
/// `next()` yields items in session order, catching up through the
/// recorded history whenever a gap appears — whether from a reconnect or
/// a sequence the buffer dropped. The stream only ends on error: a
/// session's events are infinite by definition.
pub struct Follow<'a> {
    api: &'a Api,
    /// The session being followed; filters the user stream and names the
    /// catch-up route.
    session: SessionId,
    /// The open SSE connection, between connects.
    stream: Option<zenwave::sse::SseStream>,
    /// Position in the *user-events buffer* — the SSE `id:` — handed back
    /// as `?after=` on reconnect.
    buffer_cursor: Option<u64>,
    /// The last session-`seq` handed over. `0` is "none yet": session seqs
    /// are 1-based and live-only events have none.
    delivered_seq: u64,
    /// Items a gap-fill produced, delivered before the live stream resumes.
    pending: std::collections::VecDeque<Item>,
    /// Where the reconnect ladder stands: how many drops have been backed
    /// off in a row. A stream that held [`STABLE`] resets it.
    attempt: u32,
    /// When the current stream opened, for the [`STABLE`] test.
    opened_at: Option<tokio::time::Instant>,
}

impl core::fmt::Debug for Follow<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Follow")
            .field("session", &self.session)
            .field("buffer_cursor", &self.buffer_cursor)
            .field("delivered_seq", &self.delivered_seq)
            .finish_non_exhaustive()
    }
}

impl<'a> Follow<'a> {
    /// A follower that starts at the buffer's head.
    ///
    /// `after=0` replays the whole retained buffer, not just what comes
    /// next: anything published between the caller's last request and the
    /// stream opening lands in the replay rather than racing past it.
    /// Recorded events the caller already paged are dropped on their `seq`,
    /// so replaying what was already seen is free.
    #[must_use]
    pub const fn new(api: &'a Api, session: SessionId) -> Self {
        Self {
            api,
            session,
            stream: None,
            buffer_cursor: Some(0),
            delivered_seq: 0,
            pending: std::collections::VecDeque::new(),
            attempt: 0,
            opened_at: None,
        }
    }

    /// A follower whose gap detection starts after `seq`, for a caller
    /// that already paged the earlier events itself.
    #[must_use]
    pub const fn after(mut self, seq: u64) -> Self {
        self.delivered_seq = seq;
        self
    }

    /// The next item, forever.
    ///
    /// Reconnects silently: the only errors a caller sees are the ones no
    /// reconnect can fix.
    ///
    /// # Errors
    ///
    /// Returns [`Failure`](crate::Failure) when the stream cannot be re-established,
    /// a catch-up page fails, or an event cannot be parsed.
    ///
    /// # Panics
    ///
    /// Never: `self.stream` is `Some` past `connect`, by construction.
    pub async fn next(&mut self) -> Outcome<Item> {
        loop {
            if let Some(item) = self.pending.pop_front() {
                return Ok(item);
            }
            if self.stream.is_none() {
                self.connect().await?;
            }
            let incoming = {
                let stream = self.stream.as_mut().expect("just connected");
                tokio::time::timeout(SILENCE, stream.next())
                    .await
                    .unwrap_or_default()
            };
            if let Some(Ok(event)) = incoming {
                if let Some(id) = event.id().and_then(|raw| raw.parse().ok()) {
                    self.buffer_cursor = Some(id);
                }
                let envelope: SessionEvent = event.data().map_err(|error| {
                    Failure::transport(format!("an event failed to parse: {error}"))
                })?;
                self.deliver(envelope).await?;
            } else {
                // The stream ended, errored, or went quiet: drop it, climb
                // the reconnect ladder, and open again — the buffer cursor
                // carries the position.
                self.stream = None;
                if self
                    .opened_at
                    .take()
                    .is_some_and(|at| at.elapsed() >= STABLE)
                {
                    self.attempt = 0;
                }
                let wait = backoff(self.attempt);
                self.attempt = self.attempt.saturating_add(1);
                tokio::time::sleep(wait).await;
            }
        }
    }

    /// Opens the stream at the buffer cursor.
    ///
    /// A `429` names its wait: a `Retry-After` inside
    /// [`crate::client::MAX_RETRY_AFTER`] is slept out and retried, at most
    /// [`REFUSALS`] times in a row — past that, or on a longer wait, the
    /// failure propagates for the caller to report.
    async fn connect(&mut self) -> Outcome<()> {
        use core::fmt::Write as _;
        let mut path = format!("/v1/events?session={}", self.session);
        if let Some(after) = self.buffer_cursor {
            write!(path, "&after={after}").expect("a String cannot refuse a write");
        }
        let mut refused = 0_u32;
        let stream = loop {
            match self.api.sse(&path).await {
                Ok(stream) => break stream,
                Err(failure) => {
                    let wait = failure
                        .retry_after
                        .filter(|wait| *wait <= crate::client::MAX_RETRY_AFTER);
                    match wait {
                        Some(wait) if refused < REFUSALS => {
                            refused += 1;
                            tokio::time::sleep(wait).await;
                        }
                        _ => return Err(failure),
                    }
                }
            }
        };
        self.opened_at = Some(tokio::time::Instant::now());
        self.stream = Some(stream);
        Ok(())
    }

    /// Queues an envelope for delivery, filling any gap in the session's
    /// `seq` from recorded history first.
    ///
    /// The buffer replays from the head on every connect, so an envelope
    /// at or below `delivered_seq` is one the caller already has — its own
    /// catch-up page, or a replayed row the gap-fill just delivered — and
    /// is dropped.
    async fn deliver(&mut self, envelope: SessionEvent) -> Outcome<()> {
        let Some(seq) = envelope.seq else {
            // Live-only: no position to gap against.
            self.pending.push_back(Item::Live(envelope));
            return Ok(());
        };
        if seq <= self.delivered_seq {
            return Ok(());
        }
        if seq > self.delivered_seq + 1 {
            let mut page = events(self.api, self.session, self.delivered_seq).await?;
            // The walk ends at the live envelope's own position — events at
            // or past it are already on the wire — not at the tail of the
            // recorded history, which on a long session never arrives inside
            // the gap and would otherwise page the same window forever.
            let mut paged = 0_u32;
            'pages: loop {
                let before = self.delivered_seq;
                for stored in page.events {
                    if stored.seq <= self.delivered_seq {
                        continue;
                    }
                    if stored.seq >= seq {
                        break 'pages;
                    }
                    self.delivered_seq = stored.seq;
                    self.pending.push_back(Item::Replayed(stored));
                }
                if !page.more {
                    break;
                }
                // A `more` page that moved nothing — and a walk past the
                // page cap — is a server paging forever, not a gap.
                if self.delivered_seq == before {
                    return Err(paging_stalled());
                }
                paged += 1;
                if paged >= MAX_PAGES {
                    return Err(paging_stalled());
                }
                page = events(self.api, self.session, self.delivered_seq).await?;
            }
        }
        self.delivered_seq = seq;
        self.pending.push_back(Item::Live(envelope));
        Ok(())
    }
}

/// One page of a session's recorded events, strictly after `seq`.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when the stream cannot be re-established,
/// a catch-up page fails, or an event cannot be parsed.
pub async fn events(api: &Api, session: SessionId, after: u64) -> Outcome<EventPage> {
    api.get(&format!("/v1/sessions/{session}/events?after={after}"))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::time::Instant;

    /// The request log every stub writes: the instant each request
    /// arrived — the paused clock's instant under `start_paused`, so a
    /// test measures the waits between requests rather than guessing them.
    type Requests = Arc<Mutex<Vec<Instant>>>;

    /// Answers the n-th request with `respond(n)`, a raw HTTP response.
    async fn serve_raw(
        respond: impl Fn(usize) -> String + Send + Sync + 'static,
    ) -> (url::Url, Requests) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a stub server");
        let port = listener.local_addr().expect("a bound port").port();
        let requests: Requests = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&requests);
        tokio::spawn(async move {
            let mut n = 0_usize;
            while let Ok((mut socket, _)) = listener.accept().await {
                log.lock().expect("the request log").push(Instant::now());
                let answer = respond(n);
                n += 1;
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut chunk = [0_u8; 1024];
                    while !request.windows(4).any(|end| end == b"\r\n\r\n") {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(read) => request.extend_from_slice(&chunk[..read]),
                        }
                    }
                    let _ = socket.write_all(answer.as_bytes()).await;
                });
            }
        });
        (
            url::Url::parse(&format!("http://127.0.0.1:{port}")).expect("a URL"),
            requests,
        )
    }

    /// Answers every `GET` with the same canned page.
    async fn serve(page: EventPage) -> (url::Url, Requests) {
        let body = serde_json::to_string(&page).expect("a page serializes");
        serve_raw(move |_| {
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
        })
        .await
    }

    /// A `200 text/event-stream` answer carrying `body` — an empty one
    /// opens the stream and closes it at once.
    fn sse_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// A `429` naming `retry_after` seconds, with a problem document.
    fn refused_response(retry_after: u64) -> String {
        let body = serde_json::json!({
            "type": "https://flyco.dev/problems/request-budget-exhausted",
            "title": "request budget exhausted",
            "status": 429,
        })
        .to_string();
        format!(
            "HTTP/1.1 429 Too Many Requests\r\n\
             content-type: application/problem+json\r\n\
             retry-after: {retry_after}\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// Follows `session` forever, answering the first failure `next()`
    /// sees — which lets a test tell "reconnecting" from "gave up".
    fn drive(api: Api, session: SessionId) -> tokio::task::JoinHandle<Failure> {
        tokio::spawn(async move {
            let mut follow = Follow::new(&api, session);
            loop {
                if let Err(failure) = follow.next().await {
                    return failure;
                }
            }
        })
    }

    /// Advances the paused clock in small steps until `requests` logs its
    /// n-th arrival — the follower sleeps, wakes, and asks; the stub
    /// answers — giving each task its scheduler turns between steps.
    async fn until(requests: &Requests, n: usize) {
        for _ in 0..400 {
            if requests.lock().expect("the request log").len() >= n {
                return;
            }
            tokio::time::advance(core::time::Duration::from_millis(50)).await;
            for _ in 0..50 {
                tokio::task::yield_now().await;
            }
        }
        panic!("request {n} never arrived");
    }

    /// A live envelope whose `seq` lands inside the page the gap-fill
    /// fetched used to loop forever: every row at or past the live
    /// position was skipped without moving the cursor, so `more` staying
    /// true re-fetched the same window. The fill must stop where the live
    /// stream takes over — one page, three replays, then the envelope.
    #[tokio::test]
    async fn gap_fill_stops_at_the_live_position() {
        let session = SessionId::generate();
        let page = EventPage {
            events: (5..=10)
                .map(|seq| StoredEvent {
                    seq,
                    event: serde_json::json!({"kind": "anything"}),
                    at_unix: 0,
                })
                .collect(),
            more: true,
        };
        let (base, asked) = serve(page).await;
        let api = Api::new(base, None);
        let mut follow = Follow::new(&api, session).after(4);
        tokio::time::timeout(
            core::time::Duration::from_secs(10),
            follow.deliver(SessionEvent {
                session,
                seq: Some(8),
                at_unix: 0,
                event: ClientEvent::Started {
                    harness_session_id: "h".to_owned(),
                },
            }),
        )
        .await
        .expect("the gap-fill must end")
        .expect("the page is well-formed");
        assert_eq!(follow.delivered_seq, 8);
        assert_eq!(asked.lock().expect("the request log").len(), 1);
        let seqs: Vec<Option<u64>> = follow
            .pending
            .iter()
            .map(super::Item::session_seq)
            .collect();
        assert_eq!(seqs, [Some(5), Some(6), Some(7), Some(8)]);
    }

    /// A server that accepts `GET /v1/events` and closes the body at once
    /// must not be re-requested at line rate: every dead stream climbs the
    /// ladder — the second request waits out `BACKOFF_MIN`, the fourth a
    /// 4-second rung — and nothing ever held `STABLE` to reset it.
    #[tokio::test(start_paused = true)]
    async fn reconnects_climb_the_backoff_ladder() {
        let (base, requests) = serve_raw(|_| sse_response("")).await;
        let api = Api::new(base, None);
        let driver = drive(api, SessionId::generate());
        until(&requests, 4).await;
        assert!(!driver.is_finished(), "a dead stream is not a failure");
        driver.abort();

        let times = requests.lock().expect("the request log").clone();
        assert!(times.len() >= 4);
        let gaps: Vec<core::time::Duration> = times
            .windows(2)
            .map(|pair| pair[1].duration_since(pair[0]))
            .collect();
        for (rung, gap) in [1_u64, 2, 4].iter().zip(&gaps) {
            let rung = core::time::Duration::from_secs(*rung);
            assert!(
                *gap >= rung && *gap < rung + core::time::Duration::from_secs(1),
                "gap {gap:?} missed its {rung:?} rung",
            );
        }
    }

    /// A `429` carrying `Retry-After` is a wait, not a failure: the
    /// reconnect lands after the named seconds and the caller sees no
    /// error. The stream that then opens and dies climbs the ladder on
    /// its own terms — `BACKOFF_MIN`, not another named wait.
    #[tokio::test(start_paused = true)]
    async fn retry_after_is_slept_out() {
        let (base, requests) = serve_raw(|n| {
            if n == 0 {
                refused_response(2)
            } else {
                sse_response("")
            }
        })
        .await;
        let api = Api::new(base, None);
        let driver = drive(api, SessionId::generate());
        until(&requests, 3).await;
        assert!(!driver.is_finished(), "the 429 must not surface");
        driver.abort();

        let times = requests.lock().expect("the request log").clone();
        let retry = times[1].duration_since(times[0]);
        assert!(
            retry >= core::time::Duration::from_secs(2)
                && retry < core::time::Duration::from_secs(4),
            "the 429's wait was {retry:?}",
        );
        let gap = times[2].duration_since(times[1]);
        assert!(gap >= BACKOFF_MIN, "the dead stream's wait was {gap:?}");
    }

    /// A `Retry-After` past `MAX_RETRY_AFTER` is the daily budget spent:
    /// `next()` hands the caller the problem document and the named wait
    /// instead of sleeping it out — one request, no sleeps. Real time: the
    /// failure is immediate, and a paused clock would auto-advance past the
    /// request timeout while the socket was still in flight.
    #[tokio::test]
    async fn budget_exhausted_propagates() {
        let (base, requests) = serve_raw(|_| refused_response(3600)).await;
        let api = Api::new(base, None);
        let mut follow = Follow::new(&api, SessionId::generate());
        let failure = follow
            .next()
            .await
            .expect_err("a 3600s wait is not slept out");
        assert_eq!(failure.code, crate::Exit::Problem, "{}", failure.text);
        assert_eq!(
            failure.retry_after,
            Some(core::time::Duration::from_secs(3600))
        );
        assert_eq!(requests.lock().expect("the request log").len(), 1);
    }

    /// A page that keeps answering `more` while repeating the same window
    /// is a server paging forever: the gap-fill fails with a transport
    /// failure inside `MAX_PAGES` requests — here on the second, the one
    /// that moved nothing.
    #[tokio::test]
    async fn gap_fill_fails_when_pages_stop_advancing() {
        let session = SessionId::generate();
        let page = EventPage {
            events: (5..=10)
                .map(|seq| StoredEvent {
                    seq,
                    event: serde_json::json!({"kind": "anything"}),
                    at_unix: 0,
                })
                .collect(),
            more: true,
        };
        let (base, requests) = serve(page).await;
        let api = Api::new(base, None);
        let mut follow = Follow::new(&api, session).after(4);
        let failure = follow
            .deliver(SessionEvent {
                session,
                seq: Some(100),
                at_unix: 0,
                event: ClientEvent::Started {
                    harness_session_id: "h".to_owned(),
                },
            })
            .await
            .expect_err("a window that never advances is a stall");
        assert_eq!(failure.code, crate::Exit::Transport);
        assert_eq!(requests.lock().expect("the request log").len(), 2);
    }
}
