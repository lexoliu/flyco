//! The host room: one Durable Object per enrolled machine.
//!
//! A host's `flycod host` attaches over REST, holds one SSE stream for the
//! room's [`ControlToHost`] commands, and posts its [`HostToControl`]
//! frames back in sequenced batches. The room is the only thing in flyco
//! that can see that attachment, which makes it two things: the way
//! container work reaches the machine, and the authority on whether the
//! machine is there at all.
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
//!   The frame lets the room forget the job it was holding; the REST call
//!   is what completes the machine row, because only the Worker can write
//!   D1. Two routes for one fact, exactly as a session daemon's spot
//!   notice travels twice and for the same reason.
//!
//! # The mailbox
//!
//! A host is not always attached — the machine is rebooting, the unit is
//! restarting, somebody closed the lid — and container work that arrives
//! in that window is not a notification to drop: a `Run` lost is a
//! session that never gets its container, or a container nobody ever
//! removes. So every job is a row in `host_commands` before it is
//! anything else, and it stays a row until the host answers it — the
//! answer, not the delivery, is what retires a job, so a machine that
//! dies mid-`podman run` is asked again on its next attach.
//!
//! Delivery is therefore at-least-once, which is what a container job is
//! written to survive: creating one removes any container of that name
//! first, and stopping, starting or removing a container that is already
//! in that state is what `podman` does anyway. The stream cursor gives
//! the host something better than re-executing blindly: a job row carries
//! its `seq`, so a machine can tell a job it is already running from one
//! it has never seen.

use flyco_core::host::HostFacts;
use flyco_provider::host::{ControlToHost, HostAttach, HostFrames, HostToControl, container_name};
use serde::{Deserialize, Serialize};
use skyzen::durable::{DurableObject, DurableObjectError};
use skyzen::extract::Query;
use skyzen::responder::Sse;
use skyzen::responder::sse::Event;
use skyzen::routing::{CreateRouteNode, Route, Router};
use skyzen::sql;
use skyzen::utils::{Bytes, Json};
use skyzen_services::durable::{DurableDb, DurableKv};

use crate::ApiError;
use crate::clock::now_unix;
use crate::extract::Headers;
use crate::problem::Outcome;
use crate::respond::NoContent;
use crate::room::{HEADER_INTERNAL, INTERNAL};

/// Names the host a Worker→room call belongs to.
pub const HEADER_HOST: &str = "x-flyco-host";

/// How long a host's presence marker survives its last contact.
///
/// The same contract as the session room's: attach, every frames POST,
/// and every poll of the command stream renew it, so expiry means a
/// machine that has said nothing for this long.
const PRESENCE_TTL_SECONDS: u64 = 30;

/// How often the command stream renews the presence marker.
const PRESENCE_REFRESH_SECONDS: u64 = 10;

/// KV key holding the facts the machine last reported.
const KEY_FACTS: &str = "host:facts";

/// What a host's room knows right now.
///
/// Answered by `/internal/status`, and the only honest source for the
/// first field: an attachment that fell silent is visible to the Durable
/// Object holding its marker and to nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HostStatus {
    /// Whether an attached host is still inside its contact deadline.
    pub connected: bool,
    /// What the machine last said about itself, if it has ever attached.
    pub facts: Option<HostFacts>,
    /// How many container jobs are still waiting to be answered.
    pub pending_jobs: u32,
}

/// What an attach answers with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
pub struct HostAttachResponse {
    /// Generation of this attachment; increments per attach.
    pub epoch: u64,
}

/// Query of the room's command stream.
#[derive(Debug, Deserialize, skyzen::ToSchema)]
pub struct HostCommandCursor {
    /// The attach this stream serves.
    pub epoch: u64,
}

/// The columns the `host_commands` table stores.
#[derive(Debug, skyzen::FromRow)]
struct CommandRow {
    seq: u64,
    /// The command, as a JSON document. Untyped for the same reason a
    /// stored session event is: the room hands the host what it was
    /// given, including a variant this build does not know how to read.
    #[row(json)]
    json: serde_json::Value,
}

/// The `host_presence` row.
#[derive(Debug, skyzen::FromRow)]
struct PresenceRow {
    epoch: u64,
    live_until: u64,
}

/// The relay room for one enrolled host.
#[derive(Debug, Default, Serialize, Deserialize)]
#[skyzen::durable_object]
pub struct HostRoom;

