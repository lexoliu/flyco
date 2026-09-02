//! The session room: one Durable Object per live session.
//!
//! A session's daemon holds one outbound hibernating WebSocket tagged
//! [`ROLE_DAEMON`]; every browser watching the session holds one tagged
//! [`ROLE_CLIENT`]. The room forwards between the two tags, appends the
//! harness stream to its own SQLite so a reconnecting browser can catch up,
//! and is the single writer for everything about a live session.
//!
//! # What the room is, and is not, allowed to touch
//!
//! A Durable Object cannot reach D1 or the Worker's KV. Every check that
//! needs them — does this credential exist, does this user own this session,
//! is this approval already decided — therefore happens in the *Worker*
//! before it forwards anything here. What arrives at the room is already
//! authenticated, and says so in internal headers ([`HEADER_ROLE`],
//! [`HEADER_SESSION`]) that only a same-Worker call can set. The room
//! refuses a request without them rather than guessing.
//!
//! # The daemon's mailbox
//!
//! A daemon is not always connected — it is being provisioned, it is
//! reconnecting, its machine was evicted — and a user message that arrives
//! in that window is *conversation*, not control: it is the whole point of
//! the session, and the user has no way to know it was thrown away. So
//! every user message is appended to the room's stream (as it already was,
//! for replay) and a **delivery cursor** records how far down that stream
//! the daemon has been told about. A daemon that says `Hello` is sent
//! everything past the cursor, in order, before anything else; a message
//! that arrives while it is connected is forwarded and the cursor moves
//! with it. The prompt `POST /v1/sessions` carries reaches the agent by
//! exactly this path: it is written minutes before the machine exists.
//!
//! Every other command is still dropped when nobody is listening, and that
//! is not an oversight — an interrupt, a compaction or a terminal keystroke
//! held for a daemon that reconnects an hour later would arrive as an
//! instruction about a turn that no longer exists.
//!
//! # Why the state lives outside the struct
//!
//! [`SessionRoom`] is empty. That is not an oversight: the room's state is
//! its `events` table, its KV, and its sockets' attachments, all of which
//! survive hibernation on their own. A field would be a fourth copy of the
//! same facts, re-serialized on every frame, and the first one to drift.

use flyco_core::{ClientEvent, ControlToDaemon, DaemonToControl, RepoStatus, SessionId};
use serde::{Deserialize, Serialize};
use skyzen::durable::{
    DurableConnections, DurableContext, DurableObject, DurableObjectError, WebSocketConnection,
    WebSocketEvent,
};
use skyzen::extract::Query;
use skyzen::routing::{CreateRouteNode, Route, Router};
use skyzen::sql;
use skyzen::utils::Json;
use skyzen_services::durable::{DurableDb, DurableKv};

use crate::ApiError;
use crate::clock::now_unix;
use crate::extract::Headers;
use crate::problem::Outcome;
use crate::respond::NoContent;

/// Tag on the daemon's socket. Exactly one is expected at a time.
pub const ROLE_DAEMON: &str = "daemon";

/// Tag on every browser's socket.
pub const ROLE_CLIENT: &str = "client";

/// Prefix of the tag carrying the room's session id.
///
/// The id has to survive hibernation to validate a daemon's `Hello`, and a
/// tag is the one place it can live without an I/O round trip on every
/// frame.
const SESSION_TAG_PREFIX: &str = "session:";

/// Names the role of a Worker→room call.
pub const HEADER_ROLE: &str = "x-flyco-role";

/// Names the session a Worker→room call belongs to.
pub const HEADER_SESSION: &str = "x-flyco-session";

/// Marks a Worker→room call as internal rather than relayed.
pub const HEADER_INTERNAL: &str = "x-flyco-internal";

/// Value [`HEADER_INTERNAL`] carries.
pub const INTERNAL: &str = "1";

/// Most events one catch-up page returns.
pub const EVENT_PAGE_LIMIT: u32 = 500;

/// Close code for a peer that broke the protocol.
///
/// RFC 6455 §7.4.1 1008 "policy violation": the frame was well-formed
/// WebSocket, the room simply refuses to speak to whoever sent it.
const CLOSE_POLICY: u16 = 1008;

