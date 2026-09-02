//! The host room: one Durable Object per enrolled machine.
//!
//! A host's `flycod host` holds one outbound hibernating WebSocket tagged
//! [`ROLE_HOST`]. The room is the only thing in flyco that can see that
//! socket, which makes it two things: the way container work reaches the
//! machine, and the authority on whether the machine is there at all.
//!
//! # What the room is, and is not, allowed to touch
//!
//! A Durable Object cannot reach D1 or the Worker's KV — the same rule the
//! session room lives under (see [`crate::room`]). So:
//!
//! * **Liveness** lives here and nowhere else. `hosts.state` in D1 is what
//!   the control plane last *recorded*; [`HostStatus`] is what is true now,
//!   and every Worker read of a host refreshes the row from it
//!   ([`crate::hosts::refresh`]).
//! * **Job results** arrive here as a frame *and* at the Worker over REST.
//!   The frame lets the room forget the job it was holding; the REST call is
//!   what completes the machine row, because only the Worker can write D1.
//!   Two routes for one fact, exactly as a session daemon's spot notice
//!   travels twice and for the same reason.
//!
//! # The mailbox
//!
//! A host is not always connected — the machine is rebooting, the unit is
//! restarting, somebody closed the lid — and container work that arrives in
//! that window is not a notification to drop: a `Run` lost is a session that
//! never gets its container, or a container nobody ever removes. So every
//! job is written to the room's own SQLite before it is forwarded, and stays
//! there until the host answers it. A host that says `Hello` is handed
//! everything outstanding, oldest first.
//!
//! Delivery is therefore at-least-once, which is what a container job is
//! written to survive: creating one removes any container of that name
//! first, and stopping, starting or removing a container that is already in
//! that state is what `podman` does anyway.

use flyco_core::host::HostFacts;
use flyco_provider::host::{ControlToHost, HostToControl, container_name};
use serde::{Deserialize, Serialize};
use skyzen::durable::{
    DurableConnections, DurableContext, DurableObject, DurableObjectError, WebSocketConnection,
    WebSocketEvent,
};
use skyzen::routing::{CreateRouteNode, Route, Router};
use skyzen::sql;
use skyzen::utils::{Bytes, Json};
use skyzen_services::durable::{DurableDb, DurableKv};

use crate::ApiError;
use crate::extract::Headers;
use crate::problem::Outcome;
use crate::respond::NoContent;
use crate::room::{HEADER_INTERNAL, HEADER_ROLE, INTERNAL};

/// Tag on the host's socket. Exactly one is expected at a time.
pub const ROLE_HOST: &str = "host";

/// Names the host a Worker→room call belongs to.
pub const HEADER_HOST: &str = "x-flyco-host";

/// Prefix of the tag carrying the room's host id.
///
/// The id has to survive hibernation, and a tag is the one place it can live
/// without an I/O round trip on every frame. Only the Worker build sets it:
/// natively there is no socket to accept, so nothing reads it back.
#[cfg(target_arch = "wasm32")]
const HOST_TAG_PREFIX: &str = "host:";

/// Close code for a peer that broke the protocol.
///
/// RFC 6455 §7.4.1 1008 "policy violation": the frame was well-formed
/// WebSocket, the room simply refuses to speak to whoever sent it.
const CLOSE_POLICY: u16 = 1008;

/// KV key holding the facts the machine last reported.
const KEY_FACTS: &str = "host:facts";

/// What a host socket carries once it has greeted the room.
///
/// Presence is the handshake: a socket with no attachment has not said
/// `Hello` yet, and the room writes nothing into it until it does. The
/// attachment survives hibernation, so a woken room does not re-handshake.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Greeted {
    /// When the machine greeted this room, seconds since the Unix epoch.
    at_unix: u64,
}

/// What a host's room knows right now.
///
/// Answered by `/internal/status`, and the only honest source for the first
/// field: a socket that dropped is visible to the Durable Object holding it
/// and to nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HostStatus {
    /// Whether a greeted host socket is attached to this room.
    pub connected: bool,
    /// What the machine last said about itself, if it has ever greeted.
    pub facts: Option<HostFacts>,
    /// How many container jobs are still waiting to be answered.
    pub pending_jobs: u32,
}

/// The relay room for one enrolled host.
#[derive(Debug, Default, Serialize, Deserialize)]
#[skyzen::durable_object]
pub struct HostRoom;

