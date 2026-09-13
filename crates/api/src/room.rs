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
    ClientEvent, ControlToDaemon, DaemonToControl, MessageOrigin, RepoStatus, ShellRunId,
    WorkdirRequestId,
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
            "/internal/workdir"
                .post(ask_workdir)
                .get(collect_workdir_reply),
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
        // epoch's stream is the one the room answers now.
        if row.epoch != feed.epoch {
            return Ok(crate::sse::Poll::End);
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
    if rows.is_empty() {
        return Ok(crate::sse::Poll::Idle);
    }
    feed.cursor = rows.last().map_or(feed.cursor, |row| row.seq);
    Ok(crate::sse::Poll::Emit(
        rows.iter()
            .map(|row| command_event(Some(row.seq), &row.json))
            .collect(),
    ))
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
        DaemonToControl::RepoDirty { summary } => {
            // The daemon is the only thing that can see the working tree,
            // and it reports the whole `git status --short` output rather
            // than a flag, so an empty summary is the clean tree and the
            // absence of any report is "nobody has looked" — which is what
            // `GET /v1/sessions/{id}/repo-status` refuses to answer.
            put_latest(
                kv,
                KEY_REPO_STATUS,
                &RepoStatus {
                    dirty: !summary.trim().is_empty(),
                    summary: summary.clone(),
                },
            )
            .await
            .map(|()| None)
        }
        _ => Ok(None),
    }
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

/// Creates the room's tables if this is its first write.
///
/// `AUTOINCREMENT` rather than a counter in the struct: the sequences
/// have to be monotonic across the object being rebuilt around every
/// event, and the database is the only thing here that guarantees it.
async fn ensure_schema(db: &DurableDb) -> Result<(), DurableObjectError> {
    for statement in [
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
        // id the Worker minted for it and deleted the moment that Worker
        // collects it. A table rather than KV because these expire: the
        // sweep in `store_workdir_reply` needs to find rows by age, which
        // is a query and not a key.
        "CREATE TABLE IF NOT EXISTS workdir_replies (\
             id      TEXT    PRIMARY KEY, \
             json    TEXT    NOT NULL, \
             at_unix INTEGER NOT NULL)",
    ] {
        db.query(statement)
            .execute()
            .await
            .map_err(|error| stored(&error))?;
    }
    Ok(())
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
/// The Worker that asked polls for a few seconds and then gives up, so
/// anything older than this is an answer nobody came back for — a
/// browser that closed the tab, or a request that timed out. Swept on the
/// next write rather than on a timer: a room that is answering questions
/// is exactly the room that has rows to sweep.
const WORKDIR_REPLY_TTL_SECONDS: u64 = 120;

/// Query of the route that collects an answered workdir question.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
pub struct WorkdirCursor {
    /// The request whose answer is being collected.
    pub id: Option<String>,
}

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

/// Puts one question about the checkout to the session's daemon.
///
/// Answers `503` when no daemon is attached, which is the whole reason
/// this is not [`run_command`]: a browser waiting for a listing has to be
/// told at once that there is nothing to read it, rather than waiting out
/// the poll for an answer that is never coming.
async fn ask_workdir(
    headers: Headers,
    Json(command): Json<ControlToDaemon>,
    db: DurableDb,
) -> Outcome<NoContent> {
    ask(&headers, &command, &db).await.into()
}

async fn ask(
    headers: &Headers,
    command: &ControlToDaemon,
    db: &DurableDb,
) -> Result<NoContent, ApiError> {
    internal(headers)?;
    if !matches!(command, ControlToDaemon::InspectWorkdir { .. }) {
        return Err(ApiError::Room(
            "the workdir route was given something other than a question about the checkout"
                .to_owned(),
        ));
    }
    ensure_schema(db)
        .await
        .map_err(|error| room_failed(&error))?;
    if !daemon_live(db).await.map_err(|error| room_failed(&error))? {
        return Err(ApiError::SessionDaemonOffline);
    }
    queue_command(db, command)
        .await
        .map_err(|error| room_failed(&error))?;
    Ok(NoContent)
}

/// Collects an answer the daemon has already sent, if it has.
///
/// Single use: the row is deleted as it is read, because the Worker
/// holding the browser's request is the only caller that will ever want
/// it.
async fn collect_workdir_reply(
    headers: Headers,
    Query(cursor): Query<WorkdirCursor>,
    db: DurableDb,
) -> Outcome<Json<WorkdirReply>> {
    collect(&headers, cursor.id.as_deref(), &db).await.into()
}

async fn collect(
    headers: &Headers,
    id: Option<&str>,
    db: &DurableDb,
) -> Result<Json<WorkdirReply>, ApiError> {
    internal(headers)?;
    let id =
        id.ok_or_else(|| ApiError::Room("a workdir collection named no request".to_owned()))?;
    ensure_schema(db)
        .await
        .map_err(|error| room_failed(&error))?;

    let json: Option<String> = sql!(db, "SELECT json FROM workdir_replies WHERE id = {id}")
        .fetch_scalar_optional()
        .await
        .map_err(|error| ApiError::Room(error.to_string()))?;
    let Some(json) = json else {
        return Err(ApiError::WorkdirNotAnsweredYet);
    };
    sql!(db, "DELETE FROM workdir_replies WHERE id = {id}")
        .execute()
        .await
        .map_err(|error| ApiError::Room(error.to_string()))?;

    serde_json::from_str(&json)
        .map(Json)
        .map_err(|error| ApiError::Room(format!("a stored workdir reply did not parse: {error}")))
}

fn room_failed(error: &impl core::fmt::Display) -> ApiError {
    ApiError::Room(error.to_string())
}
