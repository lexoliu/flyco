//! Enrolling a machine the user owns, and its life afterwards.
//!
//! The control plane runs on Cloudflare Workers and has no TCP sockets, so
//! it can never dial somebody's machine. A host is therefore **enrolled**:
//! the user mints a single-use token, runs one command on the machine, and
//! `flycod host` registers itself and holds an outbound socket to its
//! [`HostRoom`](crate::host_room::HostRoom) from then on.
//!
//! # Where the truth about a host lives
//!
//! Split, and deliberately:
//!
//! * **D1** holds identity — who owns it, what it is called, what it last
//!   said about itself, and the hash of its token.
//! * **Its room** holds liveness, because a Durable Object is the only thing
//!   that can see the socket and cannot write to D1 (see
//!   [`crate::host_room`]).
//!
//! So every read of a host goes through [`refresh`], which asks the room and
//! writes back what it learned. That is what makes `GET /v1/hosts` — the
//! call the enrollment wizard polls — flip from `offline` to `online` the
//! moment the machine's daemon arrives, without a cron, a heartbeat route,
//! or a second copy of the facts.
//!
//! # Credentials
//!
//! Both the enrollment token and the long-lived host token are `fh_`
//! strings stored only as a SHA-256, exactly like a session's daemon token:
//! returned once, never read back, and replaced by minting again — which is
//! what makes rotation a revocation.

use askama::Template;
use flyco_core::host::{ENROLLMENT_TOKEN_TTL_SECONDS, HOST_TOKEN_PREFIX, MAX_HOST_LABEL_CHARS};
use flyco_core::{
    CurrentUser, EnrollHost, EnrolledHost, Enrollment, EnrollmentToken, EnrollmentTokenId,
    HostFacts, HostId, HostState, HostView, ProviderAccountId, ProviderCredentials,
    ReportJobResult, UpdateHost, UserId,
};
use flyco_provider::host::{ContainerJob, ControlToHost};
use serde::Deserialize;
use skyzen::extract::Query;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::sql;
use skyzen::utils::{Json, State};
use skyzen_services::{BatchStatement, Db};

use crate::clock::now_unix;
use crate::config::ApiConfig;
use crate::crypto::{prefixed_token, token_hash};
use crate::error::ApiError;
use crate::extract::{Headers, path_id};
use crate::problem::Outcome;
use crate::respond::{Created, NoContent};
use crate::rooms::HostRooms;
use crate::{machines, provider_accounts, sessions};

/// The columns every read on this path projects.
///
/// `token_hash` is deliberately absent: a credential that is never selected
/// cannot be leaked by a later edit to a response type. The one place that
/// needs it reads it by itself — see [`authenticates`].
#[derive(Debug, Clone, skyzen::FromRow)]
pub struct HostRow {
    /// Identifier every route and every container job names it by.
    pub id: HostId,
    /// Who enrolled it.
    pub user_id: UserId,
    /// What the user calls it.
    pub label: String,
    /// What the machine last said about itself.
    #[row(json)]
    pub facts: HostFacts,
    /// Where the control plane last recorded it in its life.
    pub state: HostState,
    /// When its daemon was last seen connected.
    pub last_seen_unix: Option<u64>,
    /// When it was enrolled.
    pub created_at_unix: u64,
}

impl From<HostRow> for HostView {
    fn from(row: HostRow) -> Self {
        Self {
            id: row.id,
            label: row.label,
            state: row.state,
            facts: row.facts,
            last_seen_unix: row.last_seen_unix,
            created_at_unix: row.created_at_unix,
        }
    }
}

/// The one line the wizard shows, rendered against this deployment's own
/// origin.
///
/// A template rather than a `format!`, like every other structured thing
/// flyco emits: the command is what a user pastes into a root shell on their
/// own machine, and its shape belongs in a file the build checks.
#[derive(Debug, Template)]
#[template(path = "host/enroll_command.txt", escape = "none")]
struct EnrollCommand<'a> {
    /// Base URL of this control plane, with its trailing slash.
    origin: &'a str,
    /// The single-use enrollment token.
    token: &'a str,
}

