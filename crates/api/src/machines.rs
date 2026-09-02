//! Machines: the priced catalog, and the lifecycle of the one machine a
//! session runs on.
//!
//! The catalog is the same document the agent sees before it decides whether
//! to keep, upgrade, or downgrade its machine, so it carries prices and the
//! minimum-billing flags that make a cheap-looking machine expensive (EC2
//! Mac's 24-hour Apple-license minimum, for one). Resize preserves the disk;
//! stop deallocates compute and keeps it; only archiving a session releases
//! it.

use flyco_core::{
    AUTO_MIN_MEMORY_MIB, AUTO_MIN_VCPUS, CloudProviderKind, CurrentUser, DEFAULT_DISK_GIB,
    MachineCatalogEntry, MachineChoice, MachineDefault, MachineId, MachineSpec, MachineState,
    MachineView, OsFamily, ProviderAccountId, ResizeMachine, SessionId, Usd, UserId,
    auto_linux_choice, curate,
};
use serde::Deserialize;
use skyzen::extract::Query;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::sql;
use skyzen::utils::{Json, State};
use skyzen_services::Db;

use crate::clock::now_unix;
use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::extract::path_id;
use crate::problem::Outcome;
use crate::provisioning;
use crate::respond::Accepted;

/// The columns every read on this path projects.
///
/// `requested_spot` and `spot` are `INTEGER 0/1` — SQLite and D1 have no
/// boolean type — and `bool`'s [`FromColumn`](skyzen_services::sql::FromColumn)
/// reads exactly that.
#[derive(Debug, skyzen::FromRow)]
pub struct MachineRow {
    /// Flyco's identifier for the machine, and the stem of every
    /// provider-native resource name it owns.
    pub id: MachineId,
    session_id: SessionId,
    /// Which linked account created it, and must be used to act on it.
    pub provider_account_id: ProviderAccountId,
    provider: CloudProviderKind,
    machine_type: String,
    region: String,
    disk_gib: u32,
    requested_spot: bool,
    spot: bool,
    /// Where it is in its lifecycle.
    pub state: MachineState,
    hourly_micros: Option<Usd>,
    storage_hourly_micros: Option<Usd>,
    /// The provider's own name for it, once there is one to name.
    pub native_id: Option<String>,
    address: Option<String>,
    created_at_unix: u64,
}

impl From<MachineRow> for MachineView {
    fn from(row: MachineRow) -> Self {
        Self {
            id: row.id,
            session: row.session_id,
            spec: MachineSpec {
                provider: row.provider,
                machine_type: row.machine_type,
                region: row.region.clone(),
                spot: row.requested_spot,
                disk_gib: row.disk_gib,
            },
            state: row.state,
            spot: row.spot,
            hourly: row.hourly_micros,
            storage_hourly: row.storage_hourly_micros,
            region: row.region,
            created_at_unix: row.created_at_unix,
        }
    }
}

impl MachineRow {
    /// What was asked for, as a driver takes it.
    #[must_use]
    pub fn spec(&self) -> MachineSpec {
        MachineSpec {
            provider: self.provider,
            machine_type: self.machine_type.clone(),
            region: self.region.clone(),
            spot: self.requested_spot,
            disk_gib: self.disk_gib,
        }
    }

    /// Whether this row already describes a machine the provider created.
    ///
    /// What makes a redelivered provisioning job a no-op: a machine that is
    /// running and has a provider-native name has been built, and building a
    /// second one for the same session would be a cloud resource nobody is
    /// billing anybody for.
    #[must_use]
    pub const fn is_provisioned(&self) -> bool {
        matches!(self.state, MachineState::Running) && self.native_id.is_some()
    }