/// What a daemon socket carries once it has been greeted.
///
/// Presence is the handshake: a socket with no attachment has not said
/// `Hello` yet, and the room refuses everything else until it does. The
/// attachment survives hibernation, so a woken room does not re-handshake.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Greeted {
    /// Protocol version the daemon declared and the room accepted.
    protocol_version: u32,
}

/// One stored event, as the catch-up API serves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct StoredEvent {
    /// Monotonic position in the room's stream. Pass the last one back as
    /// `after` to continue.
    pub seq: u64,
    /// The [`ClientEvent`] this position holds.
    pub event: serde_json::Value,
    /// When the room recorded it, seconds since the Unix epoch.
    pub at_unix: u64,
}

/// A page of the room's event tail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct EventPage {
    /// The events, oldest first.
    pub events: Vec<StoredEvent>,
    /// Whether more events exist past the last one returned.
    pub more: bool,
}

/// Query of the room's catch-up route.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
pub struct EventCursor {
    /// Return events strictly after this position. Omitted starts at the
    /// beginning of the room's history.
    pub after: Option<u64>,
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

/// The relay room for one session.
#[derive(Debug, Default, Serialize, Deserialize)]
#[skyzen::durable_object]
pub struct SessionRoom;

impl DurableObject for SessionRoom {
    fn fetch(&mut self) -> Router {
        Route::new((
            "/relay/daemon".at(accept_daemon),
            "/relay/client".at(accept_client),
            "/internal/command".post(run_command),
            "/internal/broadcast".post(run_broadcast),
            "/internal/events".at(read_events),
            "/internal/repo-status".at(read_repo_status),
        ))
        .build()
    }

    async fn websocket(
        &mut self,
        ws: &WebSocketConnection,
        event: WebSocketEvent,
        ctx: &DurableContext,
    ) -> Result<(), DurableObjectError> {
        let WebSocketEvent::Message(message) = event else {
            // A close or an error needs no bookkeeping: Cloudflare has
            // already removed the socket from the connection set, so the
            // next broadcast simply reaches one fewer peer.
            log_disconnect(&event);
            return Ok(());
        };

        let Some(text) = message.into_text() else {
            return refuse(ws, "the relay carries JSON text frames only");
        };

        match role_of(ws)? {
            Role::Daemon => on_daemon_frame(ws, &text, ctx).await,
            Role::Client => on_client_frame(ws, &text, ctx).await,
        }
    }
}

/// Which side of the relay a socket is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The session's `flycod`.
    Daemon,
    /// A browser watching the session.
    Client,
}

impl Role {
    /// The role named by an internal header value or a socket tag.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            ROLE_DAEMON => Some(Self::Daemon),
            ROLE_CLIENT => Some(Self::Client),
            _ => None,
        }
    }

    /// The tag sockets of this role carry.
    ///
    /// Doubles as the [`HEADER_ROLE`] value and as the last segment of the
    /// room's relay path, so the three can never name different things.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Daemon => ROLE_DAEMON,
            Self::Client => ROLE_CLIENT,
        }
    }
}

fn log_disconnect(event: &WebSocketEvent) {
    match event {
        WebSocketEvent::Close { code, reason, .. } => {
            tracing::info!(code, reason, "a relay peer disconnected");
        }
        WebSocketEvent::Error(error) => tracing::warn!(error, "a relay socket failed"),
        WebSocketEvent::Message(_) => unreachable!("messages are handled before this point"),
    }
}

/// Closes a misbehaving peer and reports why.
///
/// Fast fail: a peer that sent something the protocol does not allow is
/// disconnected rather than ignored, so a broken daemon or a hostile client
/// cannot sit on the room quietly doing nothing useful.
fn refuse(ws: &WebSocketConnection, reason: &str) -> Result<(), DurableObjectError> {
    tracing::warn!(reason, "closing a relay peer");
    ws.close(CLOSE_POLICY, reason)
}

/// The role a socket was accepted with.
fn role_of(ws: &WebSocketConnection) -> Result<Role, DurableObjectError> {
    ws.tags()?
        .iter()
        .find_map(|tag| Role::parse(tag))
        .ok_or_else(|| {
            DurableObjectError::Runtime("a relay socket was accepted without a role tag".to_owned())
        })
}