impl DurableObject for HostRoom {
    fn fetch(&mut self) -> Router {
        Route::new((
            "/relay/host".at(accept_host),
            "/internal/command".post(run_command),
            "/internal/status".at(read_status),
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
            // A close needs no bookkeeping here: Cloudflare has already
            // removed the socket from the connection set, so the next
            // `/internal/status` answers `connected: false` on its own.
            log_disconnect(&event);
            return Ok(());
        };

        let Some(text) = message.into_text() else {
            return refuse(ws, "the host relay carries JSON text frames only");
        };

        let Ok(frame) = serde_json::from_str::<HostToControl>(&text) else {
            return refuse(
                ws,
                "the host sent a frame this protocol version does not define",
            );
        };

        let greeted = ws.attachment::<Greeted>()?;
        match frame {
            HostToControl::Hello { facts } => {
                ws.set_attachment(&Greeted {
                    at_unix: crate::clock::now_unix(),
                })?;
                put_facts(ctx.kv(), &facts).await?;
                tracing::info!(hostname = %facts.hostname, "a host greeted its room");
                replay_mailbox(ws, ctx.db()).await
            }
            _ if greeted.is_none() => refuse(ws, "the first frame must be `hello`"),
            HostToControl::JobResult { job_id, outcome } => {
                tracing::info!(
                    machine = %job_id,
                    outcome = ?core::mem::discriminant(&outcome),
                    "a host answered a container job"
                );
                // Keyed by the container the job named, which is how the
                // mailbox holds it: the name is derived from the machine id,
                // so the answer and the job agree without a second column.
                answered(ctx.db(), &container_name(job_id)).await
            }
            // The socket carrying it is the whole content: a hibernating
            // socket that has not been written to for hours is
            // indistinguishable from a machine that was unplugged, and this
            // is what tells them apart.
            HostToControl::Heartbeat => Ok(()),
        }
    }
}

fn log_disconnect(event: &WebSocketEvent) {
    match event {
        WebSocketEvent::Close { code, reason, .. } => {
            tracing::info!(code, reason, "a host relay peer disconnected");
        }
        WebSocketEvent::Error(error) => tracing::warn!(error, "a host relay socket failed"),
        WebSocketEvent::Message(_) => unreachable!("messages are handled before this point"),
    }
}

/// Closes a misbehaving peer and reports why.
fn refuse(ws: &WebSocketConnection, reason: &str) -> Result<(), DurableObjectError> {
    tracing::warn!(reason, "closing a host relay peer");
    ws.close(CLOSE_POLICY, reason)
}

// ── The mailbox ──

/// One job the host has not answered yet.
#[derive(Debug, skyzen::FromRow)]
struct JobRow {
    /// The command, as JSON. Untyped for the same reason a stored session
    /// event is: the room hands back what it was given, including a variant
    /// this build of the Worker does not know how to read.
    json: String,
}

/// Records a job before anybody tries to deliver it.
///
/// Written first and deleted on the answer, so a host that took a job and
/// died before finishing it is handed the same job when it comes back.
async fn hold(
    db: &DurableDb,
    container: &str,
    command: &ControlToHost,
) -> Result<(), DurableObjectError> {
    let json = serde_json::to_string(command)
        .map_err(|error| DurableObjectError::Serialization(error.to_string()))?;
    ensure_schema(db).await?;
    sql!(
        db,
        "INSERT INTO jobs (machine, json) VALUES ({container}, {json})"
    )
    .execute()
    .await
    .map_err(|error| stored(&error))?;
    Ok(())
}

/// Forgets the oldest outstanding job for one container.
///
/// The oldest rather than all of them: a session that was created and then
/// stopped has two jobs on the same container, and the answer to the first
/// says nothing about the second.
async fn answered(db: &DurableDb, container: &str) -> Result<(), DurableObjectError> {
    ensure_schema(db).await?;
    sql!(
        db,
        "DELETE FROM jobs WHERE seq = (SELECT MIN(seq) FROM jobs WHERE machine = {container})"
    )
    .execute()
    .await
    .map_err(|error| stored(&error))?;
    Ok(())
}

/// Hands a freshly greeted host everything still outstanding, oldest first.
///
/// The rows are not deleted here: a job is finished when the host says so,
/// not when it was handed over, so a machine that dies mid-`podman run` is
/// asked again on its next connection.
async fn replay_mailbox(
    ws: &WebSocketConnection,
    db: &DurableDb,
) -> Result<(), DurableObjectError> {
    ensure_schema(db).await?;
    let pending: Vec<JobRow> = sql!(db, "SELECT json FROM jobs ORDER BY seq")
        .fetch_all()
        .await
        .map_err(|error| stored(&error))?;

    if pending.is_empty() {
        return Ok(());
    }
    let held = pending.len();
    for row in pending {
        ws.send_text(&row.json)?;
    }
    tracing::info!(held, "handed a host the container jobs kept for it");
    Ok(())
}

/// Drops every outstanding job.
///
/// A revoked host will never open another socket, so a job left in its
/// mailbox is work that is never going to happen: keeping it would make the
/// room's own status lie about what is pending.
async fn forget_jobs(db: &DurableDb) -> Result<(), DurableObjectError> {
    ensure_schema(db).await?;
    sql!(db, "DELETE FROM jobs")
        .execute()
        .await
        .map_err(|error| stored(&error))?;
    Ok(())
}

async fn pending_jobs(db: &DurableDb) -> Result<u32, DurableObjectError> {
    ensure_schema(db).await?;
    let count: u64 = sql!(db, "SELECT COUNT(*) AS pending FROM jobs")
        .fetch_scalar()
        .await
        .map_err(|error| stored(&error))?;
    Ok(u32::try_from(count).unwrap_or(u32::MAX))
}

/// Creates the mailbox if this is the room's first write.
///
/// `AUTOINCREMENT` rather than a counter in the object: the order jobs were
/// planned in has to survive hibernation, and the database is the only thing
/// here that guarantees it.
async fn ensure_schema(db: &DurableDb) -> Result<(), DurableObjectError> {
    db.query(
        "CREATE TABLE IF NOT EXISTS jobs (\
             seq       INTEGER PRIMARY KEY AUTOINCREMENT, \
             machine   TEXT    NOT NULL, \
             json    TEXT    NOT NULL)",
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

async fn put_facts(kv: &DurableKv, facts: &HostFacts) -> Result<(), DurableObjectError> {
    kv.put_json(KEY_FACTS, facts)
        .await
        .map_err(|error| DurableObjectError::Runtime(error.to_string()))
}

async fn read_facts(kv: &DurableKv) -> Result<Option<HostFacts>, ApiError> {
    kv.get_json::<HostFacts>(KEY_FACTS)
        .await
        .map_err(|error| ApiError::Room(error.to_string()))
}

/// Sends a command to the host, and says whether it took it.
///
/// A host is a *greeted* socket, not an open one: a connection that has not
/// said `Hello` has not identified the machine behind it, and writing a
/// container job into it would be handing somebody's credentials to a peer
/// that has not spoken yet.
fn forward_to_host(
    connections: &DurableConnections,
    command: &ControlToHost,
) -> Result<bool, DurableObjectError> {
    let json = serde_json::to_string(command)
        .map_err(|error| DurableObjectError::Serialization(error.to_string()))?;
    let mut delivered = false;
    for host in connections.by_tag(ROLE_HOST)? {
        if host.attachment::<Greeted>()?.is_none() {
            continue;
        }
        host.send_text(&json)?;
        delivered = true;
    }
    Ok(delivered)
}

/// Whether a greeted host socket is attached right now.
fn is_connected(connections: &DurableConnections) -> Result<bool, DurableObjectError> {
    for host in connections.by_tag(ROLE_HOST)? {
        if host.attachment::<Greeted>()?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

// ── The room's own HTTP surface ──

/// Reads the internal header a Worker→room call must carry.
fn internal(headers: &Headers) -> Result<(), ApiError> {
    if headers.get(HEADER_INTERNAL) == Some(INTERNAL) {
        Ok(())
    } else {
        Err(ApiError::Room(
            "a host room route was reached without the internal marker".to_owned(),
        ))
    }
}

/// What an accepted relay socket answers with. See [`crate::room`].
#[cfg(target_arch = "wasm32")]
type Accepted = skyzen::durable::HibernationWebSocketUpgrade;

/// See the `wasm32` alias above.
#[cfg(not(target_arch = "wasm32"))]
type Accepted = skyzen::Response;

/// Accepts the host's hibernating socket.
async fn accept_host(headers: Headers) -> Outcome<Accepted> {
    accept(&headers).into()
}

/// Sends one command to the host, holding the container work it cannot take
/// right now.
async fn run_command(
    headers: Headers,
    body: Bytes,
    connections: DurableConnections,
    db: DurableDb,
) -> Outcome<NoContent> {
    dispatch(&headers, &body, &connections, &db).await.into()
}

/// Reads the command out of the body itself.
///
/// A [`ControlToHost`] carries a whole [`ContainerJob`](flyco_provider::host::ContainerJob),
/// which carries a daemon bootstrap — three credentials, a repository and a
/// commit identity — and none of that is an API schema anybody publishes.
/// The room's internal routes are spoken only by this Worker, so the body is
/// decoded here rather than dragged through `OpenAPI`.
async fn dispatch(
    headers: &Headers,
    body: &[u8],
    connections: &DurableConnections,
    db: &DurableDb,
) -> Result<NoContent, ApiError> {
    let command: ControlToHost = serde_json::from_slice(body)
        .map_err(|error| ApiError::Room(format!("a host command did not decode: {error}")))?;
    dispatch_command(headers, &command, connections, db).await
}

async fn dispatch_command(
    headers: &Headers,
    command: &ControlToHost,
    connections: &DurableConnections,
    db: &DurableDb,
) -> Result<NoContent, ApiError> {
    internal(headers)?;

    // Written before it is sent, so a host that takes a job and dies is
    // asked again rather than losing it — and so a job planned while the
    // machine is away is waiting when it comes back.
    if let ControlToHost::Run { job } = command {
        hold(db, job.container(), command)
            .await
            .map_err(|error| room_failed(&error))?;
    }

    let delivered = forward_to_host(connections, command).map_err(|error| room_failed(&error))?;
    if !delivered && !command.survives_a_disconnect() {
        tracing::warn!(
            ?command,
            "dropped a command: this host has no socket connected"
        );
    }

    if matches!(command, ControlToHost::Revoked) {
        forget_jobs(db).await.map_err(|error| room_failed(&error))?;
    }
    Ok(NoContent)
}

/// Serves what the room knows about its machine.
async fn read_status(
    headers: Headers,
    connections: DurableConnections,
    kv: DurableKv,
    db: DurableDb,
) -> Outcome<Json<HostStatus>> {
    status(&headers, &connections, &kv, &db).await.into()
}

async fn status(
    headers: &Headers,
    connections: &DurableConnections,
    kv: &DurableKv,
    db: &DurableDb,
) -> Result<Json<HostStatus>, ApiError> {
    internal(headers)?;
    Ok(Json(HostStatus {
        connected: is_connected(connections).map_err(|error| room_failed(&error))?,
        facts: read_facts(kv).await?,
        pending_jobs: pending_jobs(db)
            .await
            .map_err(|error| room_failed(&error))?,
    }))
}

fn room_failed(error: &DurableObjectError) -> ApiError {
    ApiError::Room(error.to_string())
}

/// Turns a validated upgrade request into an accepted hibernating socket.
///
/// The tags are the socket's whole identity for the rest of its life: the
/// role decides which way frames flow, and the host id is what the room is
/// about after it has hibernated and forgotten everything else.
///
/// # Errors
///
/// Returns [`ApiError::Room`] if the internal headers are absent or name
/// another role.
#[cfg(target_arch = "wasm32")]
fn accept(headers: &Headers) -> Result<Accepted, ApiError> {
    let host = admit(headers)?;
    Ok(Accepted::new()
        .tag(ROLE_HOST)
        .tag(format!("{HOST_TAG_PREFIX}{host}")))
}

/// Native builds do not reach this route with a socket to accept.
///
/// The same limitation the session room documents: only the peer's own
/// handshake request becomes a socket, and natively the control plane has no
/// path that carries one into a room. The room's HTTP routes work natively
/// and are what the tests drive, alongside `websocket` called directly.
///
/// # Errors
///
/// Always: with [`ApiError::Room`] if the internal headers are wrong, and
/// with [`ApiError::RelayUnavailable`] if they are right.
#[cfg(not(target_arch = "wasm32"))]
fn accept(headers: &Headers) -> Result<Accepted, ApiError> {
    admit(headers)?;
    Err(ApiError::RelayUnavailable(
        "a native control plane does not forward relay upgrades into a host room",
    ))
}

/// Checks the internal headers of an upgrade forwarded from the Worker.
fn admit(headers: &Headers) -> Result<flyco_core::HostId, ApiError> {
    let role = headers
        .get(HEADER_ROLE)
        .ok_or_else(|| ApiError::Room("a host relay upgrade named no role".to_owned()))?;
    if role != ROLE_HOST {
        return Err(ApiError::Room(
            "a host relay upgrade named another role".to_owned(),
        ));
    }
    headers
        .get(HEADER_HOST)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| ApiError::Room("a host relay upgrade named no host".to_owned()))
}
