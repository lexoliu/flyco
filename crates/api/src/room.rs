//! The session room: one Durable Object per live session.
//!
//! A session's daemon attaches over REST, holds one SSE stream for the
//! room's [`ControlToDaemon`] commands, and posts its own
//! [`DaemonToControl`] frames back in sequenced batches. Everything a
//! person sees lands in the room's `events` table; the Worker forwards
//! each emitted event to the owner's global stream, so a browser never
//! talks to the room at all.
//!
//! # What the room is, and is not, allowed to touch
//!
//! A Durable Object cannot reach D1 or the Worker's KV. Every check that
//! needs them — does this credential exist, does this user own this
//! session, is this approval already decided — therefore happens in the
//! *Worker* before it forwards anything here. What arrives at the room is
//! already authenticated, and says so in internal headers
//! ([`HEADER_SESSION`], [`HEADER_INTERNAL`]) that only a same-Worker call
//! can set. The room refuses a request without them rather than guessing.
//!
//! # How a command reaches the daemon
//!
//! There is no socket to write to: a deliverable command is a row in
//! `daemon_commands`, and the daemon's SSE stream is a cursor over that
//! table — the stream polls it, hands over every row newer than its
//! cursor, and a daemon acknowledges what it applied with `ack_through`
//! on its next frames POST, which deletes the rows. Delivery is therefore
//! at-least-once by construction: a stream that dies mid-poll leaves the
//! unacknowledged rows for the next attach, and the daemon skips what it
//! already applied.
//!
//! Whether a command becomes a row at all depends on whether a daemon is
//! attached — the presence marker — and on what it is. A user message is
//! *conversation*: it is queued whether or not a daemon is there, exactly
//! as the old mailbox did, and the prompt `POST /v1/sessions` carries
//! reaches the agent minutes before the machine exists by this path. A
//! `!` shell command queued for nobody is answered at once with
//! [`ShellOutcome::Offline`](flyco_core::ShellOutcome::Offline) rather
//! than left waiting. An interrupt, a keystroke, a compaction held for a
//! daemon that reconnects an hour later would arrive as an instruction
//! about a turn that no longer exists, so those are dropped when the
//! marker says nobody is listening.
//!
//! # Presence is a deadline, not a socket
//!
//! `daemon_presence` holds the current attach's epoch and the moment its
//! claim expires. Attach and every frames POST renew it; so does the
//! command stream itself on every poll. A daemon that falls silent stops
//! being renewed, and the next room call that finds the marker stale
//! announces `MachineConnection{connected:false}` once. That is lazier
//! than a close event — a daemon that dies while nobody asks the
//! room anything is announced on the next call rather than at once — and
//! it is the trade a long-lived connection never really offered either:
//! silence is only ever noticed when something tries to use it.
//!
//! # Why the state lives outside the struct
//!
//! [`SessionRoom`] is empty. The room's state is its tables, all of which
//! survive the object being rebuilt around every event. A field would be
//! a second copy of the same facts, re-serialized on every call, and the
//! first one to drift.

use flyco_core::wire::{DaemonAttach, DaemonFrames, EventPage, StoredEvent};
use flyco_core::workdir::WorkdirReply;
use flyco_core::{
    CheckoutStatus, ClientEvent, ControlToDaemon, DaemonToControl, DesktopInputRequest,
    DesktopTakeoverRequest, MessageOrigin, Problem, RepoStatus, ShellRunId, WorkdirRequestId,
};
use serde::{Deserialize, Serialize};
use skyzen::durable::{DurableObject, DurableObjectError};
use skyzen::extract::Query;
use skyzen::responder::Sse;
use skyzen::responder::sse::Event;
use skyzen::routing::{CreateRouteNode, Route, Router};
use skyzen::sql;
use skyzen::utils::Json;
use skyzen_services::durable::{DurableDb, DurableKv};

use crate::ApiError;
use crate::clock::now_unix;
use crate::extract::Headers;
use crate::problem::Outcome;
use crate::respond::NoContent;

/// Names the session a Worker→room call belongs to.
pub const HEADER_SESSION: &str = "x-flyco-session";

/// Marks a Worker→room call as internal rather than relayed.
pub const HEADER_INTERNAL: &str = "x-flyco-internal";

/// Value [`HEADER_INTERNAL`] carries.
pub const INTERNAL: &str = "1";

/// Most events one catch-up page returns.
pub const EVENT_PAGE_LIMIT: u32 = 500;

/// How long a daemon's presence marker survives its last contact.
///
/// Attach, every frames POST, and every poll of its command stream renew
/// it, so expiry means a daemon that has said *nothing* for this long —
/// the only shape a dead or partitioned daemon can take. The command
/// stream refreshes well inside it, so a live daemon never expires.
const PRESENCE_TTL_SECONDS: u64 = 30;

/// How often the command stream renews the presence marker.
///
/// Every poll would write the same row a hundred times inside one TTL;
/// the marker only needs to stay ahead of expiry.
const PRESENCE_REFRESH_SECONDS: u64 = 10;

/// How long a desktop watcher's lease survives its last contact.
///
/// Same shape as [`PRESENCE_TTL_SECONDS`]: the watch stream renews it,
/// and its takeover and input calls count as contact. Expiry is what
/// hands the screen back when a browser dies mid-takeover — the
/// reconcile in the command feed sees the row lapse and tells the
/// daemon the audience is gone.
const WATCHER_TTL_SECONDS: u64 = 30;

/// How often the watch stream renews its watcher lease.
const WATCHER_REFRESH_SECONDS: u64 = 10;

/// How many encoded chunks the desktop tail keeps.
///
/// The stream's store is one GOP tail: a keyframe's insert drops
/// everything before it, so the cap only ever binds a runaway GOP — a
/// screen the encoder cannot cut into keyframes fast enough. At the
/// stream's 5fps cadence it is about a minute of video.
const DESKTOP_CHUNKS_KEPT: u64 = 300;

/// How many chunks one watch-stream poll hands over.
const DESKTOP_CHUNK_BATCH: u64 = 64;

/// One event a room made, as an internal route reports it for fan-out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct EmittedEvent {
    /// Position the event took in the room's replayable stream, when it
    /// was recorded there. Live-only facts — the daemon's presence, an
    /// approval's decision — have no position and never will.
    pub seq: Option<u64>,
    /// The event.
    pub event: ClientEvent,
}

/// What an internal route answers with: the events the call produced, in
/// order, for the Worker to publish onto the session owner's stream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Emitted {
    /// The events, in the order the room made them.
    pub events: Vec<EmittedEvent>,
}

/// What an attach answers with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AttachResponse {
    /// Generation of this attachment; increments per attach.
    pub epoch: u64,
    /// The events the attach produced.
    pub events: Vec<EmittedEvent>,
}

/// Query of the room's catch-up route.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
pub struct EventCursor {
    /// Return events strictly after this position. Omitted starts at the
    /// beginning of the room's history.
    pub after: Option<u64>,
}

/// Query of the room's command stream.
#[derive(Debug, Deserialize, skyzen::ToSchema)]
pub struct CommandCursor {
    /// The attach this stream serves. A stream opened under an epoch that
    /// a later attach superseded is refused rather than fed commands it
    /// would acknowledge against the wrong generation.
    pub epoch: u64,
}

/// The columns the `events` table stores.
#[derive(Debug, skyzen::FromRow)]
struct EventRow {
    seq: u64,
    /// The event, kept as a JSON document in a text column. Untyped for the
    /// same reason [`StoredEvent::event`] is: the room replays what the
    /// daemon sent, including a variant this build does not know.
    #[row(json)]
    json: serde_json::Value,
    at_unix: u64,
}

impl From<EventRow> for StoredEvent {
    fn from(row: EventRow) -> Self {
        Self {
            seq: row.seq,
            event: row.json,
            at_unix: row.at_unix,
        }
    }
}

/// The columns the `daemon_commands` table stores.
#[derive(Debug, skyzen::FromRow)]
struct CommandRow {
    seq: u64,
    /// The command, as a JSON document. Untyped on the way out for the
    /// same reason an event is: the room hands the daemon what it was
    /// given, including a variant this build of the room does not know.
    #[row(json)]
    json: serde_json::Value,
}

/// The `daemon_presence` row.
#[derive(Debug, skyzen::FromRow)]
struct PresenceRow {
    epoch: u64,
    live_until: u64,
    gone_reported: u8,
}

/// The relay room for one session.
#[derive(Debug, Default, Serialize, Deserialize)]
#[skyzen::durable_object]
pub struct SessionRoom;

impl DurableObject for SessionRoom {
    fn fetch(&mut self) -> Router {
        Route::new((
            "/internal/daemon-attach".post(attach_daemon),
            "/internal/commands".at(stream_commands),
            "/internal/frames".post(accept_frames),
            "/internal/command".post(run_command),
            "/internal/broadcast".post(run_broadcast),
            "/internal/events".at(read_events),
            "/internal/repo-status".at(read_repo_status),
            "/internal/workdir".post(ask_workdir),
            "/internal/desktop/watch".at(watch_desktop),
            "/internal/desktop/takeover".post(desktop_takeover),
            "/internal/desktop/input".post(desktop_input),
        ))
        .build()
    }
}

// ── The daemon's three routes ──

/// Attaches a daemon to the room.
///
/// The attach is the handshake: the version check happens here, the epoch
/// minted here names every later frame batch and command stream, and the
/// presence marker the rest of the room reads is written here. A daemon
/// that attached twice — a retry that raced the first attempt's response
/// — supersedes itself: the older epoch's stream ends and its frames are
/// refused.
async fn attach_daemon(
    headers: Headers,
    Json(attach): Json<DaemonAttach>,
    db: DurableDb,
) -> Outcome<Json<AttachResponse>> {
    attach_inner(&headers, &attach, &db).await.into()
}