/// Mints a single-use enrollment token for the caller.
///
/// # Errors
///
/// Returns [`ApiError`] if entropy is unavailable, the command will not
/// render, or the write fails.
pub async fn mint(db: &Db, config: &ApiConfig, user: UserId) -> Result<EnrollmentToken, ApiError> {
    let token = prefixed_token(HOST_TOKEN_PREFIX)?;
    let id = EnrollmentTokenId::generate();
    let now = now_unix();
    let expires_at = now.saturating_add(ENROLLMENT_TOKEN_TTL_SECONDS);

    sql!(
        db,
        "INSERT INTO host_enrollment_tokens \
         (id, user_id, token_hash, expires_at_unix, created_at_unix) \
         VALUES ({id}, {user}, {token_hash(&token)}, {expires_at}, {now})"
    )
    .execute()
    .await?;

    let command = EnrollCommand {
        origin: &config.control_plane_url(),
        token: &token,
    }
    .render()
    .map_err(|_| ApiError::CorruptRecord("the host install command did not render"))?
    .trim_end()
    .to_owned();

    tracing::info!(%id, "minted a host enrollment token");
    Ok(EnrollmentToken {
        id,
        token,
        expires_at_unix: expires_at,
        command,
    })
}

/// One enrollment token, as the wizard polls it.
#[derive(Debug, skyzen::FromRow)]
struct TokenRow {
    host_id: Option<HostId>,
}

/// Reports whether a minted token has been spent, and by which machine.
///
/// # Errors
///
/// Returns [`ApiError::EnrollmentTokenNotFound`] if the token is not the
/// caller's, or [`ApiError`] if the database fails.
pub async fn enrollment(
    db: &Db,
    rooms: &HostRooms,
    user: UserId,
    id: EnrollmentTokenId,
) -> Result<Enrollment, ApiError> {
    let row: TokenRow = sql!(
        db,
        "SELECT host_id FROM host_enrollment_tokens WHERE id = {id} AND user_id = {user}"
    )
    .fetch_optional()
    .await?
    .ok_or(ApiError::EnrollmentTokenNotFound)?;

    match row.host_id {
        None => Ok(Enrollment::Pending),
        Some(host) => Ok(Enrollment::Enrolled {
            host: view(db, rooms, user, host).await?,
        }),
    }
}

/// Registers the machine an enrollment token was minted for.
///
/// Three things happen, in this order, and none of them is safe to reorder:
/// the token is spent, the host row is written, and the host becomes a
/// provider account — because the compute chip, the curated catalog, session
/// creation and the usage panel all speak `provider_accounts` and must need
/// no special case for a machine somebody owns.
///
/// # Errors
///
/// Returns [`ApiError::EnrollmentTokenExpired`] if the token is unknown,
/// expired or already spent — the three are deliberately indistinguishable —
/// or [`ApiError`] if entropy is unavailable or a write fails.
pub async fn enroll(
    db: &Db,
    config: &ApiConfig,
    request: &EnrollHost,
) -> Result<EnrolledHost, ApiError> {
    let now = now_unix();
    let presented = token_hash(&request.token);
    if !request.token.starts_with(HOST_TOKEN_PREFIX) {
        return Err(ApiError::EnrollmentTokenExpired);
    }

    let user: UserId = sql!(
        db,
        "SELECT user_id FROM host_enrollment_tokens \
         WHERE token_hash = {presented.clone()} AND spent_at_unix IS NULL \
         AND expires_at_unix > {now}"
    )
    .fetch_scalar_optional()
    .await?
    .ok_or(ApiError::EnrollmentTokenExpired)?;

    let id = HostId::generate();
    let host_token = prefixed_token(HOST_TOKEN_PREFIX)?;
    let facts = serde_json::to_string(&request.facts)
        .map_err(|_| ApiError::CorruptRecord("host facts could not be encoded"))?;
    let label = request.facts.hostname.clone();
    // Offline until its daemon actually arrives: enrolling is the machine
    // saying what it is, and the socket is a separate thing it opens next.
    let state = HostState::Offline;

    sql!(
        db,
        "INSERT INTO hosts \
         (id, user_id, label, facts, token_hash, state, created_at_unix) \
         VALUES ({id}, {user}, {label}, {facts}, {token_hash(&host_token)}, {state}, {now})"
    )
    .execute()
    .await?;

    // Spent by the row that used it, and conditionally, so two machines
    // racing on one token cannot both enroll: the second update writes
    // nothing and the second enrollment is refused.
    let spent = sql!(
        db,
        "UPDATE host_enrollment_tokens SET spent_at_unix = {now}, host_id = {id} \
         WHERE token_hash = {presented} AND spent_at_unix IS NULL"
    )
    .execute()
    .await?;
    if spent.rows_written == 0 {
        sql!(db, "DELETE FROM hosts WHERE id = {id}")
            .execute()
            .await?;
        return Err(ApiError::EnrollmentTokenExpired);
    }

    provider_accounts::create(
        db,
        config,
        user,
        request.facts.hostname.clone(),
        &ProviderCredentials::Host { host: id },
        Some(id),
    )
    .await?;

    tracing::info!(host = %id, hostname = %request.facts.hostname, "a machine enrolled");
    Ok(EnrolledHost {
        host_id: id,
        host_token,
    })
}