/// The session id a socket was accepted for.
fn session_of(ws: &WebSocketConnection) -> Result<SessionId, DurableObjectError> {
    ws.tags()?
        .iter()
        .find_map(|tag| tag.strip_prefix(SESSION_TAG_PREFIX)?.parse().ok())
        .ok_or_else(|| {
            DurableObjectError::Runtime(
                "a relay socket was accepted without a session tag".to_owned(),
            )
        })
}

/// Handles one frame from the daemon.
async fn on_daemon_frame(
    ws: &WebSocketConnection,
    text: &str,
    ctx: &DurableContext,
) -> Result<(), DurableObjectError> {
    let Ok(frame) = serde_json::from_str::<DaemonToControl>(text) else {
        return refuse(
            ws,
            "the daemon sent a frame this protocol version does not define",
        );
    };

    let greeted = ws.attachment::<Greeted>()?;
    if let DaemonToControl::Hello {
        protocol_version,
        session,
    } = frame
    {
        if protocol_version != flyco_core::WIRE_PROTOCOL_VERSION {
            tracing::warn!(
                daemon = protocol_version,
                control = flyco_core::WIRE_PROTOCOL_VERSION,
                "refusing a daemon that speaks another wire protocol version"
            );
            return refuse(ws, "wire protocol version mismatch");
        }
        if session != session_of(ws)? {
            tracing::warn!(%session, "refusing a daemon that greeted the wrong room");
            return refuse(ws, "this daemon belongs to another session");
        }
        ws.set_attachment(&Greeted { protocol_version })?;
        ws.send_json(&ControlToDaemon::Welcome)?;
        // After the welcome and before anything else: the daemon has to
        // know what it missed before it is told what is happening now. The
        // machine it is running on comes first of all — a daemon told its
        // machine changed *after* a user message would answer that message
        // believing it is somewhere else.
        replay_held_commands(ws, ctx.db()).await?;
        return replay_mailbox(ws, ctx.db()).await;
    }

    if greeted.is_none() {
        return refuse(ws, "the first frame must be `hello`");
    }

    record(&frame, ctx.db(), ctx.kv()).await?;
    // `Hello` is the only frame browsers never see, and it was handled
    // above; reaching the `None` arm would mean the mapping grew a hole.
    ClientEvent::from_daemon(frame).map_or_else(
        || {
            Err(DurableObjectError::Runtime(
                "a daemon frame past the handshake had no client form".to_owned(),
            ))
        },
        |event| broadcast(ctx.connections(), &event),
    )
}

/// Handles one frame from a browser.
async fn on_client_frame(
    ws: &WebSocketConnection,
    text: &str,
    ctx: &DurableContext,
) -> Result<(), DurableObjectError> {
    let Ok(command) = serde_json::from_str::<ControlToDaemon>(text) else {
        return refuse(
            ws,
            "the client sent a frame this protocol version does not define",
        );
    };
    if !command.is_client_command() {
        return refuse(
            ws,
            "a client may only send `user_message`, `interrupt`, `compact`, or `terminal_input`",
        );
    }

    if let ControlToDaemon::UserMessage { text } = &command {
        return deliver_user_message(ctx.db(), ctx.connections(), text).await;
    }
    forward_to_daemon(ctx.connections(), &command).map(drop)
}

/// Records a user message, echoes it to browsers, and gets it to the daemon.
///
/// The one path a user message takes, whichever door it came in by — a
/// browser's socket or the Worker forwarding a REST call — because the route
/// a message arrived on is not something a replay, or the agent, should be
/// able to tell.
///
/// Recording comes first: the message is conversation. The browser that
/// typed it already has it, every other browser watching the session does
/// not, and a catch-up that replayed only the agent's side would show
/// answers to questions nobody asked. It is also what gives a turn in the
/// history list the prompt it is named by.
///
/// The cursor moves only when the daemon actually took the frame. A message
/// left behind it is redelivered by [`replay_mailbox`] on the next `Hello`.
async fn deliver_user_message(
    db: &DurableDb,
    connections: &DurableConnections,
    text: &str,
) -> Result<(), DurableObjectError> {
    let event = ClientEvent::UserMessage {
        text: text.to_owned(),
    };
    let seq = append(db, &event).await?;
    // The mailbox is an index into the stream, written under the position
    // the event just took, so a redelivery can never reorder the
    // conversation or invent a message the replay does not also carry.
    let owned = text.to_owned();
    sql!(
        db,
        "INSERT INTO user_messages (seq, text) VALUES ({seq}, {owned})"
    )
    .execute()
    .await
    .map_err(|error| stored(&error))?;
    broadcast(connections, &event)?;

    let command = ControlToDaemon::UserMessage {
        text: text.to_owned(),
    };
    if forward_to_daemon(connections, &command)? {
        set_delivered(db, seq).await?;
    } else {
        tracing::info!(
            seq,
            "held a user message for a daemon that is not connected yet"
        );
    }
    Ok(())
}

