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
    CloudProviderKind, CurrentUser, MachineCatalogEntry, MachineId, MachineSpec, MachineState,
    MachineView, OsFamily, ProviderAccountId, ResizeMachine, SessionId, Usd, UserId,
};
use serde::Deserialize;
use skyzen::extract::Query;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::utils::{Json, State};
use skyzen_services::Db;

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
struct MachineRow {
    id: MachineId,
    session_id: SessionId,
    provider_account_id: ProviderAccountId,
    provider: CloudProviderKind,
    machine_type: String,
    region: String,
    disk_gib: u32,
    requested_spot: bool,
    spot: bool,
    state: MachineState,
    hourly_micros: Option<Usd>,
    native_id: Option<String>,
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
            region: row.region,
            created_at_unix: row.created_at_unix,
        }
    }
}

/// Every column the machine projection needs, in one place so the three
/// readers below cannot drift apart.
const MACHINE_COLUMNS: &str = "id, session_id, provider_account_id, provider, machine_type, \
                               region, disk_gib, requested_spot, spot, state, hourly_micros, \
                               native_id, address, created_at_unix";

impl MachineRow {
    /// Rebuilds what a driver needs to act on this machine.
    ///
    /// A row with no `native_id` names nothing the provider can be asked
    /// about — the machine is still being created — so acting on it is a
    /// conflict rather than a request to retry.
    fn as_provider_machine(&self) -> Result<flyco_provider::Machine, ApiError> {
        Ok(flyco_provider::Machine {
            id: self.id,
            native_id: self.native_id.clone().ok_or(ApiError::MachineNotReady)?,
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
        provisioning::Operation::Stop | provisioning::Operation::Start => row.machine_type.clone(),
    };

    db.query(
        "UPDATE machines SET state = ?, machine_type = ?, spot = ?, native_id = ?, address = ? \
         WHERE id = ?",
    )
    .bind(updated.state)
    .bind(machine_type)
    .bind(updated.capacity_mode.is_spot())
    .bind(updated.native_id.clone())
    .bind(updated.address.clone())
    .bind(row.id)
    .execute()
    .await?;

    tracing::info!(machine = %row.id, state = ?updated.state, "ran a machine lifecycle operation");
    Ok(Accepted)
}

/// Loads the machine a session runs on, scoped to its owner.
///
/// The join through `sessions` is what enforces ownership: a machine is
/// reachable only via the session it serves, and that session carries the
/// `user_id`.
async fn load(db: &Db, user: UserId, session: SessionId) -> Result<MachineRow, ApiError> {
    let sql = format!(
        "SELECT {MACHINE_COLUMNS} FROM machines \
         WHERE session_id = (SELECT id FROM sessions WHERE id = ? AND user_id = ?)"
    );
    db.query(&sql)
        .bind(session)
        .bind(user)
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

/// Merges every linked account's catalog into one document.
///
/// What each provider returns is already narrowed to what that account can
/// actually deploy — for Azure that means SKU restrictions, quota *and* the
/// subscription's own region policy, three independent gates — so a machine
/// reaching this list is one flyco can really provision. An account whose
/// catalog cannot be read is skipped with a warning rather than failing the
/// whole request: one expired credential should not hide the machines every
/// other account can still offer.
async fn catalog(
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
    Ok(entries)
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

/// The user-scoped machine routes.
pub fn routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/machines/catalog".at(get_catalog),
        "/v1/sessions/{id}/machine".at(get_session_machine),
        "/v1/sessions/{id}/machine/resize".post(resize_session_machine),
        "/v1/sessions/{id}/machine/stop".post(stop_session_machine),
        "/v1/sessions/{id}/machine/start".post(start_session_machine),
    ))
    .into_route_nodes()
}