    /// Rebuilds what a driver needs to act on this machine.
    ///
    /// A row with no `native_id` names nothing the provider can be asked
    /// about — the machine is still being created — so acting on it is a
    /// conflict rather than a request to retry.
    fn as_provider_machine(&self) -> Result<flyco_provider::Machine, ApiError> {
        Ok(flyco_provider::Machine {
            id: self.id,
            native_id: self.native_id.clone().ok_or(ApiError::MachineNotReady)?,
            region: self.region.clone(),
            state: self.state,
            capacity_mode: if self.spot {
                flyco_provider::CapacityMode::Spot
            } else {
                flyco_provider::CapacityMode::OnDemand
            },
            address: self.address.clone(),
        })
    }
}

/// Runs one lifecycle operation and records what came back.
///
/// The row is updated from the provider's answer rather than from what was
/// asked for: a resize that landed on different capacity, or a start that
/// returned a new address, has to be what the session sees next.
async fn run(
    db: &Db,
    config: &ApiConfig,
    user: UserId,
    params: &Params,
    operation: provisioning::Operation<'_>,
) -> Result<Accepted, ApiError> {
    let session: SessionId = path_id(params, "id")?;
    let row = load(db, user, session).await?;
    let machine = row.as_provider_machine()?;
    let account = provisioning::account(db, config, user, row.provider_account_id).await?;

    let updated = provisioning::operate(&account, &machine, operation)
        .await
        .map_err(|error| ApiError::Provisioning(error.to_string()))?;

    let machine_type = match operation {
        provisioning::Operation::Resize { machine_type } => machine_type.to_owned(),
        provisioning::Operation::Stop
        | provisioning::Operation::Start
        | provisioning::Operation::Destroy => row.machine_type.clone(),
    };

    sql!(
        db,
        "UPDATE machines SET state = {updated.state}, machine_type = {machine_type}, \
         spot = {updated.capacity_mode.is_spot()}, native_id = {updated.native_id.clone()}, \
         address = {updated.address.clone()}, compute_metered_at_unix = {now_unix()} \
         WHERE id = {row.id}"
    )
    .execute()
    .await?;

    tracing::info!(machine = %row.id, state = ?updated.state, "ran a machine lifecycle operation");
    Ok(Accepted)
}

/// Permanently releases the machine and disk belonging to `session`.
///
/// A reserved row without a provider-native id names no external resource,
/// so it can be marked destroyed directly. A provisioned row is destroyed
/// through the exact linked account that created it before the database is
/// allowed to claim the resource is gone.
///
/// # Errors
///
/// Returns [`ApiError`] if the session owns no machine, the provider refuses
/// destruction, or the destroyed state cannot be persisted.
pub async fn destroy_for_archive(
    db: &Db,
    config: &ApiConfig,
    user: UserId,
    session: SessionId,
) -> Result<(), ApiError> {
    let row = load(db, user, session).await?;
    if row.state == MachineState::Destroyed {
        return Ok(());
    }

    if row.native_id.is_some() {
        let machine = row.as_provider_machine()?;
        let account = provisioning::account(db, config, user, row.provider_account_id).await?;
        provisioning::operate(&account, &machine, provisioning::Operation::Destroy)
            .await
            .map_err(|error| ApiError::Provisioning(error.to_string()))?;
    }

    let destroyed = MachineState::Destroyed;
    sql!(
        db,
        "UPDATE machines SET state = {destroyed}, hourly_micros = NULL, \
         storage_hourly_micros = NULL, native_id = NULL, \
         address = NULL WHERE id = {row.id}"
    )
    .execute()
    .await?;
    Ok(())
}

/// Loads the machine a session runs on, scoped to its owner.
///
/// The join through `sessions` is what enforces ownership: a machine is
/// reachable only via the session it serves, and that session carries the
/// `user_id`.
async fn load(db: &Db, user: UserId, session: SessionId) -> Result<MachineRow, ApiError> {
    sql!(
        db,
        "SELECT id, session_id, provider_account_id, provider, machine_type, region, \
         disk_gib, requested_spot, spot, state, hourly_micros, storage_hourly_micros, native_id, address, \
         created_at_unix \
         FROM machines \
         WHERE session_id = (SELECT id FROM sessions WHERE id = {session} AND user_id = {user})"
    )
    .fetch_optional()
    .await?
    .ok_or(ApiError::MachineNotFound)
}