impl DurableObject for HostRoom {
    fn fetch(&mut self) -> Router {
        Route::new((
            "/internal/attach".post(attach_host),
            "/internal/commands".at(stream_commands),
            "/internal/frames".post(accept_frames),
            "/internal/command".post(run_command),
            "/internal/status".at(read_status),
        ))
        .build()
    }
}

// ── The host's three routes ──

/// Attaches a machine to its room.
///
/// The attach is the greeting: the facts are re-reported here because
/// they change between connections — memory is added, a disk fills,
/// Podman is upgraded — and the epoch minted here names every later
/// frame batch and command stream.
async fn attach_host(
    headers: Headers,
    Json(attach): Json<HostAttach>,
    db: DurableDb,
    kv: DurableKv,
) -> Outcome<Json<HostAttachResponse>> {
    attach_inner(&headers, &attach, &db, &kv).await.into()
}

async fn attach_inner(
    headers: &Headers,
    attach: &HostAttach,
    db: &DurableDb,
    kv: &DurableKv,
) -> Result<Json<HostAttachResponse>, ApiError> {
    internal(headers)?;
    ensure_schema(db)
        .await
        .map_err(|error| room_failed(&error))?;

    let live_until = now_unix().saturating_add(PRESENCE_TTL_SECONDS);
    sql!(
        db,
        "INSERT INTO host_presence (id, epoch, live_until) VALUES (0, 1, {live_until}) \
         ON CONFLICT (id) DO UPDATE SET \
             epoch = host_presence.epoch + 1, \
             live_until = excluded.live_until"
    )
    .execute()
    .await
    .map_err(|error| room_failed(&error))?;
    let epoch: u64 = sql!(db, "SELECT epoch FROM host_presence WHERE id = 0")
        .fetch_scalar()
        .await
        .map_err(|error| room_failed(&error))?;
    sql!(db, "DELETE FROM host_frames WHERE epoch != {epoch}")
        .execute()
        .await
        .map_err(|error| room_failed(&error))?;

    put_facts(kv, &attach.facts)
        .await
        .map_err(|error| room_failed(&error))?;
    tracing::info!(hostname = %attach.facts.hostname, epoch, "a host attached to its room");
    Ok(Json(HostAttachResponse { epoch }))
}

/// Holds a machine's command stream open: a cursor over `host_commands`,
/// polled until the attach it serves is superseded. See [`crate::room`]
/// for why storage is the rendezvous.
async fn stream_commands(
    headers: Headers,
    Query(cursor): Query<HostCommandCursor>,
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
            "a command stream was opened before any host attached".to_owned(),
        ));
    };
    if presence.epoch != epoch {
        return Err(ApiError::RelayEpochStale {
            current: presence.epoch,
            opened: epoch,
        });
    }
    Ok(crate::sse::serve(
        CommandFeed {
            db,
            epoch,
            cursor: 0,
            last_touch: 0,
        },
        poll_command_feed,
        crate::sse::HEARTBEAT,
    ))
}

/// The command stream's working state.
struct CommandFeed {
    db: DurableDb,
    /// The attach this stream serves; a newer one ends it.
    epoch: u64,
    /// How far down `host_commands` this stream has handed over.
    ///
    /// Starts at zero on every stream: a job is retired by its answer, not
    /// by having been delivered, so a fresh stream replays whatever the
    /// host has not yet answered — the mailbox semantic the room owes.
    cursor: u64,
    /// When presence was last renewed, seconds.
    last_touch: u64,
}

/// One poll of the host's command stream.
fn poll_command_feed(feed: &mut CommandFeed) -> crate::sse::PollFn<'_> {
    Box::pin(async move {
        command_feed_step(feed)
            .await
            .unwrap_or(crate::sse::Poll::Idle)
    })
}