/// Appends a frame to the room's durable stream and caches what the UI
/// reads on connect.
///
/// Only what *happened* is appended — the harness stream and the
/// provisioning timeline: a catch-up must replay events, and a usage meter
/// or a capability set is a *current value*, so storing every one of them
/// would grow the table without making the replay any more complete. Those
/// go to KV, newest wins.
async fn record(
    frame: &DaemonToControl,
    db: &DurableDb,
    kv: &DurableKv,
) -> Result<(), DurableObjectError> {
    match frame {
        DaemonToControl::Harness { event } => append(
            db,
            &ClientEvent::Harness {
                event: event.clone(),
            },
        )
        .await
        .map(drop),
        DaemonToControl::Started { harness_session_id } => {
            put_latest(kv, KEY_HARNESS_SESSION, harness_session_id).await
        }
        DaemonToControl::Capabilities { capabilities } => {
            put_latest(kv, KEY_CAPABILITIES, capabilities).await
        }
        DaemonToControl::ProvisioningStage { stage, at_unix } => append(
            db,
            &ClientEvent::ProvisioningStage {
                stage: *stage,
                at_unix: *at_unix,
            },
        )
        .await
        .map(drop),
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
        .map(drop),
        DaemonToControl::RepoDirty { summary } => {
            // The daemon is the only thing that can see the working tree, and
            // it reports the whole `git status --short` output rather than a
            // flag, so an empty summary is the clean tree and the absence of
            // any report is "nobody has looked" — which is what
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
        }
        _ => Ok(()),
    }
}

/// Appends one event to the room's replayable stream, and answers with the
/// position it took.
///
/// The position is read back rather than counted in the struct: the room
/// hibernates, and `AUTOINCREMENT` is the only thing here that stays
/// monotonic across that and across two writes racing in one wake-up. A
/// user message's position is what the delivery cursor is compared against.
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

    // Single-threaded per room: nothing else can have appended between the
    // insert above and this read.
    sql!(db, "SELECT seq FROM events ORDER BY seq DESC LIMIT 1")
        .fetch_scalar_optional()
        .await
        .map_err(|error| stored(&error))?
        .ok_or_else(|| DurableObjectError::Runtime("an appended event had no position".to_owned()))
}

/// One user message waiting for a daemon to come and take it.
#[derive(Debug, skyzen::FromRow)]
struct PendingRow {
    seq: u64,
    text: String,
}

/// Sends the daemon every user message recorded since it was last told
/// anything, oldest first, and moves the cursor past them.
///
/// Sent on the greeting rather than on the socket's accept, because the
/// accept is a `101` the room answers before any frame may be written and
/// before the daemon has proved it speaks this protocol version. `Hello` is
/// the first moment a daemon exists as far as the room is concerned.
async fn replay_mailbox(
    ws: &WebSocketConnection,
    db: &DurableDb,
) -> Result<(), DurableObjectError> {
    ensure_schema(db).await?;
    let cursor = delivered_through(db).await?;
    let pending: Vec<PendingRow> = sql!(
        db,
        "SELECT seq, text FROM user_messages WHERE seq > {cursor} ORDER BY seq"
    )
    .fetch_all()
    .await
    .map_err(|error| stored(&error))?;

    let Some(last) = pending.last().map(|row| row.seq) else {
        return Ok(());
    };
    let held = pending.len();
    for row in pending {
        ws.send_json(&ControlToDaemon::UserMessage { text: row.text })?;
    }
    set_delivered(db, last).await?;
    tracing::info!(held, through = last, "replayed a daemon's mailbox");
    Ok(())
}

/// One command kept for a daemon that was not there to take it.
#[derive(Debug, skyzen::FromRow)]
struct HeldRow {
    seq: u64,
    /// The command, as JSON. Untyped for the same reason a stored event is:
    /// the room hands back what it was given, including a variant this build
    /// of the Worker does not know how to read.
    json: String,
}