/// Lists the caller's hosts, refreshed from their rooms.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn list(db: &Db, rooms: &HostRooms, user: UserId) -> Result<Vec<HostView>, ApiError> {
    let removed = HostState::Removed;
    let rows: Vec<HostRow> = sql!(
        db,
        "SELECT id, user_id, label, facts, state, last_seen_unix, created_at_unix \
         FROM hosts WHERE user_id = {user} AND state != {removed} \
         ORDER BY created_at_unix DESC, id"
    )
    .fetch_all()
    .await?;

    let mut views = Vec::with_capacity(rows.len());
    for row in rows {
        views.push(refresh(db, rooms, row).await?);
    }
    Ok(views)
}

/// Loads one of the caller's hosts.
///
/// # Errors
///
/// Returns [`ApiError::HostNotFound`] if the host does not exist or belongs
/// to somebody else — the two are indistinguishable.
pub async fn load(db: &Db, user: UserId, id: HostId) -> Result<HostRow, ApiError> {
    sql!(
        db,
        "SELECT id, user_id, label, facts, state, last_seen_unix, created_at_unix \
         FROM hosts WHERE id = {id} AND user_id = {user}"
    )
    .fetch_optional()
    .await?
    .ok_or(ApiError::HostNotFound)
}

/// Loads one host without scoping it to an owner.
///
/// The provisioning path's read: a job was planned by a handler that had
/// already proved the caller owns the session, and the account it names is
/// the one that owns the host.
///
/// # Errors
///
/// Returns [`ApiError::HostNotFound`] if there is no such host.
pub async fn find(db: &Db, id: HostId) -> Result<HostRow, ApiError> {
    sql!(
        db,
        "SELECT id, user_id, label, facts, state, last_seen_unix, created_at_unix \
         FROM hosts WHERE id = {id}"
    )
    .fetch_optional()
    .await?
    .ok_or(ApiError::HostNotFound)
}

/// Reads one of the caller's hosts, refreshed from its room.
///
/// # Errors
///
/// Returns [`ApiError::HostNotFound`] if the host is not the caller's.
pub async fn view(
    db: &Db,
    rooms: &HostRooms,
    user: UserId,
    id: HostId,
) -> Result<HostView, ApiError> {
    let row = load(db, user, id).await?;
    refresh(db, rooms, row).await
}

/// Brings a stored host row up to date with what its room can see.
///
/// The room is the authority on whether the machine is connected and on what
/// it last reported, and it cannot write any of that down. So a read is
/// where the durable row learns it: online with fresh facts while the socket
/// is up, offline once it is gone, and untouched while a removal is in
/// flight — a host being drained is not one to put back online because a
/// container is still talking.
///
/// A room that cannot be reached costs the refresh, not the read: the
/// recorded state is the last thing flyco knew, and answering with it is
/// better than failing a list because one Durable Object was slow.
///
/// # Errors
///
/// Returns [`ApiError`] if the write-back fails.
pub async fn refresh(db: &Db, rooms: &HostRooms, row: HostRow) -> Result<HostView, ApiError> {
    if matches!(row.state, HostState::Draining | HostState::Removed) {
        return Ok(row.into());
    }

    let status = match rooms.status(row.id).await {
        Ok(status) => status,
        Err(error) => {
            tracing::warn!(host = %row.id, %error, "a host's room could not be reached");
            return Ok(row.into());
        }
    };

    let state = if status.connected {
        HostState::Online
    } else {
        HostState::Offline
    };
    let facts = status.facts.unwrap_or_else(|| row.facts.clone());
    let last_seen = if status.connected {
        Some(now_unix())
    } else {
        row.last_seen_unix
    };

    if state == row.state && facts == row.facts && last_seen == row.last_seen_unix {
        return Ok(row.into());
    }

    let encoded = serde_json::to_string(&facts)
        .map_err(|_| ApiError::CorruptRecord("host facts could not be encoded"))?;
    sql!(
        db,
        "UPDATE hosts SET state = {state}, facts = {encoded}, last_seen_unix = {last_seen} \
         WHERE id = {row.id}"
    )
    .execute()
    .await?;

    Ok(HostView {
        state,
        facts,
        last_seen_unix: last_seen,
        ..row.into()
    })
}