async fn attach_inner(
    headers: &Headers,
    attach: &DaemonAttach,
    db: &DurableDb,
) -> Result<Json<AttachResponse>, ApiError> {
    internal(headers)?;
    if attach.protocol_version != flyco_core::WIRE_PROTOCOL_VERSION {
        return Err(ApiError::ProtocolMismatch {
            daemon: attach.protocol_version,
            control: flyco_core::WIRE_PROTOCOL_VERSION,
        });
    }
    ensure_schema(db)
        .await
        .map_err(|error| room_failed(&error))?;

    let live_until = now_unix().saturating_add(PRESENCE_TTL_SECONDS);
    sql!(
        db,
        "INSERT INTO daemon_presence (id, epoch, live_until, gone_reported) \
         VALUES (0, 1, {live_until}, 0) \
         ON CONFLICT (id) DO UPDATE SET \
             epoch = daemon_presence.epoch + 1, \
             live_until = excluded.live_until, \
             gone_reported = 0"
    )
    .execute()
    .await
    .map_err(|error| room_failed(&error))?;
    let epoch: u64 = sql!(db, "SELECT epoch FROM daemon_presence WHERE id = 0")
        .fetch_scalar()
        .await
        .map_err(|error| room_failed(&error))?;
    // Frame bookkeeping from a superseded attach can never become valid
    // again — the epoch check refuses it — so it is swept rather than
    // kept.
    sql!(db, "DELETE FROM daemon_frames WHERE epoch != {epoch}")
        .execute()
        .await
        .map_err(|error| room_failed(&error))?;

    tracing::info!(epoch, "a daemon attached to its session room");
    let events = vec![EmittedEvent {
        seq: None,
        event: ClientEvent::MachineConnection { connected: true },
    }];
    Ok(Json(AttachResponse { epoch, events }))
}

/// Holds a daemon's command stream open.
///
/// The stream is a cursor over `daemon_commands`: it emits every row past
/// its cursor, then polls the table for more until the attach it serves
/// is superseded. Storage is the rendezvous because the object serving
/// this request shares no memory with the objects serving any other —
/// the rows are the only thing every activation sees identically.
async fn stream_commands(
    headers: Headers,
    Query(cursor): Query<CommandCursor>,
    db: DurableDb,
) -> Outcome<Sse> {
    open_command_stream(&headers, cursor.epoch, db).await.into()
}

async fn open_command_stream(
    headers: &Headers,
    epoch: u64,
    db: DurableDb,
) -> Result<Sse, ApiError> {
    internal(headers)?;
    ensure_schema(&db)
        .await
        .map_err(|error| room_failed(&error))?;
    let presence = read_presence(&db)
        .await
        .map_err(|error| room_failed(&error))?;
    let Some(presence) = presence else {
        return Err(ApiError::Room(
            "a command stream was opened before any daemon attached".to_owned(),
        ));
    };
    if presence.epoch != epoch {
        return Err(ApiError::RelayEpochStale {
            current: presence.epoch,
            opened: epoch,
        });
    }

    let feed = CommandFeed {
        db,
        epoch,
        cursor: 0,
        sent_resize: false,
        desktop_fresh: true,
        superseded_announced: false,
        last_touch: 0,
    };
    Ok(crate::sse::serve(
        feed,
        poll_command_feed,
        crate::sse::HEARTBEAT,
    ))
}

/// One poll of the command stream.
///
/// The remembered pane size goes first — before anything queued — because
/// the daemon's PTY should be born fitted rather than refitted a message
/// in. Then every `daemon_commands` row past the cursor, oldest first.
/// The stream ends when the attach it serves has been superseded.
fn poll_command_feed(feed: &mut CommandFeed) -> crate::sse::PollFn<'_> {
    Box::pin(async move {
        command_feed_step(feed)
            .await
            .unwrap_or(crate::sse::Poll::Idle)
    })
}

/// One poll step, fallible so a failed read surfaces once as a warning
/// rather than killing the stream — storage retries answer next tick.
async fn command_feed_step(feed: &mut CommandFeed) -> Result<crate::sse::Poll, ()> {
    let now = now_unix();
    let db = &feed.db;

    if !feed.sent_resize {
        feed.sent_resize = true;
        let size: Option<TerminalSizeRow> =
            sql!(db, "SELECT cols, rows FROM terminal_size WHERE id = 0")
                .fetch_optional()
                .await
                .map_err(|error| {
                    tracing::warn!(%error, "a command stream could not read the terminal size");
                })?;
        if let Some(TerminalSizeRow { cols, rows }) = size {
            return Ok(crate::sse::Poll::Emit(vec![command_event(
                None,
                &serde_json::json!({
                    "type": "terminal_resize",
                    "cols": cols,
                    "rows": rows,
                }),
            )]));
        }
    }

    let presence = read_presence(&feed.db).await.map_err(|error| {
        tracing::warn!(%error, "a command stream could not read daemon presence");
    })?;
    if let Some(row) = presence {
        // The attach this stream serves was superseded; the newer
        // epoch's stream is the one the room answers now. The superseded
        // daemon is told outright before the stream ends — a bare EOF is
        // indistinguishable from a dropped connection, and an uninformed
        // loser re-attaches into the epoch that replaced it, ending the
        // winner's stream in turn. The poll cannot emit and end in one
        // step, so the command goes out this tick and `Poll::End` the next.
        if row.epoch != feed.epoch {
            if feed.superseded_announced {
                return Ok(crate::sse::Poll::End);
            }
            feed.superseded_announced = true;
            let command = command_value(&ControlToDaemon::Superseded).map_err(|error| {
                tracing::warn!(%error, "a command stream could not encode its supersession");
            })?;
            return Ok(crate::sse::Poll::Emit(vec![command_event(None, &command)]));
        }
    }
    if now.saturating_sub(feed.last_touch) >= PRESENCE_REFRESH_SECONDS {
        let live_until = now.saturating_add(PRESENCE_TTL_SECONDS);
        let epoch = feed.epoch;
        sql!(
            db,
            "UPDATE daemon_presence SET live_until = {live_until} \
             WHERE id = 0 AND epoch = {epoch}"
        )
        .execute()
        .await
        .map_err(|error| {
            tracing::warn!(%error, "a command stream could not renew daemon presence");
        })?;
        feed.last_touch = now;
    }

    let mut emitted = Vec::new();
    if feed.desktop_fresh {
        feed.desktop_fresh = false;
        emitted.extend(fresh_desktop_announce(db, now).await);
    }

    // Reconcile the desktop leases against what the daemon was last
    // told. A watcher that joined or lapsed since the last tick changes
    // what the encoder should be doing, and this stream is the room's
    // only way to say so — which is also what carries the correction to
    // a fresh attach's first poll. State announcements go ahead of the
    // queued rows so a daemon learns who owns the screen before the work.
    emitted.extend(reconcile_desktop(db, now).await.unwrap_or_else(|error| {
        tracing::warn!(%error, "a command stream could not reconcile the desktop audience");
        Vec::new()
    }));

    let cursor = feed.cursor;
    let rows: Vec<CommandRow> = sql!(
        db,
        "SELECT seq, json FROM daemon_commands WHERE seq > {cursor} ORDER BY seq"
    )
    .fetch_all()
    .await
    .map_err(|error| {
        tracing::warn!(%error, "a command stream could not poll its commands");
    })?;
    if rows.is_empty() && emitted.is_empty() {
        return Ok(crate::sse::Poll::Idle);
    }
    // The poll just drained `rows` of backlog — bill them. An idle poll
    // reads nothing and is billed nothing: a stream's keepalive cadence
    // must not spend the budget a backlog could.
    crate::row_budget::charge_reads(db, rows.len() as u64)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "a command stream hit the row budget");
        })?;
    feed.cursor = rows.last().map_or(feed.cursor, |row| row.seq);
    emitted.extend(
        rows.iter()
            .map(|row| command_event(Some(row.seq), &row.json)),
    );
    Ok(crate::sse::Poll::Emit(emitted))
}

/// Reads the desktop lease table against what the daemon was last told,
/// emitting the command each change calls for.
///
/// The aggregate — is anyone watching, is anyone driving — lives in
/// `desktop_watchers`, and the last-announced reading of it in
/// `desktop_state`. This is the only writer that fires without a
/// request behind it: a watch stream ending is not an event anyone
/// sends, so expiry is noticed here, on the daemon's own heartbeat.
/// What a *fresh* attach is owed it does not cover — `desktop_state`
/// says what the previous daemon heard, which is replayed by the feed's
/// `desktop_fresh` emit instead, unconditionally.
async fn reconcile_desktop(db: &DurableDb, now: u64) -> Result<Vec<Event>, DurableObjectError> {
    let (watching, takeover) = desktop_aggregate(db, now).await?;

    let mut events = Vec::new();
    if note_audience(db, watching).await? {
        events.push(command_event(
            None,
            &command_value(&ControlToDaemon::DesktopAudience { watching })?,
        ));
    }
    if note_takeover(db, takeover).await? {
        events.push(command_event(
            None,
            &command_value(&ControlToDaemon::DesktopTakeover { active: takeover })?,
        ));
        // The seam between drivers is transcript whichever way it moved —
        // an expiry release is recorded exactly as an explicit one is.
        append(db, &ClientEvent::DesktopTakeover { active: takeover }).await?;
    }
    Ok(events)
}