/// Narrows the machine catalog.
///
/// Every field is optional: an unfiltered catalog is the honest default,
/// because a user with one linked provider should not have to name it.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
pub struct CatalogFilter {
    /// Only machines from this provider.
    pub provider: Option<CloudProviderKind>,
    /// Only machines in this provider-native region.
    pub region: Option<String>,
    /// Only machines running this operating system family.
    pub os: Option<OsFamily>,
}

/// Lists the machine types the caller can provision, with their prices.
#[skyzen::openapi]
async fn get_catalog(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    Query(filter): Query<CatalogFilter>,
    db: Db,
) -> Outcome<Json<Vec<MachineCatalogEntry>>> {
    catalog(&db, &config, user.id, &filter)
        .await
        .map(Json)
        .into()
}

/// Merges every linked account's catalog into one curated document.
///
/// Curated by [`curate`], which is the whole of docs/ux.md §7.6: newest
/// generation of each family, then the strict Pareto frontier on price
/// against capacity, then ordered by price. Every reader of the catalog goes
/// through here — the chip's slider, `GET /v1/machines/default`, and the
/// resize the agent asks for — so the user and the agent are choosing from
/// the same short list rather than from two different views of one cloud's
/// thousands of redundant rows.
///
/// What each provider returns is already narrowed to what that account can
/// actually deploy — for Azure that means SKU restrictions, quota *and* the
/// subscription's own region policy, three independent gates — so a machine
/// reaching this list is one flyco can really provision. An account whose
/// catalog cannot be read is skipped with a warning rather than failing the
/// whole request: one expired credential should not hide the machines every
/// other account can still offer.
pub(crate) async fn catalog(
    db: &Db,
    config: &ApiConfig,
    user: UserId,
    filter: &CatalogFilter,
) -> Result<Vec<MachineCatalogEntry>, ApiError> {
    let accounts = provisioning::accounts_for(db, config, user, filter.provider).await?;

    let mut entries = Vec::new();
    for account in accounts {
        match provisioning::catalog(&account).await {
            Ok(mut offered) => entries.append(&mut offered),
            Err(error) => {
                tracing::warn!(
                    account = %account.id,
                    %error,
                    "skipping a provider account whose catalog could not be read"
                );
            }
        }
    }

    entries.retain(|entry| {
        filter
            .region
            .as_ref()
            .is_none_or(|region| entry.region.eq_ignore_ascii_case(region))
            && filter.os.is_none_or(|os| entry.os == os)
    });

    // Curation is applied after filtering, not before: a frontier computed
    // over every region and then narrowed to one would hide types that are
    // on the frontier *of that region*, which is the only frontier a user
    // choosing a region can act on.
    Ok(curate(entries))
}

/// Whether a caller who names no machine wants interruptible capacity.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
pub struct DefaultMachineQuery {
    /// Whether to price and pick against spot capacity. Spot is the default
    /// because it is cheaper and flyco handles eviction.
    pub spot: Option<bool>,
}

/// Describes the machine flyco would provision if the caller named none.
///
/// The one honest way to show a user what "let flyco choose" means before
/// they commit to it: the same function `POST /v1/sessions` runs, answered
/// with the catalog entry behind it so the price and the size come from the
/// choice rather than from a second lookup that could disagree with it.
#[skyzen::openapi]
async fn get_default_machine(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    Query(query): Query<DefaultMachineQuery>,
    db: Db,
) -> Outcome<Json<MachineDefault>> {
    automatic(&db, &config, user.id, query.spot.unwrap_or(true))
        .await
        .map(Json)
        .into()
}

