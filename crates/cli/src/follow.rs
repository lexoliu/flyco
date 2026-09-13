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
            match incoming {
                Some(Ok(event)) => {
                    if let Some(id) = event.id().and_then(|raw| raw.parse().ok()) {
                        self.buffer_cursor = Some(id);
                    }
                    let envelope: SessionEvent = event.data().map_err(|error| {
                        Failure::transport(format!("an event failed to parse: {error}"))
                    })?;
                    self.deliver(envelope).await?;
                }
                // The stream ended, errored, or went quiet: drop it and
                // reconnect — the buffer cursor carries the position.
                _ => self.stream = None,
            }
        }
    }

    /// Opens the stream at the buffer cursor.
    async fn connect(&mut self) -> Outcome<()> {
        use core::fmt::Write as _;
        let mut path = format!("/v1/events?session={}", self.session);
        if let Some(after) = self.buffer_cursor {
            write!(path, "&after={after}").expect("a String cannot refuse a write");
        }
        self.stream = Some(self.api.sse(&path).await?);
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
            loop {
                for stored in page.events {
                    if stored.seq <= self.delivered_seq || stored.seq >= seq {
                        continue;
                    }
                    self.delivered_seq = stored.seq;
                    self.pending.push_back(Item::Replayed(stored));
                }
                if !page.more {
                    break;
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
