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
        return ws.send_json(&ControlToDaemon::Welcome);
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
            "a client may only send `user_message`, `interrupt`, or `terminal_input`",
        );
    }

    announce(ctx, &command).await?;
    forward_to_daemon(ctx.connections(), &command)
}

/// Records and echoes the half of a command that browsers must see.
///
/// A user message is conversation, not control: the browser that sent it
/// already has it, every other browser watching the session does not, and a
/// catch-up that replayed only the agent's side would show answers to
/// questions nobody asked. Recording it is also what gives a turn in the
/// history list the prompt it is named by.
async fn announce(
    ctx: &DurableContext,
    command: &ControlToDaemon,
) -> Result<(), DurableObjectError> {
    let ControlToDaemon::UserMessage { text } = command else {
        return Ok(());
    };
    let event = ClientEvent::UserMessage { text: text.clone() };
    append(ctx.db(), &event).await?;
    broadcast(ctx.connections(), &event)
}

/// Appends a frame to the room's durable stream and caches what the UI
/// reads on connect.
///
/// Only the harness stream is appended: a catch-up must replay what
/// happened, and a usage meter or a capability set is a *current value*, so
/// storing every one of them would grow the table without making the replay
/// any more complete. Those go to KV, newest wins.
async fn record(
    frame: &DaemonToControl,
    db: &DurableDb,
    kv: &DurableKv,
) -> Result<(), DurableObjectError> {
    match frame {
        DaemonToControl::Harness { event } => {
            append(
                db,
                &ClientEvent::Harness {
                    event: event.clone(),
                },
            )
            .await
        }
        DaemonToControl::Started { harness_session_id } => {
            put_latest(kv, KEY_HARNESS_SESSION, harness_session_id).await
        }
        DaemonToControl::Capabilities { capabilities } => {
            put_latest(kv, KEY_CAPABILITIES, capabilities).await
        }
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

/// Appends one event to the room's replayable stream.
async fn append(db: &DurableDb, event: &ClientEvent) -> Result<(), DurableObjectError> {
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
    db.query(
        "CREATE TABLE IF NOT EXISTS events (\
             seq     INTEGER PRIMARY KEY AUTOINCREMENT, \
             json    TEXT    NOT NULL, \
             at_unix INTEGER NOT NULL)",
    )
    .execute()
    .await
    .map_err(|error| stored(&error))?;
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

/// Sends a command to the session's daemon, if it is connected.
///
/// A disconnected daemon is not an error: it is a daemon mid-reconnect, and
/// the user's message is theirs to resend. Dropping it loudly beats
/// queueing it for a daemon that may never come back.
fn forward_to_daemon(
    connections: &DurableConnections,
    command: &ControlToDaemon,
) -> Result<(), DurableObjectError> {
    let json = serde_json::to_string(command)
        .map_err(|error| DurableObjectError::Serialization(error.to_string()))?;
    let daemons = connections.by_tag(ROLE_DAEMON)?;
    if daemons.is_empty() {
        tracing::warn!("dropped a command: this session has no daemon connected");
    }
    for daemon in daemons {
        daemon.send_text(&json)?;
    }
    Ok(())
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

    // A user message forwarded from the Worker is recorded exactly as one
    // that arrived on a browser socket: the route it came in by is not
    // something a replay should be able to tell.
    if let ControlToDaemon::UserMessage { text } = command {
        let event = ClientEvent::UserMessage { text: text.clone() };
        append(db, &event)
            .await
            .map_err(|error| room_failed(&error))?;
        broadcast(connections, &event).map_err(|error| room_failed(&error))?;
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

    forward_to_daemon(connections, command).map_err(|error| room_failed(&error))?;
    if let Some(event) = echo {
        broadcast(connections, &event).map_err(|error| room_failed(&error))?;
    }
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