/// Picks the machine flyco provisions when the caller names none.
///
/// # Errors
///
/// Returns [`ApiError::NoDeployableLinuxMachine`] if no linked account
/// offers a Linux type big enough for flyco to choose on its own.
pub(crate) async fn automatic(
    db: &Db,
    config: &ApiConfig,
    user: UserId,
    spot: bool,
) -> Result<MachineDefault, ApiError> {
    let entries = catalog(
        db,
        config,
        user,
        &CatalogFilter {
            provider: None,
            region: None,
            os: Some(OsFamily::Linux),
        },
    )
    .await?;
    let entry = auto_linux_choice(&entries, spot).ok_or(nothing_big_enough())?;
    // `auto_linux_choice` only ever returns an entry with an account; the
    // read is written as a refusal rather than an unwrap so the invariant
    // is enforced here too, where it is used.
    let account = entry.account.ok_or_else(nothing_big_enough)?;
    Ok(MachineDefault {
        choice: MachineChoice {
            provider_account: account,
            machine_type: entry.machine_type.clone(),
            region: entry.region.clone(),
            spot,
            disk_gib: DEFAULT_DISK_GIB,
        },
        entry: entry.clone(),
    })
}

/// The refusal a catalog with nothing flyco may pick answers with.
const fn nothing_big_enough() -> ApiError {
    ApiError::NoDeployableLinuxMachine {
        vcpus: AUTO_MIN_VCPUS,
        memory_gib: AUTO_MIN_MEMORY_MIB / 1024,
    }
}

/// Describes the machine a session is running on.
#[skyzen::openapi]
async fn get_session_machine(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Outcome<Json<MachineView>> {
    read_machine(&db, user.id, &params).await.map(Json).into()
}

async fn read_machine(db: &Db, user: UserId, params: &Params) -> Result<MachineView, ApiError> {
    let session: SessionId = path_id(params, "id")?;
    Ok(load(db, user, session).await?.into())
}

/// Moves a session's machine to another type, keeping its disk.
///
/// Answers `202`: the provider destroys and recreates the compute half
/// asynchronously, and the session's daemon reconnects when it is back.
#[skyzen::openapi]
async fn resize_session_machine(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    params: Params,
    Json(request): Json<ResizeMachine>,
    db: Db,
) -> Outcome<Accepted> {
    run(
        &db,
        &config,
        user.id,
        &params,
        provisioning::Operation::Resize {
            machine_type: &request.machine_type,
        },
    )
    .await
    .into()
}

/// Deallocates a session's machine, keeping its disk.
///
/// The session stops costing compute and keeps everything on disk, which is
/// what makes a paused session cheap rather than lost.
#[skyzen::openapi]
async fn stop_session_machine(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    params: Params,
    db: Db,
) -> Outcome<Accepted> {
    run(
        &db,
        &config,
        user.id,
        &params,
        provisioning::Operation::Stop,
    )
    .await
    .into()
}

/// Brings a stopped session's machine back, on the same disk.
#[skyzen::openapi]
async fn start_session_machine(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    params: Params,
    db: Db,
) -> Outcome<Accepted> {
    run(
        &db,
        &config,
        user.id,
        &params,
        provisioning::Operation::Start,
    )
    .await
    .into()
}

// ── The machine row, as the provisioning queue writes it ──
//
// A session has exactly one machine row for its whole life — `session_id` is
// UNIQUE — and that row is what makes provisioning safe to run twice. It is
// written empty when the session is created, filled in when the provider
// answers, and reset rather than replaced when the session is resumed, so
// the machine id (and therefore every provider-native resource name derived
// from it) is stable across attempts. A retry that reaches the provider
// again is then an update of the same virtual machine or container, not a
// second one.

/// Reserves the row a session's machine will occupy.
///
/// No price is recorded yet, and the absence is the point: nothing exists to
/// meter, and a rate written before the capacity is known would be the rate
/// of the machine that was *asked* for. The queue consumer records the real
/// one alongside the capacity it actually obtained.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails — including the UNIQUE
/// violation that a second machine for one session would be.
pub async fn reserve(
    db: &Db,
    session: SessionId,
    account: ProviderAccountId,
    spec: &MachineSpec,
) -> Result<MachineId, ApiError> {
    let id = MachineId::generate();
    let provisioning = MachineState::Provisioning;
    let now = now_unix();

    sql!(
        db,
        "INSERT INTO machines \
         (id, session_id, provider_account_id, provider, machine_type, region, disk_gib, \
          requested_spot, spot, state, created_at_unix) \
         VALUES ({id}, {session}, {account}, {spec.provider}, {spec.machine_type.clone()}, \
                 {spec.region.clone()}, {spec.disk_gib}, {spec.spot}, {spec.spot}, \
                 {provisioning}, {now})"
    )
    .execute()
    .await?;

    Ok(id)
}

/// Reads a session's machine row without scoping it to an owner.
///
/// The queue's read: a job was enqueued by a handler that had already proved
/// the caller owns the session, so re-deriving that here would be a second
/// copy of a check that already happened. Every user-facing read goes
/// through [`load`], which joins through `sessions` for exactly that reason.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn for_session(db: &Db, session: SessionId) -> Result<Option<MachineRow>, ApiError> {
    Ok(sql!(
        db,
        "SELECT id, session_id, provider_account_id, provider, machine_type, region, \
         disk_gib, requested_spot, spot, state, hourly_micros, storage_hourly_micros, native_id, address, \
         created_at_unix \
         FROM machines WHERE session_id = {session}"
    )
    .fetch_optional()
    .await?)
}