/// Records that a machine's daemon has just opened its socket.
///
/// The durable half of a host coming back, and it happens on the *upgrade*
/// rather than on the `Hello` frame for the reason everything else about a
/// room does: the frame reaches a Durable Object, and a Durable Object
/// cannot write D1. The Worker authenticates that upgrade, so this is the
/// one moment the control plane can see a machine arrive and record it —
/// without which a host that enrolled and connected while nobody was
/// looking would offer no catalog at all.
///
/// A drained host is left alone: its token is gone, so nothing can reach
/// this with one, and a row that says `removed` is finished with.
///
/// # Errors
///
/// Returns [`ApiError`] if the write fails.
pub async fn arrived(db: &Db, host: HostId) -> Result<(), ApiError> {
    let online = HostState::Online;
    let removed = HostState::Removed;
    let draining = HostState::Draining;
    sql!(
        db,
        "UPDATE hosts SET state = {online}, last_seen_unix = {now_unix()} \
         WHERE id = {host} AND state != {removed} AND state != {draining}"
    )
    .execute()
    .await?;
    Ok(())
}

/// Renames one of the caller's hosts, and the account it provisions
/// through.
///
/// A machine somebody owns is two rows: the host it is, and the provider
/// account it offers itself as. Both carry the name — the card reads the
/// host's, the compute chip's account selector reads the account's — so a
/// rename that wrote only one of them would leave the same machine called
/// two different things depending on where you looked at it.
///
/// One atomic batch rather than two writes, because a rename that landed
/// halfway is exactly the drift this fixes: `execute_batch` is a real
/// transaction on the native backend and D1's own `batch()` on the Worker.
///
/// # Errors
///
/// Returns [`ApiError::InvalidHostLabel`] for an empty or over-long label,
/// or [`ApiError::HostNotFound`] if the host is not the caller's.
pub async fn rename(
    db: &Db,
    rooms: &HostRooms,
    user: UserId,
    id: HostId,
    label: &str,
) -> Result<HostView, ApiError> {
    let label = label.trim();
    if label.is_empty() || label.chars().count() > MAX_HOST_LABEL_CHARS {
        return Err(ApiError::InvalidHostLabel {
            max: MAX_HOST_LABEL_CHARS,
        });
    }
    let row = load(db, user, id).await?;

    db.execute_batch(vec![
        BatchStatement::new("UPDATE hosts SET label = ? WHERE id = ?")
            .bind(label)
            .bind(row.id),
        BatchStatement::new("UPDATE provider_accounts SET label = ? WHERE host_id = ?")
            .bind(label)
            .bind(row.id),
    ])
    .await?;

    refresh(
        db,
        rooms,
        HostRow {
            label: label.to_owned(),
            ..row
        },
    )
    .await
}

/// Mints a new token for a host, revoking the one it holds.
///
/// # Errors
///
/// Returns [`ApiError::HostNotFound`] if the host is not the caller's, or
/// [`ApiError::HostRemoved`] for one that has been drained — a removed host
/// is finished with, and handing it a live credential would put it back in
/// service by accident.
pub async fn rotate(db: &Db, user: UserId, id: HostId) -> Result<EnrolledHost, ApiError> {
    let row = load(db, user, id).await?;
    if matches!(row.state, HostState::Removed) {
        return Err(ApiError::HostRemoved);
    }

    let host_token = prefixed_token(HOST_TOKEN_PREFIX)?;
    sql!(
        db,
        "UPDATE hosts SET token_hash = {token_hash(&host_token)} WHERE id = {row.id}"
    )
    .execute()
    .await?;

    tracing::info!(host = %id, "rotated a host token");
    Ok(EnrolledHost {
        host_id: id,
        host_token,
    })
}