/// The desktop aggregate a stream serving a fresh attach owes its
/// daemon, told outright.
///
/// A restarted daemon powers its supervisor on clear, and a watcher
/// still holding the screen across the gap is a fact nothing else
/// re-sends — `reconcile_desktop` only fires on a change from
/// `desktop_state`. Only the set halves need saying: a daemon boots
/// assuming no audience and no takeover, so a false announces nothing
/// it does not already believe — and emitting it would replay a phantom
/// release into a live watcher's stream.
///
/// The aggregate is then landed on `desktop_state` through the same
/// CAS the routes and the reconcile claim, so a flip nobody announced
/// is still recorded once, and the poll's own reconcile does not
/// announce the same state a second time. Rows still queued from the
/// daemon this attach replaces are stale in its hands and swept.
async fn fresh_desktop_announce(db: &DurableDb, now: u64) -> Vec<Event> {
    let mut emitted = Vec::new();
    let (watching, takeover) = match desktop_aggregate(db, now).await {
        Ok(aggregate) => aggregate,
        Err(error) => {
            tracing::warn!(%error, "a command stream could not read the desktop leases");
            return emitted;
        }
    };
    for command in [
        watching.then_some(ControlToDaemon::DesktopAudience { watching }),
        takeover.then_some(ControlToDaemon::DesktopTakeover { active: takeover }),
    ]
    .into_iter()
    .flatten()
    {
        match command_value(&command) {
            Ok(value) => emitted.push(command_event(None, &value)),
            Err(error) => {
                tracing::warn!(%error, "a desktop state would not encode");
            }
        }
    }
    if let Err(error) = note_desktop_state(db, watching, takeover).await {
        tracing::warn!(%error, "a command stream could not note the desktop state");
    }
    if let Err(error) = sweep_stale_desktop_commands(db).await {
        tracing::warn!(%error, "a command stream could not sweep stale desktop commands");
    }
    emitted
}

/// Lands the desktop aggregate on `desktop_state` without emitting —
/// the fresh attach that runs this was already told outright. Landing
/// the takeover half is still the announce: a flip that reached the
/// lease table but never a daemon (the caller that made it was cut off
/// mid-write) is recorded here, under the same CAS the routes and the
/// reconcile claim.
async fn note_desktop_state(
    db: &DurableDb,
    watching: bool,
    takeover: bool,
) -> Result<(), DurableObjectError> {
    note_audience(db, watching).await?;
    if note_takeover(db, takeover).await? {
        append(db, &ClientEvent::DesktopTakeover { active: takeover }).await?;
    }
    Ok(())
}

/// Drops the `daemon_commands` rows a fresh attach must not inherit.
///
/// State rows — an audience flip, a takeover — are covered by the
/// aggregate the attach was just told; replaying the queue's copy after
/// it would hand the daemon a state older than the announce. An input
/// batch is staler still: it was only ever live for the daemon holding
/// the screen when it was sent, and `survives_a_disconnect` names it
/// ephemeral. Everything else keeps its place in the queue.
async fn sweep_stale_desktop_commands(db: &DurableDb) -> Result<(), DurableObjectError> {
    let rows: Vec<CommandRow> = sql!(db, "SELECT seq, json FROM daemon_commands")
        .fetch_all()
        .await?;
    for row in rows {
        let Ok(
            ControlToDaemon::DesktopAudience { .. }
            | ControlToDaemon::DesktopTakeover { .. }
            | ControlToDaemon::DesktopInput { .. },
        ) = serde_json::from_value::<ControlToDaemon>(row.json)
        else {
            continue;
        };
        sql!(db, "DELETE FROM daemon_commands WHERE seq = {row.seq}")
            .execute()
            .await?;
    }
    Ok(())
}

/// Who is on the session's screen right now: (anyone watching, anyone
/// driving).
///
/// The sweep is part of the answer: a lapsed lease is already nobody, so
/// expired rows are deleted before either count runs — a dead tab holds
/// neither the encoder nor the screen past its deadline.
async fn desktop_aggregate(db: &DurableDb, now: u64) -> Result<(bool, bool), DurableObjectError> {
    sql!(db, "DELETE FROM desktop_watchers WHERE live_until < {now}")
        .execute()
        .await?;
    let watchers: u64 = sql!(
        db,
        "SELECT count(*) FROM desktop_watchers WHERE live_until >= {now}"
    )
    .fetch_scalar()
    .await?;
    let taken: u64 = sql!(
        db,
        "SELECT count(*) FROM desktop_watchers \
         WHERE takeover != 0 AND live_until >= {now}"
    )
    .fetch_scalar()
    .await?;
    Ok((watchers > 0, taken > 0))
}

/// The attach's replay of the desktop aggregate, and the live
/// corrections after it.
///
/// `note_audience`/`note_takeover` are conditional updates on the one
/// `desktop_state` row: landing the write is what names the caller the
/// flip's announcer, so the takeover route, the watch route, and this
/// reconcile can never emit the same transition twice.
async fn note_audience(db: &DurableDb, watching: bool) -> Result<bool, DurableObjectError> {
    Ok(sql!(
        db,
        "UPDATE desktop_state SET watching = {watching} \
         WHERE id = 0 AND watching != {watching}"
    )
    .execute()
    .await?
    .rows_written
        > 0)
}

/// See [`note_audience`].
async fn note_takeover(db: &DurableDb, takeover: bool) -> Result<bool, DurableObjectError> {
    Ok(sql!(
        db,
        "UPDATE desktop_state SET takeover = {takeover} \
         WHERE id = 0 AND takeover != {takeover}"
    )
    .execute()
    .await?
    .rows_written
        > 0)
}

/// A command as the `daemon_commands` row would store it.
///
/// The desktop announcements the feed emits are not rows — they carry
/// no ordering obligation and are acknowledged for nothing — but they
/// encode through the real variant rather than a literal, so a tag or
/// field rename fails to compile instead of silently emitting a command
/// the daemon does not know.
fn command_value(command: &ControlToDaemon) -> Result<serde_json::Value, DurableObjectError> {
    serde_json::to_value(command)
        .map_err(|error| DurableObjectError::Serialization(error.to_string()))
}

/// Encodes one command for the wire.
///
/// The envelope's `command` field carries the row's JSON verbatim rather
/// than a re-serialized [`ControlToDaemon`], so a command written by a
/// newer control plane reaches the daemon exactly as stored. A `seq` of
/// `None` marks a state replay — the remembered pane size — which carries
/// no ordering obligation and is acknowledged for nothing.
fn command_event(seq: Option<u64>, command: &serde_json::Value) -> Event {
    let envelope = serde_json::json!({
        "seq": seq,
        "command": command,
    });
    let event = Event::data(envelope.to_string()).event("command");
    match seq {
        Some(seq) => event.id(seq.to_string()),
        None => event,
    }
}

/// The command stream's working state.
struct CommandFeed {
    db: DurableDb,
    /// The attach this stream serves; a newer one ends it.
    epoch: u64,
    /// How far down `daemon_commands` this stream has handed over.
    cursor: u64,
    /// Whether the remembered pane size has been replayed yet.
    sent_resize: bool,
    /// Whether this stream still owes its daemon the desktop aggregate —
    /// set only for the attach it opens on, cleared by the first poll.
    desktop_fresh: bool,
    /// Whether the supersession notice has already been emitted — the next
    /// poll ends the body.
    superseded_announced: bool,
    /// When presence was last renewed, seconds.
    last_touch: u64,
}

/// Accepts one batch of a daemon's outbound frames.
///
/// Ordering is the caller's job and the room enforces it: a batch resumes
/// where the last stored one ended, so a retransmission is answered
/// without reapplying anything and a gap is refused — the daemon's
/// in-flight queue already holds the missing frames, so refusing costs a
/// resend rather than a loss.
async fn accept_frames(
    headers: Headers,
    Json(batch): Json<DaemonFrames>,
    db: DurableDb,
    kv: DurableKv,
) -> Outcome<Json<Emitted>> {
    accept_batch(&headers, batch, &db, &kv).await.into()
}

async fn accept_batch(
    headers: &Headers,
    batch: DaemonFrames,
    db: &DurableDb,
    kv: &DurableKv,
) -> Result<Json<Emitted>, ApiError> {
    internal(headers)?;
    ensure_schema(db)
        .await
        .map_err(|error| room_failed(&error))?;
    let Some(presence) = read_presence(db)
        .await
        .map_err(|error| room_failed(&error))?
    else {
        return Err(ApiError::Room(
            "a daemon posted frames before it attached".to_owned(),
        ));
    };
    if presence.epoch != batch.epoch {
        return Err(ApiError::RelayEpochStale {
            current: presence.epoch,
            opened: batch.epoch,
        });
    }

    let epoch = batch.epoch;
    let through: u64 = sql!(
        db,
        "SELECT through FROM daemon_frames WHERE epoch = {epoch}"
    )
    .fetch_scalar_optional()
    .await
    .map_err(|error| room_failed(&error))?
    .unwrap_or(0);
    let next = through.saturating_add(1);
    if batch.from_seq > next {
        return Err(ApiError::RelayFramesGap {
            next,
            got: batch.from_seq,
        });
    }
    // A batch may overlap the stored tail — the POST that carried its head
    // may have been answered and lost on the wire. Frames at or below
    // `through` are already applied; only the rest is new.
    let skip = usize::try_from(next.saturating_sub(batch.from_seq)).unwrap_or(usize::MAX);
    let fresh = batch.frames.get(skip..).unwrap_or(&[]);
    let mut emitted = Vec::with_capacity(fresh.len());
    for frame in fresh {
        emitted.extend(apply_daemon_frame(frame, db, kv).await?);
    }
    if !batch.frames.is_empty() {
        let new_through = batch
            .from_seq
            .saturating_add(u64::try_from(batch.frames.len()).unwrap_or(0))
            .saturating_sub(1);
        sql!(
            db,
            "INSERT INTO daemon_frames (epoch, through) VALUES ({epoch}, {new_through}) \
             ON CONFLICT (epoch) DO UPDATE SET \
                 through = max(excluded.through, daemon_frames.through)"
        )
        .execute()
        .await
        .map_err(|error| room_failed(&error))?;
    }

    let ack_through = batch.ack_through;
    if ack_through > 0 {
        sql!(db, "DELETE FROM daemon_commands WHERE seq <= {ack_through}")
            .execute()
            .await
            .map_err(|error| room_failed(&error))?;
    }

    // A POST is contact: a daemon whose command stream died but which is
    // still talking is alive, and the marker should say so until the
    // stream catches up again.
    let live_until = now_unix().saturating_add(PRESENCE_TTL_SECONDS);
    sql!(
        db,
        "UPDATE daemon_presence SET live_until = {live_until} \
         WHERE id = 0 AND epoch = {epoch}"
    )
    .execute()
    .await
    .map_err(|error| room_failed(&error))?;

    Ok(Json(Emitted { events: emitted }))
}

