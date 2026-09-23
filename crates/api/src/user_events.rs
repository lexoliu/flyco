//! The user event stream: one Durable Object per user.
//!
//! Everything a person can see arrives here: a session room answers each
//! internal call with the events it produced, and the Worker publishes
//! them onto the owner's stream. `GET /v1/events` is that stream — one
//! SSE connection carrying every session the user owns, multiplexed by
//! the [`SessionEvent`] envelope's `session` field.
//!
//! # Why a second object, and why a table
//!
//! A session room cannot reach the user it belongs to — a Durable Object
//! addresses nothing but itself — and a browser holding one connection
//! per session would pay a stream per open tab. The user object is the
//! fan-in: session rooms stay session-shaped, and this object is the one
//! place a user's whole event flow can be read from. It keeps the same
//! contract the rooms keep: rows, not subscribers, because the object
//! serving a stream shares no memory with the one that answered a
//! publish.
//!
//! # Retention
//!
//! Rows are a live-stream buffer, not the record — the record is each
//! session room's `events` table, which `events?after=` replays. A row
//! here survives long enough to cover a reconnect window and is then
//! swept: a client that comes back past it refills from the session's own
//! history instead.

use flyco_core::SessionId;
use flyco_core::wire::SessionEvent;
use serde::{Deserialize, Serialize};
use skyzen::durable::{DurableObject, DurableObjectError};
use skyzen::extract::Query;
use skyzen::responder::Sse;
use skyzen::responder::sse::Event;
use skyzen::routing::{CreateRouteNode, Route, Router};
use skyzen::sql;
use skyzen::utils::Json;
use skyzen_services::durable::DurableDb;

use crate::ApiError;
use crate::clock::now_unix;
use crate::extract::Headers;
use crate::problem::Outcome;
use crate::respond::NoContent;
use crate::room::{EmittedEvent, HEADER_INTERNAL, INTERNAL};

/// Names the user a Worker→object call belongs to.
pub const HEADER_USER: &str = "x-flyco-user";

/// How long a published event stays on the buffer.
///
/// A reconnect inside the window resumes the stream where it left off;
/// past it, the client refills from the session's own `events?after=`
/// history, which is the authoritative copy.
const EVENT_TTL_SECONDS: u64 = 3600;

/// What the Worker publishes: the events one room call produced, with the
/// session they belong to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
pub struct PublishEvents {
    /// The session the events belong to.
    pub session: SessionId,
    /// The events, in the order the room made them.
    pub events: Vec<EmittedEvent>,
}

/// Query of the stream route.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
pub struct StreamCursor {
    /// Resume strictly after this position in the buffer. The Worker maps
    /// the client's `Last-Event-ID` onto it. Omitted means a first
    /// connect, which starts live: the buffer is a reconnect window, not
    /// history — replaying it would hand the client transitions that are
    /// an hour stale, and the past it wants is the session's own
    /// `events?after=` pages.
    pub after: Option<u64>,
    /// Emit only this session's events, when the caller is following one.
    pub session: Option<SessionId>,
}

/// The columns the `user_events` table stores.
#[derive(Debug, skyzen::FromRow)]
struct EventRow {
    seq: u64,
    /// The [`SessionEvent`] envelope, pre-serialized at publish: the
    /// stream serves it verbatim rather than reserialize a document it
    /// never reads.
    json: String,
}

/// The per-user event stream.
#[derive(Debug, Default, Serialize, Deserialize)]
#[skyzen::durable_object]
pub struct UserEvents;

impl DurableObject for UserEvents {
    fn fetch(&mut self) -> Router {
        Route::new((
            "/internal/publish".post(publish),
            "/internal/stream".at(stream),
        ))
        .build()
    }
}

/// Reads the internal headers a Worker→object call must carry.
fn internal(headers: &Headers) -> Result<(), ApiError> {
    if headers.get(HEADER_INTERNAL) == Some(INTERNAL) {
        Ok(())
    } else {
        Err(ApiError::Room(
            "a user-events route was reached without the internal marker".to_owned(),
        ))
    }
}

/// The schema this build expects.
///
/// The durable answer to "is the schema already there" lives in the
/// `schema_meta` table [`crate::schema_version`] keeps — checking it
/// costs one storage read where running every `CREATE` blind costs one
/// per statement — and this object runs the check on every publish and
/// every stream open. Bump it when the DDL below changes so a buffer
/// built by an older build upgrades once, on its next call.
const SCHEMA_VERSION: i64 = 2;

/// Creates the buffer table if this is the object's first write.
async fn ensure_schema(db: &DurableDb) -> Result<(), DurableObjectError> {
    crate::schema_version::ensure(
        db,
        SCHEMA_VERSION,
        &[
            "CREATE TABLE IF NOT EXISTS user_events (\
                 seq     INTEGER PRIMARY KEY AUTOINCREMENT, \
                 session TEXT    NOT NULL, \
                 json    TEXT    NOT NULL, \
                 at_unix INTEGER NOT NULL)",
            // The TTL sweep in `publish` deletes by `at_unix`; without this
            // index every publish scans the whole buffer.
            "CREATE INDEX IF NOT EXISTS user_events_at ON user_events (at_unix)",
            // The user's row-read ledger — one row per UTC day, debited by
            // `row_budget::charge_reads`, and the circuit breaker that keeps a
            // runaway reader here from spending the account's quota.
            crate::row_budget::SCHEMA,
        ],
    )
    .await
}