/// Whether `presented` is the live token of `host`.
///
/// A host with no token — one that has been removed — authenticates nothing,
/// exactly as a wrong token does.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn authenticates(db: &Db, host: HostId, presented: &str) -> Result<bool, ApiError> {
    if !presented.starts_with(HOST_TOKEN_PREFIX) {
        return Ok(false);
    }

    let stored: Option<Option<String>> = sql!(db, "SELECT token_hash FROM hosts WHERE id = {host}")
        .fetch_scalar_optional()
        .await?;

    Ok(stored
        .flatten()
        .is_some_and(|stored| stored == token_hash(presented)))
}

/// Drains a host and revokes its token.
///
/// The whole of the removal, in the order that keeps it honest:
///
/// 1. The sessions still running there are counted. Unless `force`, that
///    count is the refusal — removing a host out from under a working agent
///    is not something to do because a button was pressed.
/// 2. The host goes to [`HostState::Draining`], durably, so a removal
///    interrupted halfway is not a host that reads as available.
/// 3. Every live container is stopped, **keeping its volume**: the session's
///    work is still there for an archive snapshot, and the user still owns
///    the disk it is on.
/// 4. The token is revoked and the host is [`HostState::Removed`]. The room
///    is told, so the unit on the machine stops rather than reconnecting
///    every few seconds against a credential that will never work again.
///
/// # Errors
///
/// Returns [`ApiError::HostNotFound`] if the host is not the caller's,
/// [`ApiError::HostHasActiveSessions`] if sessions are still running there
/// and `force` was not passed, or [`ApiError`] if a write fails.
pub async fn remove(
    db: &Db,
    rooms: &HostRooms,
    user: UserId,
    id: HostId,
    force: bool,
) -> Result<(), ApiError> {
    let row = load(db, user, id).await?;
    if matches!(row.state, HostState::Removed) {
        return Ok(());
    }
    let account = account_of(db, id).await?;
    let live = machines::live_on_account(db, account).await?;
    if !live.is_empty() && !force {
        return Err(ApiError::HostHasActiveSessions {
            sessions: u32::try_from(live.len()).unwrap_or(u32::MAX),
        });
    }

    let draining = HostState::Draining;
    sql!(db, "UPDATE hosts SET state = {draining} WHERE id = {id}")
        .execute()
        .await?;

    for machine in &live {
        let Some(container) = machine.native_id.clone() else {
            continue;
        };
        // Best effort by construction: the machine may already be gone, and
        // the user's hardware is theirs either way. What must not happen is
        // the removal stopping halfway and leaving a live token behind.
        if let Err(error) = rooms
            .command(
                id,
                &ControlToHost::Run {
                    job: ContainerJob::Stop { container },
                },
            )
            .await
        {
            tracing::warn!(host = %id, machine = %machine.id, %error, "a drain stop did not reach the host");
        }
        machines::deallocate(db, machine.id).await?;
    }

    let removed = HostState::Removed;
    sql!(
        db,
        "UPDATE hosts SET state = {removed}, token_hash = NULL WHERE id = {id}"
    )
    .execute()
    .await?;

    if let Err(error) = rooms.command(id, &ControlToHost::Revoked).await {
        tracing::warn!(host = %id, %error, "a revoked host was not told its token is gone");
    }

    tracing::info!(host = %id, drained = live.len(), "removed a host");
    Ok(())
}

/// The provider account one host provisions through.
///
/// # Errors
///
/// Returns [`ApiError::ProviderAccountNotFound`] for a host with no account,
/// which is a row enrollment could not have written.
async fn account_of(db: &Db, host: HostId) -> Result<ProviderAccountId, ApiError> {
    sql!(
        db,
        "SELECT id FROM provider_accounts WHERE host_id = {host}"
    )
    .fetch_scalar_optional()
    .await?
    .ok_or(ApiError::ProviderAccountNotFound)
}