/// One poll step; a failed read is a warning and an idle tick, not a
/// dead stream.
async fn command_feed_step(feed: &mut CommandFeed) -> Result<crate::sse::Poll, ()> {
    let now = now_unix();
    let db = &feed.db;
    let presence = read_presence(db).await.map_err(|error| {
        tracing::warn!(%error, "a host command stream could not read presence");
    })?;
    if let Some(row) = presence
        && row.epoch != feed.epoch
    {
        return Ok(crate::sse::Poll::End);
    }
    if now.saturating_sub(feed.last_touch) >= PRESENCE_REFRESH_SECONDS {
        let live_until = now.saturating_add(PRESENCE_TTL_SECONDS);
        let epoch = feed.epoch;
        sql!(
            db,
            "UPDATE host_presence SET live_until = {live_until} \
             WHERE id = 0 AND epoch = {epoch}"
        )
        .execute()
        .await
        .map_err(|error| {
            tracing::warn!(%error, "a host command stream could not renew presence");
        })?;
        feed.last_touch = now;
    }

    let cursor = feed.cursor;
    let rows: Vec<CommandRow> = sql!(
        db,
        "SELECT seq, json FROM host_commands WHERE seq > {cursor} ORDER BY seq"
    )
    .fetch_all()
    .await
    .map_err(|error| {
        tracing::warn!(%error, "a host command stream could not poll its commands");
    })?;
    if rows.is_empty() {
        return Ok(crate::sse::Poll::Idle);
    }
    // The poll just drained `rows` of backlog — bill them. An idle poll
    // reads nothing and is billed nothing: a stream's keepalive cadence
    // must not spend the budget a backlog could.
    crate::row_budget::charge_reads(db, rows.len() as u64)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "a host command stream hit the row budget");
        })?;
    feed.cursor = rows.last().map_or(feed.cursor, |row| row.seq);
    Ok(crate::sse::Poll::Emit(
        rows.iter()
            .map(|row| {
                let envelope = serde_json::json!({
                    "seq": row.seq,
                    "command": row.json,
                });
                Event::data(envelope.to_string())
                    .event("command")
                    .id(row.seq.to_string())
            })
            .collect(),
    ))
}

/// Accepts one batch of a machine's outbound frames.
///
/// The same ordering contract the session room enforces: a batch resumes
/// where the last stored one ended, a retransmission is answered without
/// reapplying, a gap is refused.
async fn accept_frames(
    headers: Headers,
    Json(batch): Json<HostFrames>,
    db: DurableDb,
) -> Outcome<NoContent> {
    accept_batch(&headers, batch, &db).await.into()
}

async fn accept_batch(
    headers: &Headers,
    batch: HostFrames,
    db: &DurableDb,
) -> Result<NoContent, ApiError> {
    internal(headers)?;
    ensure_schema(db)
        .await
        .map_err(|error| room_failed(&error))?;
    let Some(presence) = read_presence(db)
        .await
        .map_err(|error| room_failed(&error))?
    else {
        return Err(ApiError::Room(
            "a host posted frames before it attached".to_owned(),
        ));
    };
    if presence.epoch != batch.epoch {
        return Err(ApiError::RelayEpochStale {
            current: presence.epoch,
            opened: batch.epoch,
        });
    }

    let epoch = batch.epoch;
    let through: u64 = sql!(db, "SELECT through FROM host_frames WHERE epoch = {epoch}")
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
    let skip = usize::try_from(next.saturating_sub(batch.from_seq)).unwrap_or(usize::MAX);
    let fresh = batch.frames.get(skip..).unwrap_or(&[]);
    for frame in fresh {
        match frame {
            HostToControl::JobResult { job_id, outcome } => {
                tracing::info!(
                    machine = %job_id,
                    outcome = ?core::mem::discriminant(outcome),
                    "a host answered a container job"
                );
                // Keyed by the container the job named, which is how the
                // mailbox holds it: the name is derived from the machine
                // id, so the answer and the job agree without a second
                // column.
                answered(db, &container_name(*job_id))
                    .await
                    .map_err(|error| room_failed(&error))?;
            }
        }
    }
    if !batch.frames.is_empty() {
        let new_through = batch
            .from_seq
            .saturating_add(u64::try_from(batch.frames.len()).unwrap_or(0))
            .saturating_sub(1);
        sql!(
            db,
            "INSERT INTO host_frames (epoch, through) VALUES ({epoch}, {new_through}) \
             ON CONFLICT (epoch) DO UPDATE SET \
                 through = max(excluded.through, host_frames.through)"
        )
        .execute()
        .await
        .map_err(|error| room_failed(&error))?;
    }

    // Ephemeral rows — a `Revoked` — are retired by acknowledgement. Job
    // rows are not: a job is done when its answer arrives, not when it was
    // handed over.
    let ack_through = batch.ack_through;
    if ack_through > 0 {
        sql!(
            db,
            "DELETE FROM host_commands WHERE seq <= {ack_through} AND machine IS NULL"
        )
        .execute()
        .await
        .map_err(|error| room_failed(&error))?;
    }

    let live_until = now_unix().saturating_add(PRESENCE_TTL_SECONDS);
    sql!(
        db,
        "UPDATE host_presence SET live_until = {live_until} \
         WHERE id = 0 AND epoch = {epoch}"
    )
    .execute()
    .await
    .map_err(|error| room_failed(&error))?;
    Ok(NoContent)
}