/// Appends a call's events to the user's buffer.
///
/// The envelope is serialized once, here: `session` goes in its own
/// column only because a `?session=` filter needs to match on it.
async fn publish(
    headers: Headers,
    Json(body): Json<PublishEvents>,
    db: DurableDb,
) -> Outcome<NoContent> {
    publish_inner(&headers, body, &db).await.into()
}

async fn publish_inner(
    headers: &Headers,
    body: PublishEvents,
    db: &DurableDb,
) -> Result<NoContent, ApiError> {
    internal(headers)?;
    ensure_schema(db)
        .await
        .map_err(|error| room_failed(&error))?;

    let session_id = body.session.to_string();
    let session = session_id.as_str();
    let now = now_unix();
    for emitted in body.events {
        let envelope = SessionEvent {
            session: body.session,
            seq: emitted.seq,
            at_unix: now,
            event: emitted.event,
        };
        let json = serde_json::to_string(&envelope)
            .map_err(|error| ApiError::Room(format!("an event failed to serialize: {error}")))?;
        sql!(
            db,
            "INSERT INTO user_events (session, json, at_unix) VALUES ({session}, {json}, {now})"
        )
        .execute()
        .await
        .map_err(|error| room_failed(&error))?;
    }

    let oldest = now.saturating_sub(EVENT_TTL_SECONDS);
    sql!(db, "DELETE FROM user_events WHERE at_unix < {oldest}")
        .execute()
        .await
        .map_err(|error| room_failed(&error))?;
    Ok(NoContent)
}

/// Serves the user's event buffer as an SSE stream.
async fn stream(
    headers: Headers,
    Query(cursor): Query<StreamCursor>,
    db: DurableDb,
) -> Outcome<Sse> {
    open_stream(&headers, cursor.after, cursor.session, db)
        .await
        .into()
}

async fn open_stream(
    headers: &Headers,
    after: Option<u64>,
    session: Option<SessionId>,
    db: DurableDb,
) -> Result<Sse, ApiError> {
    internal(headers)?;
    ensure_schema(&db)
        .await
        .map_err(|error| room_failed(&error))?;
    let cursor = match after {
        Some(after) => after,
        // The buffer's tail, read in the same call the stream is opened
        // in: anything published after this read lands past it and is
        // delivered live, so the live-only start has no window.
        None => sql!(db, "SELECT COALESCE(MAX(seq), 0) FROM user_events")
            .fetch_scalar::<u64>()
            .await
            .map_err(|error| room_failed(&error))?,
    };
    Ok(crate::sse::serve(
        EventFeed {
            db,
            cursor,
            session,
        },
        poll_event_feed,
        crate::sse::HEARTBEAT,
    ))
}

/// The stream's working state.
struct EventFeed {
    db: DurableDb,
    /// How far down the buffer this stream has handed over.
    cursor: u64,
    /// The one session to emit, when the caller is following one.
    session: Option<SessionId>,
}

/// One poll of the user event stream.
fn poll_event_feed(feed: &mut EventFeed) -> crate::sse::PollFn<'_> {
    Box::pin(async move {
        event_feed_step(feed)
            .await
            .unwrap_or(crate::sse::Poll::Idle)
    })
}

/// One poll step; a failed read is a warning and an idle tick, not a
/// dead stream.
async fn event_feed_step(feed: &mut EventFeed) -> Result<crate::sse::Poll, ()> {
    let cursor = feed.cursor;
    let session = feed.session.map(|id| id.to_string());
    let db = &feed.db;
    let rows: Vec<EventRow> = match session {
        // Both filters read the same two indexes; written twice rather
        // than composed, because a `sql!` parameter cannot express an
        // optional clause.
        Some(session) => sql!(
            db,
            "SELECT seq, json FROM user_events \
             WHERE seq > {cursor} AND session = {session} ORDER BY seq"
        )
        .fetch_all()
        .await
        .map_err(|error| {
            tracing::warn!(%error, "an event stream could not poll its buffer");
        })?,
        None => sql!(
            db,
            "SELECT seq, json FROM user_events WHERE seq > {cursor} ORDER BY seq"
        )
        .fetch_all()
        .await
        .map_err(|error| {
            tracing::warn!(%error, "an event stream could not poll its buffer");
        })?,
    };
    if rows.is_empty() {
        return Ok(crate::sse::Poll::Idle);
    }
    // The poll just drained `rows` of backlog — bill them. An idle poll
    // reads nothing and is billed nothing: a stream's keepalive cadence
    // must not spend the budget a backlog could.
    crate::row_budget::charge_reads(db, rows.len() as u64)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "an event stream hit the row budget");
        })?;
    feed.cursor = rows.last().map_or(feed.cursor, |row| row.seq);
    Ok(crate::sse::Poll::Emit(
        rows.into_iter()
            .map(|row| Event::data(row.json).id(row.seq.to_string()))
            .collect(),
    ))
}

fn room_failed(error: &impl core::fmt::Display) -> ApiError {
    ApiError::Room(error.to_string())
}