/// Applies one daemon frame the way the relay did: recorded when it is
/// something that happened, folded into the emitted list in its client
/// form.
async fn apply_daemon_frame(
    frame: &DaemonToControl,
    db: &DurableDb,
    kv: &DurableKv,
) -> Result<Vec<EmittedEvent>, ApiError> {
    // Addressed rather than fanned out: one browser is waiting on the
    // HTTP request this answers, and nobody else has any use for it.
    if let DaemonToControl::WorkdirReply { id, reply } = frame {
        store_workdir_reply(db, *id, reply)
            .await
            .map_err(|error| room_failed(&error))?;
        return Ok(Vec::new());
    }

    // Stream bytes rather than an event: chunks land in the stream's own
    // table, where watch streams read them, and never in `events` — a
    // replayed transcript is not a screen recording.
    if let DaemonToControl::DesktopChunk { keyframe, data } = frame {
        store_desktop_chunk(db, *keyframe, data)
            .await
            .map_err(|error| room_failed(&error))?;
        return Ok(Vec::new());
    }

    let seq = record(frame, db, kv)
        .await
        .map_err(|error| room_failed(&error))?;
    let Some(event) = ClientEvent::from_daemon(frame.clone()) else {
        // Every post-attach frame has a client form; reaching this means
        // the mapping grew a hole.
        return Err(ApiError::Room(
            "a daemon frame had no client form".to_owned(),
        ));
    };
    Ok(vec![EmittedEvent { seq, event }])
}

// ── The room's command surface ──

/// Runs a control-plane command against the room.
///
/// The Worker calls this after it has done the durable half of the work —
/// recording an approval decision in D1, applying a budget signal — so the
/// live half can never disagree with what was persisted. The answer names
/// every event the command produced, for the Worker to fan out.
async fn run_command(
    headers: Headers,
    Json(command): Json<ControlToDaemon>,
    db: DurableDb,
) -> Outcome<Json<Emitted>> {
    dispatch_command(&headers, &command, &db).await.into()
}

async fn dispatch_command(
    headers: &Headers,
    command: &ControlToDaemon,
    db: &DurableDb,
) -> Result<Json<Emitted>, ApiError> {
    internal(headers)?;
    ensure_schema(db)
        .await
        .map_err(|error| room_failed(&error))?;

    // A stale presence marker gets announced once, before anything else
    // the command produces, so a watcher learns the daemon is gone ahead
    // of whatever the command itself has to say. The flag keeps the
    // offline paths below from saying it a second time in the same call.
    let mut events = Vec::new();
    let gone_announced = announce_stale_presence(db, &mut events)
        .await
        .map_err(|error| room_failed(&error))?;
    let live = daemon_live(db).await.map_err(|error| room_failed(&error))?;

    match command {
        ControlToDaemon::UserMessage { text, origin } => {
            deliver_user_message(db, live, text, *origin, &mut events).await?;
        }
        ControlToDaemon::ShellCommand { command } => {
            deliver_shell_command(db, live, gone_announced, command, &mut events).await?;
        }
        // A pane size is a state, not an instant: it is kept for whichever
        // daemon attaches next, and one that is here now is told at once.
        // A daemon not being here is not news worth announcing for it —
        // the pane was fitted while the machine was still being built,
        // which is the ordinary case.
        ControlToDaemon::TerminalResize { cols, rows } => {
            remember_terminal_size(db, *cols, *rows)
                .await
                .map_err(|error| room_failed(&error))?;
            if live {
                queue_command(db, command)
                    .await
                    .map_err(|error| room_failed(&error))?;
            }
        }
        // Composed by the room itself as it ends a superseded stream —
        // the one command nobody may send it.
        ControlToDaemon::Superseded => {
            return Err(ApiError::RoomRefused(Box::new(Problem::of_type(
                "superseded-is-room-composed",
                400,
                "Bad Request",
                "superseded is the room's own last word on a superseded stream, not a command it accepts",
            ))));
        }
        _ => {
            // Everything else queues only while a daemon is attached. The
            // commands that must not be lost are exactly the ones
            // `survives_a_disconnect` names, plus user messages and shell
            // commands handled above; an interrupt or keystroke held for a
            // daemon that reconnects an hour later would arrive as an
            // instruction about a turn that no longer exists. Whoever sent
            // one, and everyone else watching, is told the machine is off
            // the room instead — a client that pressed Stop and was told
            // nothing waits on a turn no longer being run.
            if live || command.survives_a_disconnect() {
                queue_command(db, command)
                    .await
                    .map_err(|error| room_failed(&error))?;
            } else {
                tracing::warn!(
                    ?command,
                    "dropped a command: this session has no daemon attached"
                );
                if !gone_announced {
                    events.push(EmittedEvent {
                        seq: None,
                        event: ClientEvent::MachineConnection { connected: false },
                    });
                }
            }
            events.extend(echo_of(db, command).await?);
        }
    }
    Ok(Json(Emitted { events }))
}

/// Whether the daemon's presence marker is still inside its deadline.
async fn daemon_live(db: &DurableDb) -> Result<bool, DurableObjectError> {
    let Some(presence) = read_presence(db).await? else {
        return Ok(false);
    };
    Ok(presence.live_until >= now_unix())
}

/// Reads the presence row, if an attach has ever written one.
async fn read_presence(db: &DurableDb) -> Result<Option<PresenceRow>, DurableObjectError> {
    sql!(
        db,
        "SELECT epoch, live_until, gone_reported FROM daemon_presence WHERE id = 0"
    )
    .fetch_optional()
    .await
    .map_err(|error| stored(&error))
}

/// Emits `MachineConnection{false}` once for an attach that has expired.
///
/// The flag on the presence row is what makes this once *ever*: every
/// room call checks the marker, and only the first to find it stale says
/// so. The returned bool is what makes it once *per call*: the offline
/// paths announce it themselves for a daemon that was already known gone.
async fn announce_stale_presence(
    db: &DurableDb,
    events: &mut Vec<EmittedEvent>,
) -> Result<bool, DurableObjectError> {
    let Some(presence) = read_presence(db).await? else {
        return Ok(false);
    };
    if presence.live_until >= now_unix() || presence.gone_reported != 0 {
        return Ok(false);
    }
    sql!(
        db,
        "UPDATE daemon_presence SET gone_reported = 1 WHERE id = 0"
    )
    .execute()
    .await
    .map_err(|error| stored(&error))?;
    events.push(EmittedEvent {
        seq: None,
        event: ClientEvent::MachineConnection { connected: false },
    });
    Ok(true)
}

/// Records a command for the daemon's stream to hand over.
///
/// Every deliverable command is a row — the stream is a cursor over the
/// table and an acknowledgement deletes from it, so queued-but-live and
/// queued-while-away are the same mechanism at different lengths of wait.
async fn queue_command(
    db: &DurableDb,
    command: &ControlToDaemon,
) -> Result<u64, DurableObjectError> {
    let json = serde_json::to_string(command)
        .map_err(|error| DurableObjectError::Serialization(error.to_string()))?;
    sql!(
        db,
        "INSERT INTO daemon_commands (json, at_unix) VALUES ({json}, {now_unix()})"
    )
    .execute()
    .await
    .map_err(|error| stored(&error))?;
    sql!(
        db,
        "SELECT seq FROM daemon_commands ORDER BY seq DESC LIMIT 1"
    )
    .fetch_scalar()
    .await
    .map_err(|error| stored(&error))
}

/// Records a user message, echoes it, and queues it for the daemon.
///
/// The one path a user message takes, whichever door it came in by. The
/// recorded event comes first — the message is conversation, and a replay
/// without it would show answers to questions nobody asked — and the
/// command row follows, because a daemon that is away is still owed the
/// message: it is the whole point of the session, and the user has no way
/// to know it was thrown away.
async fn deliver_user_message(
    db: &DurableDb,
    live: bool,
    text: &str,
    origin: MessageOrigin,
    events: &mut Vec<EmittedEvent>,
) -> Result<(), ApiError> {
    let seq = append(
        db,
        &ClientEvent::UserMessage {
            text: text.to_owned(),
            origin,
        },
    )
    .await
    .map_err(|error| room_failed(&error))?;
    events.push(EmittedEvent {
        seq: Some(seq),
        event: ClientEvent::UserMessage {
            text: text.to_owned(),
            origin,
        },
    });

    // The origin travels no further. What reaches the harness is the
    // text: a model told that its next instruction was written by a
    // program would reason about the framing instead of the work.
    let command = ControlToDaemon::UserMessage {
        text: text.to_owned(),
        origin: MessageOrigin::User,
    };
    queue_command(db, &command)
        .await
        .map_err(|error| room_failed(&error))?;
    if !live {
        tracing::info!("queued a user message for a daemon that is not attached yet");
    }
    Ok(())
}

/// Records a `!` shell command, echoes it, and queues it for the daemon
/// (docs/ux.md §9.3).
///
/// The room is where a run gets its identity. The client sends a bare
/// [`ControlToDaemon::ShellCommand`]; this mints the [`ShellRunId`] that
/// the recorded row, every output chunk and the exit status are keyed by,
/// and reissues the command to the daemon as [`ControlToDaemon::RunShell`].
/// One writer assigning one identity is what keeps two clients running
/// `!` commands at the same moment from having their output attached to
/// each other's row.
///
/// Recorded before it is queued, like a user message, because a `!`
/// command is something a person did to this session: a replay without it
/// would show a build's output with nothing saying what was built.
///
/// Nothing waits for a daemon that is away. A command held for one that
/// turns up an hour later would run against a working tree the user is no
/// longer looking at — so a session with no daemon attached gets an
/// immediate [`ShellOutcome::Offline`](flyco_core::ShellOutcome::Offline)
/// instead of silence.
async fn deliver_shell_command(
    db: &DurableDb,
    live: bool,
    gone_announced: bool,
    command: &str,
    events: &mut Vec<EmittedEvent>,
) -> Result<(), ApiError> {
    let run = ShellRunId::generate();
    let asked = ClientEvent::ShellCommand {
        run,
        command: command.to_owned(),
    };
    let seq = append(db, &asked)
        .await
        .map_err(|error| room_failed(&error))?;
    events.push(EmittedEvent {
        seq: Some(seq),
        event: asked,
    });

    if live {
        let instruction = ControlToDaemon::RunShell {
            run,
            command: command.to_owned(),
        };
        queue_command(db, &instruction)
            .await
            .map_err(|error| room_failed(&error))?;
        return Ok(());
    }

    tracing::warn!(%run, "a shell command arrived while this session had no daemon attached");
    // Both halves of the truth: this run is over before it started, and
    // the reason is that the machine is not on the room.
    if !gone_announced {
        events.push(EmittedEvent {
            seq: None,
            event: ClientEvent::MachineConnection { connected: false },
        });
    }
    let offline = ClientEvent::ShellExited {
        run,
        outcome: flyco_core::ShellOutcome::Offline,
        truncated: false,
    };
    let seq = append(db, &offline)
        .await
        .map_err(|error| room_failed(&error))?;
    events.push(EmittedEvent {
        seq: Some(seq),
        event: offline,
    });
    Ok(())
}