/// Keeps a command for the daemon to take when it comes back.
///
/// Only the commands [`ControlToDaemon::survives_a_disconnect`] admits reach
/// here, and there is exactly one: the machine changing, which is a state
/// rather than an instant and which happens precisely while no daemon is
/// connected because the change restarted the machine.
async fn hold_for_daemon(
    db: &DurableDb,
    command: &ControlToDaemon,
) -> Result<(), DurableObjectError> {
    let json = serde_json::to_string(command)
        .map_err(|error| DurableObjectError::Serialization(error.to_string()))?;
    ensure_schema(db).await?;
    sql!(db, "INSERT INTO held_commands (json) VALUES ({json})")
        .execute()
        .await
        .map_err(|error| stored(&error))?;
    Ok(())
}

/// Hands a freshly greeted daemon everything that was kept for it, oldest
/// first, and forgets it.
///
/// Deleted rather than kept behind a cursor, unlike the user-message
/// mailbox: these commands are not part of the conversation and nothing
/// replays them, so a row that has been delivered has no further use. A
/// delete that runs after a successful write is what makes delivery
/// at-most-once here — and a machine change delivered twice would tell the
/// agent its processes died twice.
async fn replay_held_commands(
    ws: &WebSocketConnection,
    db: &DurableDb,
) -> Result<(), DurableObjectError> {
    ensure_schema(db).await?;
    let held: Vec<HeldRow> = sql!(db, "SELECT seq, json FROM held_commands ORDER BY seq")
        .fetch_all()
        .await
        .map_err(|error| stored(&error))?;

    let Some(last) = held.last().map(|row| row.seq) else {
        return Ok(());
    };
    let count = held.len();
    for row in held {
        ws.send_text(&row.json)?;
    }
    sql!(db, "DELETE FROM held_commands WHERE seq <= {last}")
        .execute()
        .await
        .map_err(|error| stored(&error))?;
    tracing::info!(
        count,
        "handed a reconnected daemon the commands kept for it"
    );
    Ok(())
}

/// The stream position through which the daemon has been told everything.
async fn delivered_through(db: &DurableDb) -> Result<u64, DurableObjectError> {
    Ok(sql!(db, "SELECT seq FROM delivery WHERE id = 0")
        .fetch_scalar_optional()
        .await
        .map_err(|error| stored(&error))?
        .unwrap_or(0))
}