// ── The mailbox ──

/// Records a job before anybody tries to deliver it.
///
/// Written first and deleted on the answer, so a host that took a job and
/// died is asked again rather than losing it — and so a job planned while
/// the machine is away is waiting when it comes back. A command that is
/// not container work takes the same table with no machine: it is
/// delivered by the same cursor and retired by acknowledgement.
async fn hold(
    db: &DurableDb,
    container: Option<&str>,
    command: &ControlToHost,
) -> Result<(), DurableObjectError> {
    let json = serde_json::to_string(command)
        .map_err(|error| DurableObjectError::Serialization(error.to_string()))?;
    let now = now_unix();
    ensure_schema(db).await?;
    match container {
        Some(container) => sql!(
            db,
            "INSERT INTO host_commands (machine, json, at_unix) \
             VALUES ({container}, {json}, {now})"
        )
        .execute()
        .await
        .map_err(|error| stored(&error)),
        None => sql!(
            db,
            "INSERT INTO host_commands (machine, json, at_unix) \
             VALUES (NULL, {json}, {now})"
        )
        .execute()
        .await
        .map_err(|error| stored(&error)),
    }?;
    Ok(())
}

/// Forgets the oldest outstanding job for one container.
///
/// The oldest rather than all of them: a session that was created and
/// then stopped has two jobs on the same container, and the answer to the
/// first says nothing about the second.
async fn answered(db: &DurableDb, container: &str) -> Result<(), DurableObjectError> {
    ensure_schema(db).await?;
    sql!(
        db,
        "DELETE FROM host_commands WHERE seq = \
         (SELECT MIN(seq) FROM host_commands WHERE machine = {container})"
    )
    .execute()
    .await
    .map_err(|error| stored(&error))?;
    Ok(())
}

/// Drops every outstanding container job.
///
/// A revoked host will never attach again, so a job left in its mailbox
/// is work that is never going to happen: keeping it would make the
/// room's own status lie about what is pending. Non-job rows are left —
/// the `Revoked` itself is one, and it is owed delivery.
async fn forget_jobs(db: &DurableDb) -> Result<(), DurableObjectError> {
    ensure_schema(db).await?;
    sql!(db, "DELETE FROM host_commands WHERE machine IS NOT NULL")
        .execute()
        .await
        .map_err(|error| stored(&error))?;
    Ok(())
}

async fn pending_jobs(db: &DurableDb) -> Result<u32, DurableObjectError> {
    ensure_schema(db).await?;
    let count: u64 = sql!(
        db,
        "SELECT COUNT(*) AS pending FROM host_commands WHERE machine IS NOT NULL"
    )
    .fetch_scalar()
    .await
    .map_err(|error| stored(&error))?;
    Ok(u32::try_from(count).unwrap_or(u32::MAX))
}

/// The schema this build expects.
///
/// The durable answer to "is the schema already there" lives in the
/// `schema_meta` table [`crate::schema_version`] keeps — checking it
/// costs one storage read where running every `CREATE` blind costs one
/// per statement — and every hot path in the room calls this first. Bump
/// it when the DDL below changes so a room built by an older build
/// upgrades once, on its next call.
const SCHEMA_VERSION: i64 = 2;

/// Creates the mailbox if this is the room's first write.
///
/// `AUTOINCREMENT` rather than a counter in the object: the order jobs
/// were planned in has to survive the object being rebuilt around every
/// event, and the database is the only thing here that guarantees it.
async fn ensure_schema(db: &DurableDb) -> Result<(), DurableObjectError> {
    crate::schema_version::ensure(
        db,
        SCHEMA_VERSION,
        &[
            // Every command owed to the host. `machine` names the container a
            // job acts on — the answer deletes by it — and is NULL for the
            // commands that are not container work, which acknowledgement
            // deletes instead.
            "CREATE TABLE IF NOT EXISTS host_commands (\
                 seq     INTEGER PRIMARY KEY AUTOINCREMENT, \
                 machine TEXT, \
                 json    TEXT    NOT NULL, \
                 at_unix INTEGER NOT NULL)",
            // One row, because a room has exactly one host. The epoch names
            // the current attach; `live_until` is the deadline its contact
            // renews.
            "CREATE TABLE IF NOT EXISTS host_presence (\
                 id         INTEGER PRIMARY KEY CHECK (id = 0), \
                 epoch      INTEGER NOT NULL, \
                 live_until INTEGER NOT NULL)",
            // One row per attach, recording how far into that epoch's frame
            // numbering the room has stored.
            "CREATE TABLE IF NOT EXISTS host_frames (\
                 epoch   INTEGER PRIMARY KEY, \
                 through INTEGER NOT NULL)",
            // The room's row-read ledger — one row per UTC day, debited by
            // `row_budget::charge_reads`, and the circuit breaker that keeps a
            // runaway reader here from spending the account's quota.
            crate::row_budget::SCHEMA,
        ],
    )
    .await
}