/// The event a command echoes to watchers, when it has one.
///
/// A model or permission-mode change is echoed whether the daemon took
/// the command or the room queued it: the change is already recorded, so
/// a browser watching a session between machines still sees the line —
/// the model is what the next machine comes up on.
async fn echo_of(db: &DurableDb, command: &ControlToDaemon) -> Result<Vec<EmittedEvent>, ApiError> {
    let (event, recorded) = match command {
        ControlToDaemon::ApprovalDecision { id, decision } => (
            ClientEvent::ApprovalDecided {
                id: *id,
                decision: *decision,
            },
            false,
        ),
        ControlToDaemon::Archive { .. } => (
            ClientEvent::SessionStateChanged {
                state: flyco_core::SessionState::Archived,
            },
            false,
        ),
        ControlToDaemon::SetModel { model } => (
            ClientEvent::ModelChanged {
                model: model.clone(),
            },
            true,
        ),
        ControlToDaemon::SetPermissionMode { mode } => {
            (ClientEvent::PermissionModeChanged { mode: *mode }, true)
        }
        ControlToDaemon::SetComputerUse { enabled } => {
            (ClientEvent::ComputerUseChanged { enabled: *enabled }, true)
        }
        _ => return Ok(Vec::new()),
    };

    // Recorded for an event that is *transcript* — a model change is a
    // line in the conversation, because what answers from here on is a
    // different model and a replay with no seam in it would misrepresent
    // itself. Live-only for the ones a browser re-reads from the control
    // plane anyway: an approval's decision and a lifecycle move would be
    // a second answer free to disagree with the first.
    let seq = if recorded {
        Some(
            append(db, &event)
                .await
                .map_err(|error| room_failed(&error))?,
        )
    } else {
        None
    };
    Ok(vec![EmittedEvent { seq, event }])
}

/// Appends a frame to the room's durable stream and caches what the UI
/// reads on connect.
///
/// Only what *happened* is appended — the harness stream and the
/// provisioning timeline: a catch-up must replay events, and a usage
/// meter or a capability set is a *current value*, so storing every one
/// of them would grow the table without making the replay any more
/// complete. Those go to KV, newest wins.
async fn record(
    frame: &DaemonToControl,
    db: &DurableDb,
    kv: &DurableKv,
) -> Result<Option<u64>, DurableObjectError> {
    match frame {
        DaemonToControl::Harness { event } => append(
            db,
            &ClientEvent::Harness {
                event: event.clone(),
            },
        )
        .await
        .map(Some),
        DaemonToControl::Started { harness_session_id } => {
            put_latest(kv, KEY_HARNESS_SESSION, harness_session_id)
                .await
                .map(|()| None)
        }
        DaemonToControl::Capabilities { capabilities } => {
            put_latest(kv, KEY_CAPABILITIES, capabilities)
                .await
                .map(|()| None)
        }
        // Appended rather than kept in KV beside the capability set, and
        // for the reason the model list is appended too: the `/` palette is
        // built from the room's replayed stream, so a browser that opens
        // the session long after the daemon reported its commands has to
        // find them there or open on flyco's own three.
        DaemonToControl::Commands { commands } => append(
            db,
            &ClientEvent::Commands {
                commands: commands.clone(),
            },
        )
        .await
        .map(Some),
        // Both halves of a `!` command's answer are appended: the command
        // was recorded when it arrived, and a transcript row that replayed
        // as a command with no output and no exit status would be worse
        // than not replaying it at all. The web terminal's output is live
        // only for the opposite reason — it has no row to belong to.
        DaemonToControl::ShellOutput { run, stream, data } => append(
            db,
            &ClientEvent::ShellOutput {
                run: *run,
                stream: *stream,
                data: data.clone(),
            },
        )
        .await
        .map(Some),
        DaemonToControl::ShellExited {
            run,
            outcome,
            truncated,
        } => append(
            db,
            &ClientEvent::ShellExited {
                run: *run,
                outcome: outcome.clone(),
                truncated: *truncated,
            },
        )
        .await
        .map(Some),
        DaemonToControl::ProvisioningStage { stage, at_unix } => append(
            db,
            &ClientEvent::ProvisioningStage {
                stage: *stage,
                at_unix: *at_unix,
            },
        )
        .await
        .map(Some),
        DaemonToControl::DesktopState { status, detail } => append(
            db,
            &ClientEvent::DesktopState {
                status: *status,
                detail: detail.clone(),
            },
        )
        .await
        .map(Some),
        DaemonToControl::DesktopActive => append(db, &ClientEvent::DesktopActive).await.map(Some),
        // Appended rather than only forwarded: a reclamation is something
        // that *happened* to the session, and the browser most likely to
        // want it is one opened after the machine was already gone.
        DaemonToControl::SpotNotice { seconds_remaining } => append(
            db,
            &ClientEvent::SpotNotice {
                seconds_remaining: *seconds_remaining,
            },
        )
        .await
        .map(Some),
        DaemonToControl::RepoDirty { dir, summary } => note_checkout(kv, dir.as_ref(), summary)
            .await
            .map(|()| None),
        DaemonToControl::RepoAdded { slug, branch, dir } => append(
            db,
            &ClientEvent::RepoAdded {
                slug: slug.clone(),
                branch: branch.clone(),
                dir: dir.clone(),
            },
        )
        .await
        .map(Some),
        _ => Ok(None),
    }
}

/// Records one checkout's last-reported `git status` in the room's KV.
///
/// The daemon is the only thing that can see the working trees, and it
/// reports each checkout's whole `git status --short` output rather than a
/// flag, so an empty summary is the clean tree and the absence of any
/// report is "nobody has looked" — which is what
/// `GET /v1/sessions/{id}/repo-status` refuses to answer.
async fn note_checkout(
    kv: &DurableKv,
    dir: Option<&String>,
    summary: &str,
) -> Result<(), DurableObjectError> {
    let mut status: RepoStatus = kv
        .get_json(KEY_REPO_STATUS)
        .await
        .map_err(|error| DurableObjectError::Runtime(error.to_string()))?
        .unwrap_or_default();
    let checkout = CheckoutStatus {
        dir: dir.cloned(),
        dirty: !summary.trim().is_empty(),
        summary: summary.to_owned(),
    };
    match status
        .checkouts
        .iter_mut()
        .find(|entry| entry.dir == checkout.dir)
    {
        Some(entry) => *entry = checkout,
        None => status.checkouts.push(checkout),
    }
    put_latest(kv, KEY_REPO_STATUS, &status).await
}

/// Appends one event to the room's replayable stream, and answers with
/// the position it took.
///
/// The position is read back rather than counted in the struct: the room
/// is rebuilt around every event, and `AUTOINCREMENT` is the only thing
/// here that stays monotonic across that and across two writes racing in
/// one wake-up.
async fn append(db: &DurableDb, event: &ClientEvent) -> Result<u64, DurableObjectError> {
    let json = serde_json::to_string(event)
        .map_err(|error| DurableObjectError::Serialization(error.to_string()))?;

    ensure_schema(db).await?;
    sql!(
        db,
        "INSERT INTO events (json, at_unix) VALUES ({json}, {now_unix()})"
    )
    .execute()
    .await
    .map_err(|error| stored(&error))?;

    // Single-threaded per room: nothing else can have appended between
    // the insert above and this read.
    sql!(db, "SELECT seq FROM events ORDER BY seq DESC LIMIT 1")
        .fetch_scalar_optional()
        .await
        .map_err(|error| stored(&error))?
        .ok_or_else(|| DurableObjectError::Runtime("an appended event had no position".to_owned()))
}

/// The browser's terminal pane, as it was last fitted.
#[derive(Debug, skyzen::FromRow)]
struct TerminalSizeRow {
    cols: u16,
    rows: u16,
}

/// Records the size of the client's terminal pane.
async fn remember_terminal_size(
    db: &DurableDb,
    cols: u16,
    rows: u16,
) -> Result<(), DurableObjectError> {
    ensure_schema(db).await?;
    sql!(
        db,
        "INSERT INTO terminal_size (id, cols, rows) VALUES (0, {cols}, {rows}) \
         ON CONFLICT (id) DO UPDATE SET cols = excluded.cols, rows = excluded.rows"
    )
    .execute()
    .await
    .map_err(|error| stored(&error))?;
    Ok(())
}

/// KV key holding the harness-native session id a resume needs.
const KEY_HARNESS_SESSION: &str = "room:harness_session_id";

/// KV key holding the newest capability set the harness advertised.
const KEY_CAPABILITIES: &str = "room:capabilities";

/// KV key holding the working tree the daemon last reported.
const KEY_REPO_STATUS: &str = "room:repo_status";

async fn put_latest<T: Serialize + Sync>(
    kv: &DurableKv,
    key: &str,
    value: &T,
) -> Result<(), DurableObjectError> {
    kv.put_json(key, value)
        .await
        .map_err(|error| DurableObjectError::Runtime(error.to_string()))
}