/// Records what the provider actually built.
///
/// `spot` and `hourly_micros` come from the machine that exists rather than
/// from the request that asked for it: a spot request a provider cannot
/// honour is answered with on-demand capacity, and the price billed follows
/// what was obtained.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn record(
    db: &Db,
    machine: &flyco_provider::Machine,
    hourly: Option<Usd>,
    storage_hourly: Option<Usd>,
) -> Result<(), ApiError> {
    let spot = machine.capacity_mode.is_spot();
    let now = now_unix();
    sql!(
        db,
        "UPDATE machines SET state = {machine.state}, spot = {spot}, \
         hourly_micros = {hourly}, storage_hourly_micros = {storage_hourly}, \
         compute_meter_started_at_unix = {now}, compute_metered_at_unix = {now}, \
         storage_meter_started_at_unix = {now}, storage_metered_at_unix = {now}, \
         native_id = {machine.native_id.clone()}, address = {machine.address.clone()} \
         WHERE id = {machine.id}"
    )
    .execute()
    .await?;
    Ok(())
}

/// Puts a session's existing machine row back into `provisioning` so the
/// queue will build it again.
///
/// The row keeps its identity, which is what lets a resume reuse the
/// provider-native resource names the session already had: an Azure `PUT`
/// against the same names updates the machine it finds, and the podman
/// script byo-ssh renders removes the container before recreating it. A new
/// row would mean a new set of names and, on a provider that had not
/// released the old ones, two machines for one session.
///
/// # Errors
///
/// Returns [`ApiError::MachineNotFound`] if the session never had a machine
/// row reserved, which would mean it was not created through
/// `POST /v1/sessions`.
pub async fn reset_for_resume(db: &Db, session: SessionId) -> Result<MachineId, ApiError> {
    let row = for_session(db, session)
        .await?
        .ok_or(ApiError::MachineNotFound)?;
    let provisioning = MachineState::Provisioning;

    sql!(
        db,
        "UPDATE machines SET state = {provisioning} WHERE id = {row.id}"
    )
    .execute()
    .await?;

    Ok(row.id)
}

/// The user-scoped machine routes.
pub fn routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/machines/catalog".at(get_catalog),
        "/v1/machines/default".at(get_default_machine),
        "/v1/sessions/{id}/machine".at(get_session_machine),
        "/v1/sessions/{id}/machine/resize".post(resize_session_machine),
        "/v1/sessions/{id}/machine/stop".post(stop_session_machine),
        "/v1/sessions/{id}/machine/start".post(start_session_machine),
    ))
    .into_route_nodes()
}