/// The room's own database failed, which is a runtime fault rather than
/// anything a caller did.
fn stored(error: &skyzen_services::DurableDbError) -> DurableObjectError {
    DurableObjectError::Runtime(error.to_string())
}

/// Reads the presence row, if an attach has ever written one.
async fn read_presence(db: &DurableDb) -> Result<Option<PresenceRow>, DurableObjectError> {
    sql!(
        db,
        "SELECT epoch, live_until FROM host_presence WHERE id = 0"
    )
    .fetch_optional()
    .await
    .map_err(|error| stored(&error))
}

/// Whether the host's presence marker is still inside its deadline.
async fn host_live(db: &DurableDb) -> Result<bool, DurableObjectError> {
    let Some(presence) = read_presence(db).await? else {
        return Ok(false);
    };
    Ok(presence.live_until >= now_unix())
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

/// Sends one command to the host, holding the container work it cannot
/// take right now.
async fn run_command(headers: Headers, body: Bytes, db: DurableDb) -> Outcome<NoContent> {
    dispatch(&headers, &body, &db).await.into()
}

/// Reads the command out of the body itself.
///
/// A [`ControlToHost`] carries a whole
/// [`ContainerJob`](flyco_provider::host::ContainerJob), which carries a
/// daemon bootstrap — three credentials, a repository and a commit
/// identity — and none of that is an API schema anybody publishes. The
/// room's internal routes are spoken only by this Worker, so the body is
/// decoded here rather than dragged through `OpenAPI`.
async fn dispatch(headers: &Headers, body: &[u8], db: &DurableDb) -> Result<NoContent, ApiError> {
    let command: ControlToHost = serde_json::from_slice(body)
        .map_err(|error| ApiError::Room(format!("a host command did not decode: {error}")))?;
    dispatch_command(headers, &command, db).await
}

async fn dispatch_command(
    headers: &Headers,
    command: &ControlToHost,
    db: &DurableDb,
) -> Result<NoContent, ApiError> {
    internal(headers)?;
    ensure_schema(db)
        .await
        .map_err(|error| room_failed(&error))?;

    // Written before it can be delivered, so a host that takes a job and
    // dies is asked again rather than losing it — and so a job planned
    // while the machine is away is waiting when it comes back.
    let live = host_live(db).await.map_err(|error| room_failed(&error))?;
    match command {
        ControlToHost::Run { job } => {
            hold(db, Some(job.container()), command)
                .await
                .map_err(|error| room_failed(&error))?;
        }
        _ if live => {
            hold(db, None, command)
                .await
                .map_err(|error| room_failed(&error))?;
        }
        ControlToHost::Revoked => {
            tracing::warn!(
                ?command,
                "dropped a command: this host has no live attachment"
            );
        }
    }

    if matches!(command, ControlToHost::Revoked) {
        forget_jobs(db).await.map_err(|error| room_failed(&error))?;
    }
    Ok(NoContent)
}

/// Serves what the room knows about its machine.
async fn read_status(headers: Headers, kv: DurableKv, db: DurableDb) -> Outcome<Json<HostStatus>> {
    status(&headers, &kv, &db).await.into()
}

async fn status(
    headers: &Headers,
    kv: &DurableKv,
    db: &DurableDb,
) -> Result<Json<HostStatus>, ApiError> {
    internal(headers)?;
    ensure_schema(db)
        .await
        .map_err(|error| room_failed(&error))?;
    Ok(Json(HostStatus {
        connected: host_live(db).await.map_err(|error| room_failed(&error))?,
        facts: read_facts(kv).await?,
        pending_jobs: pending_jobs(db)
            .await
            .map_err(|error| room_failed(&error))?,
    }))
}

fn room_failed(error: &impl core::fmt::Display) -> ApiError {
    ApiError::Room(error.to_string())
}