/// The schema this build expects.
///
/// The durable answer to "is the schema already there" lives in the
/// `schema_meta` table [`crate::schema_version`] keeps: checking it costs
/// one storage read where running every `CREATE` blind costs one per
/// statement — and every hot path in the room (each event append, each
/// page read, each attach) calls `ensure_schema` first. Bump it when the
/// DDL below changes so a room built by an older build upgrades once, on
/// its next call.
const SCHEMA_VERSION: i64 = 3;

/// Creates the room's tables if this is its first write.
///
/// `AUTOINCREMENT` rather than a counter in the struct: the sequences
/// have to be monotonic across the object being rebuilt around every
/// event, and the database is the only thing here that guarantees it.
async fn ensure_schema(db: &DurableDb) -> Result<(), DurableObjectError> {
    crate::schema_version::ensure(
        db,
        SCHEMA_VERSION,
        &[
            "CREATE TABLE IF NOT EXISTS events (\
             seq     INTEGER PRIMARY KEY AUTOINCREMENT, \
             json    TEXT    NOT NULL, \
             at_unix INTEGER NOT NULL)",
            // Every command owed to the daemon, whether it is attached or
            // not. The command stream is a cursor over this table; a row is
            // deleted the moment the daemon acknowledges past it, so the
            // table is a delivery log rather than a transcript — what a
            // command *was* is in `events` where it matters.
            "CREATE TABLE IF NOT EXISTS daemon_commands (\
             seq     INTEGER PRIMARY KEY AUTOINCREMENT, \
             json    TEXT    NOT NULL, \
             at_unix INTEGER NOT NULL)",
            // Exactly one row, because a room has exactly one daemon. The
            // epoch names the current attach; `live_until` is the deadline its
            // contact renews; `gone_reported` is what makes the first caller
            // past that deadline the only one to announce it. The CHECK is
            // what makes one row structural rather than a convention.
            "CREATE TABLE IF NOT EXISTS daemon_presence (\
             id            INTEGER PRIMARY KEY CHECK (id = 0), \
             epoch         INTEGER NOT NULL, \
             live_until    INTEGER NOT NULL, \
             gone_reported INTEGER NOT NULL)",
            // One row per attach, recording how far into that epoch's frame
            // numbering the room has stored. A retransmitted batch is answered
            // from this without reapplying a frame of it; a gap is refused.
            "CREATE TABLE IF NOT EXISTS daemon_frames (\
             epoch   INTEGER PRIMARY KEY, \
             through INTEGER NOT NULL)",
            // The size the client's terminal pane last reported, handed to
            // every daemon's fresh command stream: a daemon that was still
            // booting when the pane opened, or one restarted by a resize, has
            // a PTY at its default size and no other way to learn the real
            // one (issue #258). One row, because a session has one pane
            // size — the last client to fit its pane wins.
            "CREATE TABLE IF NOT EXISTS terminal_size (\
             id   INTEGER PRIMARY KEY CHECK (id = 0), \
             cols INTEGER NOT NULL, \
             rows INTEGER NOT NULL)",
            // One row per answered question about the checkout, keyed by the
            // id the Worker minted for it and deleted the moment the held
            // question serves it. A table rather than KV because these expire: the
            // sweep in `store_workdir_reply` needs to find rows by age, which
            // is a query and not a key.
            "CREATE TABLE IF NOT EXISTS workdir_replies (\
             id      TEXT    PRIMARY KEY, \
             json    TEXT    NOT NULL, \
             at_unix INTEGER NOT NULL)",
            // The TTL sweep in `store_workdir_reply` deletes by `at_unix`;
            // without this index every store scans the whole table.
            "CREATE INDEX IF NOT EXISTS workdir_replies_at ON workdir_replies (at_unix)",
            // The room's row-read ledger — one row per UTC day, debited by
            // `row_budget::charge_reads`, and the circuit breaker that keeps a
            // runaway reader here from spending the account's quota.
            crate::row_budget::SCHEMA,
            // The desktop stream's GOP tail: the newest keyframe plus
            // everything encoded since. Not `events` — a replayed transcript
            // is not a screen recording — and bounded, so a live session
            // cannot grow it without limit.
            "CREATE TABLE IF NOT EXISTS desktop_chunks (\
             seq      INTEGER PRIMARY KEY AUTOINCREMENT, \
             keyframe INTEGER NOT NULL, \
             data     TEXT    NOT NULL, \
             at_unix  INTEGER NOT NULL)",
            // One lease per browser watching the desktop stream. A row is a
            // heartbeat — the stream and its takeover and input calls renew
            // `live_until` — so a tab that dies stops counting, and a
            // takeover it held lapses, on the deadline rather than on a close
            // event nothing sends. `takeover` marks the one row allowed to
            // drive the screen.
            "CREATE TABLE IF NOT EXISTS desktop_watchers (\
             id         INTEGER PRIMARY KEY AUTOINCREMENT, \
             takeover   INTEGER NOT NULL, \
             live_until INTEGER NOT NULL)",
            // What the daemon was last told about its desktop audience. The
            // conditional updates in `note_audience`/`note_takeover` make a
            // flip atomic — whichever room call lands it is the one that
            // emits the command, so a join racing an expiry can never emit
            // the same transition twice. One row, like the pane size.
            "CREATE TABLE IF NOT EXISTS desktop_state (\
             id       INTEGER PRIMARY KEY CHECK (id = 0), \
             watching INTEGER NOT NULL, \
             takeover INTEGER NOT NULL)",
            // The row the conditional updates CAS against; absent it, a
            // first flip would have nothing to flip from.
            "INSERT OR IGNORE INTO desktop_state (id, watching, takeover) VALUES (0, 0, 0)",
        ],
    )
    .await
}

/// The room's own database failed, which is a runtime fault rather than
/// anything a caller did.
fn stored(error: &skyzen_services::DurableDbError) -> DurableObjectError {
    DurableObjectError::Runtime(error.to_string())
}

// ── The room's own HTTP surface ──

/// Reads the internal headers a Worker→room call must carry.
fn internal(headers: &Headers) -> Result<(), ApiError> {
    if headers.get(HEADER_INTERNAL) == Some(INTERNAL) {
        Ok(())
    } else {
        Err(ApiError::Room(
            "a room route was reached without the internal marker".to_owned(),
        ))
    }
}

/// Appends a control-plane event to the room's stream and reports it for
/// fan-out.
///
/// The other half of [`run_command`]: a command is something the *daemon*
/// must act on, and this is something only the user needs to see. The
/// provisioning timeline is the case that needs it — the queue knows the
/// machine was reserved minutes before any daemon exists to say so — and
/// it is appended rather than only emitted, because a browser opened
/// after provisioning finished must still be able to replay how long each
/// stage took.
async fn run_broadcast(
    headers: Headers,
    Json(event): Json<ClientEvent>,
    db: DurableDb,
) -> Outcome<Json<Emitted>> {
    dispatch_broadcast(&headers, &event, &db).await.into()
}

async fn dispatch_broadcast(
    headers: &Headers,
    event: &ClientEvent,
    db: &DurableDb,
) -> Result<Json<Emitted>, ApiError> {
    internal(headers)?;
    let seq = append(db, event)
        .await
        .map_err(|error| room_failed(&error))?;
    Ok(Json(Emitted {
        events: vec![EmittedEvent {
            seq: Some(seq),
            event: event.clone(),
        }],
    }))
}

/// Serves a page of the room's event tail.
async fn read_events(
    headers: Headers,
    Query(cursor): Query<EventCursor>,
    db: DurableDb,
) -> Outcome<Json<EventPage>> {
    page(&headers, cursor.after.unwrap_or(0), &db).await.into()
}

async fn page(headers: &Headers, after: u64, db: &DurableDb) -> Result<Json<EventPage>, ApiError> {
    internal(headers)?;
    ensure_schema(db)
        .await
        .map_err(|error| room_failed(&error))?;

    // One row past the page tells the caller whether to come back,
    // without a second `COUNT(*)` over a table that only grows.
    let limit = EVENT_PAGE_LIMIT + 1;
    // Bill the page's bound before reading: a room whose day is spent is
    // refused for the ledger's one row rather than the `limit` it asked
    // for — the case a runaway follower makes hot.
    crate::row_budget::charge_reads(db, u64::from(limit)).await?;
    let rows: Vec<EventRow> = sql!(
        db,
        "SELECT seq, json, at_unix FROM events WHERE seq > {after} ORDER BY seq LIMIT {limit}"
    )
    .fetch_all()
    .await
    .map_err(|error| ApiError::Room(error.to_string()))?;

    let more = rows.len() > EVENT_PAGE_LIMIT as usize;
    let events = rows
        .into_iter()
        .take(EVENT_PAGE_LIMIT as usize)
        .map(Into::into)
        .collect();

    Ok(Json(EventPage { events, more }))
}

/// Serves the working tree the daemon last reported.
async fn read_repo_status(headers: Headers, kv: DurableKv) -> Outcome<Json<RepoStatus>> {
    repo_status(&headers, &kv).await.into()
}

async fn repo_status(headers: &Headers, kv: &DurableKv) -> Result<Json<RepoStatus>, ApiError> {
    internal(headers)?;
    kv.get_json::<RepoStatus>(KEY_REPO_STATUS)
        .await
        .map_err(|error| ApiError::Room(error.to_string()))?
        .map(Json)
        .ok_or(ApiError::RepoStatusUnknown)
}

/// How long an answered workdir question is kept for its asker.
///
/// The Worker that asked holds its request open for a few seconds and
/// then gives up, so anything older than this is an answer nobody came
/// back for — a browser that closed the tab, or a request that timed
/// out. Swept on the next write rather than on a timer: a room that is
/// answering questions is exactly the room that has rows to sweep.
const WORKDIR_REPLY_TTL_SECONDS: u64 = 120;