/// Records that the daemon has now been told everything through `seq`.
///
/// Monotonic: the daemon is greeted before its mailbox is replayed, so a
/// message arriving during the replay's own awaits is forwarded at once and
/// moves the cursor past it. The replay finishing afterwards with an older
/// position must not pull the cursor back, or that message would be
/// delivered twice on the next `Hello`.
async fn set_delivered(db: &DurableDb, seq: u64) -> Result<(), DurableObjectError> {
    sql!(
        db,
        "INSERT INTO delivery (id, seq) VALUES (0, {seq}) \
         ON CONFLICT (id) DO UPDATE SET seq = max(excluded.seq, delivery.seq)"
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

/// Creates the event table if this is the room's first write.
///
/// `AUTOINCREMENT` rather than a counter in the struct: the sequence has to
/// be monotonic across hibernation and across two writes racing in one
/// wake-up, and the database is the only thing here that guarantees both.
async fn ensure_schema(db: &DurableDb) -> Result<(), DurableObjectError> {
    for statement in [
        "CREATE TABLE IF NOT EXISTS events (\
             seq     INTEGER PRIMARY KEY AUTOINCREMENT, \
             json    TEXT    NOT NULL, \
             at_unix INTEGER NOT NULL)",
        // The daemon's mailbox: one row per user message, keyed by the
        // position that message holds in `events`. An index rather than a
        // queue — nothing is deleted from it — so the cursor and the stream
        // are talking about the same positions and a redelivery can never
        // reorder the conversation.
        "CREATE TABLE IF NOT EXISTS user_messages (\
             seq  INTEGER PRIMARY KEY REFERENCES events(seq), \
             text TEXT    NOT NULL)",
        // Exactly one row, because a room has exactly one daemon. The
        // CHECK is what makes that structural rather than a convention.
        "CREATE TABLE IF NOT EXISTS delivery (\
             id  INTEGER PRIMARY KEY CHECK (id = 0), \
             seq INTEGER NOT NULL)",
        // Commands written while no daemon was listening. A queue rather
        // than an index, because these are not conversation: a row is
        // deleted the moment a daemon has taken it.
        "CREATE TABLE IF NOT EXISTS held_commands (\
             seq  INTEGER PRIMARY KEY AUTOINCREMENT, \
             json TEXT    NOT NULL)",
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

/// Sends an event to every browser watching this room.
fn broadcast(
    connections: &DurableConnections,
    event: &ClientEvent,
) -> Result<(), DurableObjectError> {
    let json = serde_json::to_string(event)
        .map_err(|error| DurableObjectError::Serialization(error.to_string()))?;
    for client in connections.by_tag(ROLE_CLIENT)? {
        client.send_text(&json)?;
    }
    Ok(())
}

/// Sends a command to the session's daemon, and says whether one took it.
///
/// A daemon is a *greeted* socket, not an open one: a connection that has
/// not said `Hello` has not agreed a protocol version, and writing a command
/// into it would be speaking before either side knows the other's language.
/// The handshake is a moment away, and [`replay_mailbox`] hands over
/// everything written during it.
///
/// A disconnected daemon is not an error — it is a daemon mid-reconnect, or
/// a machine that does not exist yet. What happens next depends on the
/// command: a user message is held in the mailbox and redelivered on the
/// next `Hello`, and everything else is dropped with a warning, because an
/// interrupt or a keystroke replayed into a later turn would be an
/// instruction about something that is no longer happening.
fn forward_to_daemon(
    connections: &DurableConnections,
    command: &ControlToDaemon,
) -> Result<bool, DurableObjectError> {
    let json = serde_json::to_string(command)
        .map_err(|error| DurableObjectError::Serialization(error.to_string()))?;
    let mut delivered = false;
    for daemon in connections.by_tag(ROLE_DAEMON)? {
        if daemon.attachment::<Greeted>()?.is_none() {
            continue;
        }
        daemon.send_text(&json)?;
        delivered = true;
    }
    Ok(delivered)
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

/// What an accepted relay socket answers with.
///
/// On the Worker this is the hibernation upgrade itself — skyzen's runtime
/// calls its `Responder` with the real request, which is where the Durable
/// Object state it needs lives. Natively there is nothing to accept, so the
/// alias is a plain response and [`accept`] only ever returns an error into
/// it.
#[cfg(target_arch = "wasm32")]
type Accepted = skyzen::durable::HibernationWebSocketUpgrade;

/// See the `wasm32` alias above.
#[cfg(not(target_arch = "wasm32"))]
type Accepted = skyzen::Response;

/// Accepts the daemon's hibernating socket.
async fn accept_daemon(headers: Headers) -> Outcome<Accepted> {
    accept(&headers, Role::Daemon).into()
}

/// Accepts a browser's hibernating socket.
async fn accept_client(headers: Headers) -> Outcome<Accepted> {
    accept(&headers, Role::Client).into()
}

/// Runs a control-plane command against the room.
///
/// The Worker calls this after it has done the durable half of the work —
/// recording an approval decision in D1, applying a budget signal — so the
/// live half can never disagree with what was persisted.
async fn run_command(
    headers: Headers,
    Json(command): Json<ControlToDaemon>,
    connections: DurableConnections,
    db: DurableDb,
) -> Outcome<NoContent> {
    dispatch_command(&headers, &command, &connections, &db)
        .await
        .into()
}

async fn dispatch_command(
    headers: &Headers,
    command: &ControlToDaemon,
    connections: &DurableConnections,
    db: &DurableDb,
) -> Result<NoContent, ApiError> {
    internal(headers)?;

    // A user message forwarded from the Worker takes exactly the path one
    // that arrived on a browser socket takes — recorded, echoed, and either
    // delivered or held for the daemon.
    if let ControlToDaemon::UserMessage { text } = command {
        deliver_user_message(db, connections, text)
            .await
            .map_err(|error| room_failed(&error))?;
        return Ok(NoContent);
    }

    let echo = match &command {
        ControlToDaemon::ApprovalDecision { id, decision } => Some(ClientEvent::ApprovalDecided {
            id: *id,
            decision: *decision,
        }),
        ControlToDaemon::Archive { .. } => Some(ClientEvent::SessionStateChanged {
            state: flyco_core::SessionState::Archived,
        }),
        _ => None,
    };

    if !forward_to_daemon(connections, command).map_err(|error| room_failed(&error))? {
        if command.survives_a_disconnect() {
            hold_for_daemon(db, command)
                .await
                .map_err(|error| room_failed(&error))?;
            tracing::info!(
                ?command,
                "held a command for a daemon that is not connected"
            );
        } else {
            tracing::warn!(
                ?command,
                "dropped a command: this session has no daemon connected"
            );
        }
    }
    if let Some(event) = echo {
        broadcast(connections, &event).map_err(|error| room_failed(&error))?;
    }
    Ok(NoContent)
}

/// Appends a control-plane event to the room's stream and shows it to every
/// browser watching.
///
/// The other half of [`run_command`]: a command is something the *daemon*
/// must act on, and this is something only the user needs to see. The
/// provisioning timeline is the case that needs it — the queue knows the
/// machine was reserved minutes before any daemon exists to say so — and it
/// is appended rather than only broadcast, because a browser opened after
/// provisioning finished must still be able to replay how long each stage
/// took.
async fn run_broadcast(
    headers: Headers,
    Json(event): Json<ClientEvent>,
    connections: DurableConnections,
    db: DurableDb,
) -> Outcome<NoContent> {
    dispatch_broadcast(&headers, &event, &connections, &db)
        .await
        .into()
}

async fn dispatch_broadcast(
    headers: &Headers,
    event: &ClientEvent,
    connections: &DurableConnections,
    db: &DurableDb,
) -> Result<NoContent, ApiError> {
    internal(headers)?;
    append(db, event)
        .await
        .map_err(|error| room_failed(&error))?;
    broadcast(connections, event).map_err(|error| room_failed(&error))?;
    Ok(NoContent)
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

    // One row past the page tells the caller whether to come back, without
    // a second `COUNT(*)` over a table that only grows.
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

fn room_failed(error: &DurableObjectError) -> ApiError {
    ApiError::Room(error.to_string())
}

/// Turns a validated upgrade request into an accepted hibernating socket.
///
/// The tags are the socket's whole identity for the rest of its life: the
/// role decides which way frames flow, and the session id is what a `Hello`
/// is checked against after the room has hibernated and forgotten
/// everything else.
///
/// # Errors
///
/// Returns [`ApiError::Room`] if the internal headers are absent or name
/// another role.
#[cfg(target_arch = "wasm32")]
fn accept(headers: &Headers, expected: Role) -> Result<Accepted, ApiError> {
    let session = admit(headers, expected)?;
    Ok(Accepted::new()
        .tag(expected.tag())
        .tag(format!("{SESSION_TAG_PREFIX}{session}")))
}

/// Native builds do not reach this route with a socket to accept.
///
/// The simulator delivers WebSocket events now, and
/// `HibernationWebSocketUpgrade` responds on both targets — but an upgrade
/// only becomes a socket if the *browser's own* handshake request reaches
/// the object, and `Rooms::upgrade` on the Worker forwards exactly that.
/// Natively the control plane has no path that carries a client's upgrade
/// into a room, so the route refuses rather than accepting a socket nothing
/// would write to. The room's HTTP routes work natively and are what the
/// tests drive, alongside `websocket` called directly.
///
/// # Errors
///
/// Always: with [`ApiError::Room`] if the internal headers are wrong, and
/// with [`ApiError::RelayUnavailable`] if they are right.
#[cfg(not(target_arch = "wasm32"))]
fn accept(headers: &Headers, expected: Role) -> Result<Accepted, ApiError> {
    admit(headers, expected)?;
    Err(ApiError::RelayUnavailable(
        "a native control plane does not forward relay upgrades into a session room",
    ))
}

/// Checks the internal headers of an upgrade forwarded from the Worker.
fn admit(headers: &Headers, expected: Role) -> Result<SessionId, ApiError> {
    let role = headers
        .get(HEADER_ROLE)
        .and_then(Role::parse)
        .ok_or_else(|| ApiError::Room("a relay upgrade named no role".to_owned()))?;
    if role != expected {
        return Err(ApiError::Room(
            "a relay upgrade reached the route of another role".to_owned(),
        ));
    }
    headers
        .get(HEADER_SESSION)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| ApiError::Room("a relay upgrade named no session".to_owned()))
}