/// Records what a host made of one container job.
///
/// The durable half of `HostToControl::JobResult`, and what completes a
/// machine row exactly as a cloud driver's answer does: the container and
/// volume the host actually created are what a later stop, start or removal
/// has to name.
///
/// # Errors
///
/// Returns [`ApiError::MachineNotFound`] if the job names a machine that is
/// not on this host, and [`ApiError`] if a write fails.
pub async fn record_job_result(
    db: &Db,
    host: &HostRow,
    report: &ReportJobResult,
) -> Result<(), ApiError> {
    let account = account_of(db, host.id).await?;
    let machine = machines::find(db, report.job_id)
        .await?
        .filter(|row| row.provider_account_id == account)
        .ok_or(ApiError::MachineNotFound)?;

    match &report.outcome {
        flyco_core::JobOutcome::Running { container, volume } => {
            machines::record_container(db, machine.id, container, volume).await?;
            tracing::info!(host = %host.id, machine = %machine.id, "a host reported a running container");
        }
        flyco_core::JobOutcome::Done => {
            tracing::info!(host = %host.id, machine = %machine.id, "a host finished a container job");
        }
        flyco_core::JobOutcome::Failed { message } => {
            // The session is what the user is watching, and a container that
            // never came up is exactly the failure a cloud provision reports
            // the same way: with the reason, in the state the user can act
            // on.
            sessions::fail(db, machine.session(), message).await?;
            tracing::warn!(host = %host.id, machine = %machine.id, message, "a container job failed");
        }
    }
    Ok(())
}

// ── Routes ──

/// Mints an enrollment token and the one command that spends it.
#[skyzen::openapi]
async fn mint_enrollment_token(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    db: Db,
) -> Outcome<Created<Json<EnrollmentToken>>> {
    mint(&db, &config, user.id)
        .await
        .map(|token| Created(Json(token)))
        .into()
}

/// Reports whether the machine a token was minted for has arrived.
#[skyzen::openapi]
async fn get_enrollment(
    State(user): State<CurrentUser>,
    params: Params,
    rooms: HostRooms,
    db: Db,
) -> Outcome<Json<Enrollment>> {
    read_enrollment(&user, &params, &rooms, &db).await.into()
}

async fn read_enrollment(
    user: &CurrentUser,
    params: &Params,
    rooms: &HostRooms,
    db: &Db,
) -> Result<Json<Enrollment>, ApiError> {
    let id = path_id::<EnrollmentTokenId>(params, "id")?;
    enrollment(db, rooms, user.id, id).await.map(Json)
}

/// Registers a machine, spending the enrollment token it presents.
///
/// Public, because the machine holds no user credential: the enrollment
/// token *is* the credential, it is single-use, and it names the user who
/// minted it.
#[skyzen::openapi]
async fn enroll_host(
    State(config): State<ApiConfig>,
    Json(request): Json<EnrollHost>,
    db: Db,
) -> Outcome<Created<Json<EnrolledHost>>> {
    enroll(&db, &config, &request)
        .await
        .map(|enrolled| Created(Json(enrolled)))
        .into()
}

/// Lists the caller's enrolled machines.
#[skyzen::openapi]
async fn list_hosts(
    State(user): State<CurrentUser>,
    rooms: HostRooms,
    db: Db,
) -> Outcome<Json<Vec<HostView>>> {
    list(&db, &rooms, user.id).await.map(Json).into()
}

/// Describes one of the caller's enrolled machines.
#[skyzen::openapi]
async fn get_host(
    State(user): State<CurrentUser>,
    params: Params,
    rooms: HostRooms,
    db: Db,
) -> Outcome<Json<HostView>> {
    read_host(&user, &params, &rooms, &db).await.into()
}

async fn read_host(
    user: &CurrentUser,
    params: &Params,
    rooms: &HostRooms,
    db: &Db,
) -> Result<Json<HostView>, ApiError> {
    let id = path_id::<HostId>(params, "id")?;
    view(db, rooms, user.id, id).await.map(Json)
}

/// Renames one of the caller's enrolled machines.
#[skyzen::openapi]
async fn update_host(
    State(user): State<CurrentUser>,
    params: Params,
    Json(update): Json<UpdateHost>,
    rooms: HostRooms,
    db: Db,
) -> Outcome<Json<HostView>> {
    rename_host(&user, &params, &update, &rooms, &db)
        .await
        .into()
}