/// How long a held workdir question waits for its answer.
///
/// A diff of a large tree runs git twice on the session VM, so this is
/// generous by the standards of a REST call — and it is still bounded,
/// because the browser is holding a request open behind it.
const WORKDIR_DEADLINE_SECONDS: u64 = 12;

/// The terminal event of a held workdir question that carries the answer.
pub(crate) const WORKDIR_REPLY_EVENT: &str = "reply";

/// The terminal event of a held workdir question the deadline ended.
pub(crate) const WORKDIR_TIMEOUT_EVENT: &str = "timeout";

/// Keeps one answer until the Worker that asked comes back for it.
async fn store_workdir_reply(
    db: &DurableDb,
    id: WorkdirRequestId,
    reply: &WorkdirReply,
) -> Result<(), DurableObjectError> {
    let json = serde_json::to_string(reply)
        .map_err(|error| DurableObjectError::Serialization(error.to_string()))?;
    let key = id.to_string();
    let now = now_unix();
    ensure_schema(db).await?;
    sql!(
        db,
        "INSERT INTO workdir_replies (id, json, at_unix) VALUES ({key}, {json}, {now}) \
         ON CONFLICT (id) DO UPDATE SET json = excluded.json, at_unix = excluded.at_unix"
    )
    .execute()
    .await
    .map_err(|error| stored(&error))?;

    let oldest = now.saturating_sub(WORKDIR_REPLY_TTL_SECONDS);
    sql!(db, "DELETE FROM workdir_replies WHERE at_unix < {oldest}")
        .execute()
        .await
        .map_err(|error| stored(&error))?;
    Ok(())
}

/// Puts one question about the checkout to the session's daemon and holds
/// the request open for the answer.
///
/// Answers `503` when no daemon is attached, which is the whole reason
/// this is not [`run_command`]: a browser waiting for a listing has to be
/// told at once that there is nothing to read it, rather than waiting out
/// the deadline for an answer that is never coming. When there is one,
/// the response is a stream that ends with one event — `reply` carrying
/// the daemon's answer, or `timeout` when [`WORKDIR_DEADLINE_SECONDS`]
/// passes first — so the Worker makes one call per question instead of
/// polling a collect route.
async fn ask_workdir(
    headers: Headers,
    Json(command): Json<ControlToDaemon>,
    db: DurableDb,
) -> Outcome<Sse> {
    ask(&headers, &command, &db).await.into()
}

async fn ask(
    headers: &Headers,
    command: &ControlToDaemon,
    db: &DurableDb,
) -> Result<Sse, ApiError> {
    internal(headers)?;
    let ControlToDaemon::InspectWorkdir { id, .. } = command else {
        return Err(ApiError::Room(
            "the workdir route was given something other than a question about the checkout"
                .to_owned(),
        ));
    };
    ensure_schema(db)
        .await
        .map_err(|error| room_failed(&error))?;
    if !daemon_live(db).await.map_err(|error| room_failed(&error))? {
        return Err(ApiError::SessionDaemonOffline);
    }
    queue_command(db, command)
        .await
        .map_err(|error| room_failed(&error))?;
    Ok(crate::sse::serve(
        WorkdirFeed {
            db: db.clone(),
            request: *id,
            deadline: now_unix().saturating_add(WORKDIR_DEADLINE_SECONDS),
            terminal_sent: false,
        },
        poll_workdir_reply,
        crate::sse::HEARTBEAT,
    ))
}

/// The wait a held workdir question runs.
struct WorkdirFeed {
    db: DurableDb,
    /// The question this wait is for.
    request: WorkdirRequestId,
    /// The unix second the wait gives up at.
    deadline: u64,
    /// Whether the terminal event has been emitted — the next poll ends
    /// the body.
    terminal_sent: bool,
}

/// One poll of a held workdir question.
///
/// The reply row is the rendezvous: the `frames` POST that stores the
/// answer and the request this stream holds open are different
/// activations of the object, and storage is the only state they share.
/// The feed's first tick doubles as the fallback for a reply that landed
/// between queueing the command and opening the wait — it reads the same
/// row either way, so a reply that raced the ask is found, not missed.
///
/// The deadline is read before storage is, so a read that keeps failing
/// still ends the wait with [`WORKDIR_TIMEOUT_EVENT`] on time: the Worker
/// holds the browser's request open on this stream's word alone.
fn poll_workdir_reply(feed: &mut WorkdirFeed) -> crate::sse::PollFn<'_> {
    Box::pin(async move {
        if feed.terminal_sent {
            return crate::sse::Poll::End;
        }
        let expired = now_unix() >= feed.deadline;
        let terminal = match take_workdir_reply(&feed.db, feed.request).await {
            Ok(Some(json)) => Event::data(json).event(WORKDIR_REPLY_EVENT),
            Ok(None) if expired => Event::data("timed out").event(WORKDIR_TIMEOUT_EVENT),
            Ok(None) => return crate::sse::Poll::Idle,
            Err(error) => {
                tracing::warn!(%error, request = %feed.request, "a workdir wait could not read its reply");
                if !expired {
                    return crate::sse::Poll::Idle;
                }
                Event::data("timed out").event(WORKDIR_TIMEOUT_EVENT)
            }
        };
        feed.terminal_sent = true;
        crate::sse::Poll::Emit(vec![terminal])
    })
}

/// Takes the stored answer to one question, if it has landed.
///
/// Single use: the row goes with the answer, and the reply is checked
/// ahead of the deadline so an answer that lands on the last tick is
/// still served.
async fn take_workdir_reply(
    db: &DurableDb,
    request: WorkdirRequestId,
) -> Result<Option<String>, skyzen_services::DurableDbError> {
    let id = request.to_string();
    let json: Option<String> = sql!(
        db,
        "SELECT json FROM workdir_replies WHERE id = {id.as_str()}"
    )
    .fetch_scalar_optional()
    .await?;
    if json.is_some() {
        sql!(db, "DELETE FROM workdir_replies WHERE id = {id.as_str()}")
            .execute()
            .await?;
    }
    Ok(json)
}

// ── The desktop stream ──

/// Opens a browser's desktop stream: the encoded tail, then live chunks.
///
/// The lease is the request's first act: `desktop_watchers` is the
/// audience the daemon encodes for and the identity takeover and input
/// calls name, and the stream that serves the watcher is also what keeps
/// it alive. The join cursor is the newest keyframe — a decoder starts
/// from nothing else — and the GOP trim keeps that the table's head.
async fn watch_desktop(headers: Headers, db: DurableDb) -> Outcome<Sse> {
    open_desktop_stream(&headers, db).await.into()
}

async fn open_desktop_stream(headers: &Headers, db: DurableDb) -> Result<Sse, ApiError> {
    internal(headers)?;
    ensure_schema(&db)
        .await
        .map_err(|error| room_failed(&error))?;
    let now = now_unix();
    let live_until = now.saturating_add(WATCHER_TTL_SECONDS);
    let watcher: u64 = sql!(
        db,
        "INSERT INTO desktop_watchers (takeover, live_until) \
         VALUES (0, {live_until}) RETURNING id"
    )
    .fetch_scalar()
    .await
    .map_err(|error| room_failed(&error))?;
    let cursor: u64 = sql!(
        db,
        "SELECT seq FROM desktop_chunks WHERE keyframe != 0 ORDER BY seq DESC LIMIT 1"
    )
    .fetch_scalar_optional::<u64>()
    .await
    .map_err(|error| room_failed(&error))?
    .map_or(0, |seq| seq.saturating_sub(1));

    // A join is already an audience flip: land it now rather than on the
    // command feed's next reconcile, so the encoder starts one tick
    // sooner. The CAS keeps a second watcher's stream from re-announcing.
    if note_audience(&db, true)
        .await
        .map_err(|error| room_failed(&error))?
        && daemon_live(&db)
            .await
            .map_err(|error| room_failed(&error))?
    {
        queue_command(&db, &ControlToDaemon::DesktopAudience { watching: true })
            .await
            .map_err(|error| room_failed(&error))?;
    }

    let feed = ChunkFeed {
        db,
        watcher,
        cursor,
        greeted: false,
        last_touch: 0,
    };
    Ok(crate::sse::serve(
        feed,
        poll_chunk_feed,
        crate::sse::HEARTBEAT,
    ))
}

/// Takes the screen, or hands it back, on behalf of one watcher.
///
/// The watcher must be a live lease — a takeover from a stream that has
/// already ended would lock the agent behind a screen nobody holds. The
/// flip is decided by the CAS on `desktop_state`: whoever lands it is
/// the one that records the seam and tells the daemon, so an explicit
/// release racing a lease expiry emits exactly one transition.
async fn desktop_takeover(
    headers: Headers,
    Json(request): Json<DesktopTakeoverRequest>,
    db: DurableDb,
) -> Outcome<Json<Emitted>> {
    take_desktop(&headers, &request, &db).await.into()
}

async fn take_desktop(
    headers: &Headers,
    request: &DesktopTakeoverRequest,
    db: &DurableDb,
) -> Result<Json<Emitted>, ApiError> {
    internal(headers)?;
    ensure_schema(db)
        .await
        .map_err(|error| room_failed(&error))?;
    let now = now_unix();
    let watcher = request.watcher;
    let live: Option<u8> = sql!(
        db,
        "SELECT takeover FROM desktop_watchers \
         WHERE id = {watcher} AND live_until >= {now}"
    )
    .fetch_scalar_optional()
    .await
    .map_err(|error| room_failed(&error))?;
    if live.is_none() {
        return Err(ApiError::DesktopWatcherGone);
    }
    let active = request.active;
    let live_until = now.saturating_add(WATCHER_TTL_SECONDS);
    sql!(
        db,
        "UPDATE desktop_watchers SET takeover = {active}, live_until = {live_until} \
         WHERE id = {watcher}"
    )
    .execute()
    .await
    .map_err(|error| room_failed(&error))?;
    if active {
        // One screen, one driver: a second tab taking over steals the
        // lease outright, and the tab it displaced learns of it as a
        // refusal on its next input rather than co-driving silently.
        sql!(
            db,
            "UPDATE desktop_watchers SET takeover = 0 WHERE id != {watcher}"
        )
        .execute()
        .await
        .map_err(|error| room_failed(&error))?;
    }

    let taken: u64 = sql!(
        db,
        "SELECT count(*) FROM desktop_watchers \
         WHERE takeover != 0 AND live_until >= {now}"
    )
    .fetch_scalar()
    .await
    .map_err(|error| room_failed(&error))?;
    let takeover = taken > 0;

    let mut events = Vec::new();
    if note_takeover(db, takeover)
        .await
        .map_err(|error| room_failed(&error))?
    {
        let event = ClientEvent::DesktopTakeover { active: takeover };
        let seq = append(db, &event)
            .await
            .map_err(|error| room_failed(&error))?;
        events.push(EmittedEvent {
            seq: Some(seq),
            event,
        });
        if daemon_live(db).await.map_err(|error| room_failed(&error))? {
            queue_command(db, &ControlToDaemon::DesktopTakeover { active: takeover })
                .await
                .map_err(|error| room_failed(&error))?;
        }
    }
    Ok(Json(Emitted { events }))
}

/// Forwards one batch of a driving watcher's input to the daemon.
///
/// Refused rather than dropped when the watcher does not hold the
/// screen: a click silently ignored looks exactly like a display that
/// did not respond, and the refusal is what tells the panel its
/// takeover lapsed.
async fn desktop_input(
    headers: Headers,
    Json(request): Json<DesktopInputRequest>,
    db: DurableDb,
) -> Outcome<NoContent> {
    drive_desktop_input(&headers, &request, &db).await.into()
}

async fn drive_desktop_input(
    headers: &Headers,
    request: &DesktopInputRequest,
    db: &DurableDb,
) -> Result<NoContent, ApiError> {
    internal(headers)?;
    ensure_schema(db)
        .await
        .map_err(|error| room_failed(&error))?;
    let now = now_unix();
    let watcher = request.watcher;
    let held: Option<WatcherRow> = sql!(
        db,
        "SELECT takeover, live_until FROM desktop_watchers WHERE id = {watcher}"
    )
    .fetch_optional()
    .await
    .map_err(|error| room_failed(&error))?;
    match held {
        Some(row) if row.live_until >= now && row.takeover != 0 => {}
        Some(row) if row.live_until >= now => return Err(ApiError::DesktopTakeoverRequired),
        _ => return Err(ApiError::DesktopWatcherGone),
    }
    if !daemon_live(db).await.map_err(|error| room_failed(&error))? {
        return Err(ApiError::SessionDaemonOffline);
    }

    // The call is contact: an active driver's lease must not lapse
    // between heartbeats of a stream that is stalled behind it.
    let live_until = now.saturating_add(WATCHER_TTL_SECONDS);
    sql!(
        db,
        "UPDATE desktop_watchers SET live_until = {live_until} WHERE id = {watcher}"
    )
    .execute()
    .await
    .map_err(|error| room_failed(&error))?;
    queue_command(
        db,
        &ControlToDaemon::DesktopInput {
            events: request.events.clone(),
        },
    )
    .await
    .map_err(|error| room_failed(&error))?;
    Ok(NoContent)
}

/// One poll of a desktop watch stream.
fn poll_chunk_feed(feed: &mut ChunkFeed) -> crate::sse::PollFn<'_> {
    Box::pin(async move {
        chunk_feed_step(feed)
            .await
            .unwrap_or(crate::sse::Poll::Idle)
    })
}

/// One poll step, fallible so a failed read surfaces once as a warning
/// rather than killing the stream — storage retries answer next tick.
async fn chunk_feed_step(feed: &mut ChunkFeed) -> Result<crate::sse::Poll, ()> {
    let now = now_unix();
    let db = &feed.db;
    let mut events = Vec::new();

    // The hello names this watcher's lease — the id takeover and input
    // calls carry — and says whether the screen is already driven, so
    // the panel opens on the truth rather than on its first refusal.
    if !feed.greeted {
        feed.greeted = true;
        let taken: u64 = sql!(
            db,
            "SELECT count(*) FROM desktop_watchers \
             WHERE takeover != 0 AND live_until >= {now}"
        )
        .fetch_scalar()
        .await
        .map_err(|error| {
            tracing::warn!(%error, "a desktop stream could not read the watchers");
        })?;
        let watcher = feed.watcher;
        events.push(
            Event::data(
                serde_json::json!({
                    "watcher": watcher,
                    "takeover": taken > 0,
                })
                .to_string(),
            )
            .event("hello"),
        );
    }

    if now.saturating_sub(feed.last_touch) >= WATCHER_REFRESH_SECONDS {
        let live_until = now.saturating_add(WATCHER_TTL_SECONDS);
        let watcher = feed.watcher;
        sql!(
            db,
            "UPDATE desktop_watchers SET live_until = {live_until} WHERE id = {watcher}"
        )
        .execute()
        .await
        .map_err(|error| {
            tracing::warn!(%error, "a desktop stream could not renew its watcher");
        })?;
        feed.last_touch = now;
    }

    // A GOP trim that ran ahead of this cursor took rows the stream still
    // owed: jump to the newest keyframe and mark the cut for the client,
    // whose decoder cannot span it.
    let oldest: Option<u64> = sql!(db, "SELECT seq FROM desktop_chunks ORDER BY seq LIMIT 1")
        .fetch_scalar_optional()
        .await
        .map_err(|error| {
            tracing::warn!(%error, "a desktop stream could not read the chunk tail");
        })?;
    if oldest.is_some_and(|oldest| feed.cursor.saturating_add(1) < oldest) {
        let head: Option<u64> = sql!(
            db,
            "SELECT seq FROM desktop_chunks WHERE keyframe != 0 ORDER BY seq DESC LIMIT 1"
        )
        .fetch_scalar_optional()
        .await
        .map_err(|error| {
            tracing::warn!(%error, "a desktop stream could not find the newest keyframe");
        })?;
        feed.cursor = head.map_or(0, |seq| seq.saturating_sub(1));
        events.push(Event::data("{}").event("resync"));
    }

    let cursor = feed.cursor;
    let limit = DESKTOP_CHUNK_BATCH;
    let rows: Vec<ChunkRow> = sql!(
        db,
        "SELECT seq, keyframe, data FROM desktop_chunks \
         WHERE seq > {cursor} ORDER BY seq LIMIT {limit}"
    )
    .fetch_all()
    .await
    .map_err(|error| {
        tracing::warn!(%error, "a desktop stream could not poll its chunks");
    })?;
    // A cursor feed bills the rows it actually returned — an idle poll
    // reads nothing and is billed nothing, so a quiet screen costs the
    // watcher nothing.
    if !rows.is_empty() {
        crate::row_budget::charge_reads(db, rows.len() as u64)
            .await
            .map_err(|error| {
                tracing::warn!(%error, "a desktop stream hit the row budget");
            })?;
    }
    feed.cursor = rows.last().map_or(feed.cursor, |row| row.seq);
    events.extend(rows.iter().map(|row| {
        Event::data(
            serde_json::json!({
                "keyframe": row.keyframe != 0,
                "data": row.data,
            })
            .to_string(),
        )
        .event("chunk")
        .id(row.seq.to_string())
    }));
    if events.is_empty() {
        return Ok(crate::sse::Poll::Idle);
    }
    Ok(crate::sse::Poll::Emit(events))
}

/// The watch stream's working state.
struct ChunkFeed {
    db: DurableDb,
    /// The lease this stream minted and renews.
    watcher: u64,
    /// How far down `desktop_chunks` it has handed over.
    cursor: u64,
    /// Whether the hello has gone out yet.
    greeted: bool,
    /// When the lease was last renewed, seconds.
    last_touch: u64,
}

/// The columns the `desktop_chunks` table stores.
#[derive(Debug, skyzen::FromRow)]
struct ChunkRow {
    seq: u64,
    keyframe: u8,
    /// The encoded bytes, base64 — the wire form the room stores verbatim.
    data: String,
}

/// The columns the `desktop_watchers` table stores.
#[derive(Debug, skyzen::FromRow)]
struct WatcherRow {
    takeover: u8,
    live_until: u64,
}

/// Stores one encoded chunk and keeps the tail to one GOP.
///
/// The bytes land as base64 text: it is the wire form the daemon already
/// speaks and the watch stream re-emits, so the room never transcodes.
/// The trims are the two bounds — a keyframe makes everything before it
/// unreachable (the join and the resync both read forward from the
/// newest one), and the row cap is the hard bound a runaway GOP cannot
/// grow past.
async fn store_desktop_chunk(
    db: &DurableDb,
    keyframe: bool,
    data: &[u8],
) -> Result<(), DurableObjectError> {
    use base64::Engine as _;
    let data = base64::engine::general_purpose::STANDARD.encode(data);
    let at = now_unix();
    let seq: u64 = sql!(
        db,
        "INSERT INTO desktop_chunks (keyframe, data, at_unix) \
         VALUES ({keyframe}, {data}, {at}) RETURNING seq"
    )
    .fetch_scalar()
    .await
    .map_err(|error| stored(&error))?;
    if keyframe {
        sql!(db, "DELETE FROM desktop_chunks WHERE seq < {seq}")
            .execute()
            .await
            .map_err(|error| stored(&error))?;
    }
    let keep = DESKTOP_CHUNKS_KEPT;
    sql!(
        db,
        "DELETE FROM desktop_chunks WHERE seq < \
         (SELECT seq FROM desktop_chunks ORDER BY seq DESC LIMIT 1 OFFSET {keep})"
    )
    .execute()
    .await
    .map_err(|error| stored(&error))?;
    Ok(())
}

fn room_failed(error: &impl core::fmt::Display) -> ApiError {
    // The problem document reports a generic `session-room-unavailable`
    // for good reason — the inner text can name storage internals — but a
    // refusal nobody can see is a refusal nobody can fix, so the error is
    // logged here rather than only carried to the caller.
    tracing::warn!(%error, "a room call failed");
    ApiError::Room(error.to_string())
}