async fn rename_host(
    user: &CurrentUser,
    params: &Params,
    update: &UpdateHost,
    rooms: &HostRooms,
    db: &Db,
) -> Result<Json<HostView>, ApiError> {
    let id = path_id::<HostId>(params, "id")?;
    rename(db, rooms, user.id, id, &update.label)
        .await
        .map(Json)
}

/// Narrows a host removal.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
struct RemoveQuery {
    /// Required while sessions are still running on the machine: the caller
    /// has seen the refusal and wants their containers stopped anyway.
    #[serde(default)]
    force: bool,
}

/// Drains a host, stops what runs on it, and revokes its token.
#[skyzen::openapi]
async fn delete_host(
    State(user): State<CurrentUser>,
    Query(query): Query<RemoveQuery>,
    params: Params,
    rooms: HostRooms,
    db: Db,
) -> Outcome<NoContent> {
    drain_host(&user, &params, query.force, &rooms, &db)
        .await
        .into()
}

async fn drain_host(
    user: &CurrentUser,
    params: &Params,
    force: bool,
    rooms: &HostRooms,
    db: &Db,
) -> Result<NoContent, ApiError> {
    let id = path_id::<HostId>(params, "id")?;
    remove(db, rooms, user.id, id, force).await?;
    Ok(NoContent)
}

/// Mints a new token for one of the caller's machines, revoking the old one.
#[skyzen::openapi]
async fn rotate_host_token(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Outcome<Json<EnrolledHost>> {
    rotate_token(&user, &params, &db).await.into()
}

async fn rotate_token(
    user: &CurrentUser,
    params: &Params,
    db: &Db,
) -> Result<Json<EnrolledHost>, ApiError> {
    let id = path_id::<HostId>(params, "id")?;
    rotate(db, user.id, id).await.map(Json)
}

/// Records what a host made of a container job it was sent.
///
/// Authenticated by the host's own `fh_` token rather than by a user
/// credential, exactly as a session daemon's routes are by its `fd_` token:
/// it resolves to *this machine* and to nothing else.
#[skyzen::openapi]
async fn report_job_result(
    params: Params,
    headers: Headers,
    Json(report): Json<ReportJobResult>,
    db: Db,
) -> Outcome<NoContent> {
    accept_job_result(&params, &headers, &report, &db)
        .await
        .into()
}

async fn accept_job_result(
    params: &Params,
    headers: &Headers,
    report: &ReportJobResult,
    db: &Db,
) -> Result<NoContent, ApiError> {
    let host = authenticated(params, headers, db).await?;
    record_job_result(db, &host, report).await?;
    Ok(NoContent)
}

/// Resolves the host a host-scoped request is acting for.
///
/// # Errors
///
/// Returns [`ApiError::MissingCredential`] when nothing was presented and
/// [`ApiError::InvalidHostCredential`] when what was presented is not this
/// machine's live token.
pub async fn authenticated(
    params: &Params,
    headers: &Headers,
    db: &Db,
) -> Result<HostRow, ApiError> {
    let id = path_id::<HostId>(params, "id")?;
    let presented = headers.bearer().ok_or(ApiError::MissingCredential)?;
    if !authenticates(db, id, presented).await? {
        return Err(ApiError::InvalidHostCredential);
    }
    find(db, id).await
}

/// The route a machine enrolls itself through, which carries no user
/// credential.
pub fn public_routes() -> Vec<RouteNode> {
    Route::new(("/v1/hosts/enroll".post(enroll_host),)).into_route_nodes()
}

/// The routes a machine calls with its own token.
pub fn host_routes() -> Vec<RouteNode> {
    Route::new(("/v1/hosts/{id}/job-results".post(report_job_result),)).into_route_nodes()
}

/// The user-scoped host routes.
pub fn routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/hosts".at(list_hosts),
        "/v1/hosts/enrollment-tokens".post(mint_enrollment_token),
        "/v1/hosts/enrollment-tokens/{id}".at(get_enrollment),
        "/v1/hosts/{id}"
            .at(get_host)
            .patch(update_host)
            .delete(delete_host),
        "/v1/hosts/{id}/token/rotate".post(rotate_host_token),
    ))
    .into_route_nodes()
}
