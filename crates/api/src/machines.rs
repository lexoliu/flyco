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
    AUTO_MIN_MEMORY_MIB, AUTO_MIN_VCPUS, AgentMachineView, BillingMinimum, ClientEvent,
    CloudProviderKind, ControlToDaemon, CurrentUser, DEFAULT_DISK_GIB, MachineCapacity,
    MachineCatalog, MachineCatalogEntry, MachineChoice, MachineDefault, MachineId, MachineSpec,
    MachineState, MachineView, OsFamily, ProviderAccountId, RegionLocation, ResizeMachine, Runtime,
    SessionId, SessionMachine, StopReason, Usd, UserId, auto_linux_choice, curate,
};
use serde::Deserialize;
use skyzen::extract::Query;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::sql;
use skyzen::utils::{Json, State};
use skyzen_services::{Db, Kv, Queue};

use crate::catalog;
use crate::clock::now_unix;
use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::extract::{CallerLocation, path_id};
use crate::github::{GithubClient, GithubOauth};
use crate::problem::Outcome;
use crate::provisioning;
use crate::respond::Accepted;
use crate::rooms::{HostRooms, Rooms};
use crate::sessions;

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
    /// Whether it is a virtual machine or a managed container.
    ///
    /// On the row rather than derived from `provider`, because one account
    /// offers both: a subscription sells `Standard_D4s_v6` as a VM and
    /// `aca-4x8` as a Container Apps job. It is what a driver needs to know
    /// what a stop does to the disk, and what the daemon's configuration
    /// states so the machine knows what to do with a `SIGTERM`.
    runtime: Runtime,
    region: String,
    disk_gib: u32,
    requested_spot: bool,
    spot: bool,
    /// Where it is in its lifecycle.
    pub state: MachineState,
    hourly_micros: Option<Usd>,
    storage_hourly_micros: Option<Usd>,
    /// vCPUs the catalog published for the type it is running.
    ///
    /// Recorded on the row rather than looked up on demand: the agent asks
    /// what machine it is on far more often than a catalog can be read, and
    /// a type curation has since dropped is still a machine that is running.
    /// `NULL` for hardware the user registered, whose size flyco has never
    /// measured.
    vcpus: Option<u32>,
    memory_mib: Option<u64>,
    /// Hours the provider billed the moment it booted, for a license-bound
    /// type. `NULL` for every type that imposes no floor.
    minimum_hours: Option<u32>,
    minimum_charge_micros: Option<Usd>,
    /// The provider's own name for it, once there is one to name.
    ///
    /// The container name on a machine the user owns, and the instance or
    /// resource id everywhere else.
    pub native_id: Option<String>,
    /// The Podman volume holding the session's checkout, on a host.
    ///
    /// `NULL` for every cloud machine, whose disk is part of the instance
    /// [`native_id`](Self::native_id) already names, and written when the
    /// host reports the container it actually created.
    pub volume_name: Option<String>,
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
                runtime: row.runtime,
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
    /// The machine, as the agent driving this session is told about it.
    ///
    /// Assembled from the row alone. Every fact the agent needs to decide
    /// whether to keep working here was written when the machine was built
    /// or last resized, so answering costs one read of one row rather than a
    /// walk through a provider's catalog.
    #[must_use]
    pub fn session_machine(&self) -> SessionMachine {
        SessionMachine {
            machine_type: self.machine_type.clone(),
            hourly: self.hourly_micros,
            spot: self.spot,
            capacity: self
                .vcpus
                .zip(self.memory_mib)
                .map(|(vcpus, memory_mib)| MachineCapacity { vcpus, memory_mib }),
            minimum: self
                .minimum_hours
                .zip(self.minimum_charge_micros)
                .map(|(hours, charge)| BillingMinimum { hours, charge }),
        }
    }

    /// The provider-native type this machine is running.
    #[must_use]
    pub fn machine_type(&self) -> String {
        self.machine_type.clone()
    }

    /// The session this machine serves. A machine serves exactly one.
    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session_id
    }

    /// What was asked for, as a driver takes it.
    #[must_use]
    pub fn spec(&self) -> MachineSpec {
        MachineSpec {
            provider: self.provider,
            machine_type: self.machine_type.clone(),
            runtime: self.runtime,
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
    pub(crate) fn as_provider_machine(&self) -> Result<flyco_provider::Machine, ApiError> {
        Ok(flyco_provider::Machine {
            id: self.id,
            native_id: self.native_id.clone().ok_or(ApiError::MachineNotReady)?,
            // What the row was provisioned as, which is what decides whether
            // the driver's stop is a deallocation or the end of a container
            // execution.
            runtime: self.runtime,
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
    github: &impl GithubOauth,
    hosts: &HostRooms,
    user: UserId,
    session: SessionId,
    operation: provisioning::Operation<'_>,
) -> Result<flyco_provider::Machine, ApiError> {
    let row = load(db, user, session).await?;
    let machine = row.as_provider_machine()?;
    let account = provisioning::account(db, config, github, user, row.provider_account_id).await?;

    // A machine the user owns is acted on by asking the machine, so a host
    // that is not connected is a refusal the caller can act on — start the
    // daemon — rather than a provider failure they cannot.
    provisioning::require_host_online(hosts, &account).await?;
    let updated = provisioning::operate(hosts, &account, &machine, operation)
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
        // The stop mark is cleared here and not on its own: a machine
        // a driver has just acted on is no longer on its way out, whatever
        // the last daemon on it said, and a stale mark would have the
        // container drivers read a live execution as a departing one.
        "UPDATE machines SET state = {updated.state}, machine_type = {machine_type}, \
         spot = {updated.capacity_mode.is_spot()}, native_id = {updated.native_id.clone()}, \
         address = {updated.address.clone()}, compute_metered_at_unix = {now_unix()}, \
         stopping_since_unix = NULL, stopping_reason = NULL \
         WHERE id = {row.id}"
    )
    .execute()
    .await?;

    tracing::info!(machine = %row.id, state = ?updated.state, "ran a machine lifecycle operation");
    Ok(updated)
}

/// The catalog a session's machine may be moved within.
///
/// A resize keeps the disk, so it cannot cross an account or a region — the
/// disk is in one of each — which makes those two, plus the provider, the
/// filter rather than a preference. The runtime is there for the same
/// reason and is not a fourth kind of thing: moving between a virtual
/// machine and a managed container is not a change of size, it is a
/// different machine with a different bargain about what survives a stop,
/// and offering it here would be offering something the resize route cannot
/// express.
///
/// What comes back is the curated list of docs/ux.md §7.6, the same one the
/// user's slider and the agent's `machine_resize` tool read.
fn resize_filter(row: &MachineRow) -> CatalogFilter {
    CatalogFilter {
        provider: Some(row.provider),
        account: Some(row.provider_account_id),
        region: Some(row.region.clone()),
        os: None,
        runtime: Some(row.runtime),
    }
}

/// Every type this session's machine can become.
///
/// # Errors
///
/// Returns [`ApiError`] if the session owns no machine or a provider catalog
/// could not be read.
pub(crate) async fn resize_catalog(
    db: &Db,
    config: &ApiConfig,
    github: &impl GithubOauth,
    kv: &Kv,
    user: UserId,
    session: SessionId,
) -> Result<Vec<MachineCatalogEntry>, ApiError> {
    let row = load(db, user, session).await?;
    Ok(catalog(
        db,
        config,
        github,
        kv,
        Refresh::ReadOnly,
        user,
        &resize_filter(&row),
    )
    .await?
    .entries)
}

/// Resolves a requested type against the catalog the session may move within.
async fn offered(
    db: &Db,
    config: &ApiConfig,
    github: &impl GithubOauth,
    kv: &Kv,
    user: UserId,
    row: &MachineRow,
    machine_type: &str,
) -> Result<MachineCatalogEntry, ApiError> {
    catalog(
        db,
        config,
        github,
        kv,
        Refresh::ReadOnly,
        user,
        &resize_filter(row),
    )
    .await?
    .entries
    .into_iter()
    .find(|entry| entry.machine_type == machine_type)
    .ok_or_else(|| ApiError::MachineTypeNotOffered(machine_type.to_owned()))
}

/// Moves a session's machine to another type and tells everyone watching.
///
/// The user's own resize, and the one a `machine_resize_license_bound`
/// approval performs once the user has agreed to the minimum charge. Neither
/// is gated on the licence, because in both a person decided.
///
/// # Errors
///
/// Returns [`ApiError::MachineTypeNotOffered`] if the type is not one this
/// machine can become, or [`ApiError`] if the provider refuses the resize.
#[expect(
    clippy::too_many_arguments,
    reason = "a resize names the session, the caller, the type, and every \
              service it touches"
)]
pub(crate) async fn resize(
    db: &Db,
    config: &ApiConfig,
    github: &impl GithubOauth,
    kv: &Kv,
    rooms: &Rooms,
    hosts: &HostRooms,
    user: UserId,
    session: SessionId,
    machine_type: &str,
) -> Result<(), ApiError> {
    let row = load(db, user, session).await?;
    let entry = offered(db, config, github, kv, user, &row, machine_type).await?;
    apply(
        db, config, github, rooms, hosts, user, session, &row, &entry,
    )
    .await
}

/// The agent's resize, refused when it would spend money on its own.
///
/// A license-bound type bills its minimum the moment it boots, which is a
/// commitment rather than a choice of machine — so the daemon raises an
/// [`ApprovalPayload::MachineResizeLicenseBound`] and the user's decision
/// comes back through [`resize`]. The refusal is here rather than only in the
/// daemon's tool because a rule the agent could talk its way past is not a
/// rule (docs/ARCHITECTURE.md: approvals are enforced by flyco, never by
/// prompt engineering).
///
/// # Errors
///
/// Returns [`ApiError::LicenseBoundResizeNeedsApproval`] for a type with a
/// billing minimum, [`ApiError::MachineTypeNotOffered`] for a type this
/// machine cannot become, or [`ApiError`] if the provider refuses.
///
/// [`ApprovalPayload::MachineResizeLicenseBound`]: flyco_core::ApprovalPayload::MachineResizeLicenseBound
#[expect(
    clippy::too_many_arguments,
    reason = "the agent's resize names the same services the user's does"
)]
pub(crate) async fn resize_for_agent(
    db: &Db,
    config: &ApiConfig,
    github: &impl GithubOauth,
    kv: &Kv,
    rooms: &Rooms,
    hosts: &HostRooms,
    user: UserId,
    session: SessionId,
    machine_type: &str,
) -> Result<(), ApiError> {
    let row = load(db, user, session).await?;
    let entry = offered(db, config, github, kv, user, &row, machine_type).await?;
    on_the_agents_authority(&entry)?;
    apply(
        db, config, github, rooms, hosts, user, session, &row, &entry,
    )
    .await
}

/// Whether an agent may move onto this type without asking.
///
/// The whole of the licence rule, in one place, so it reads the same way it
/// is enforced: a type that bills a minimum the moment it boots costs the
/// user money before it does any work, and that is a decision a person
/// makes.
fn on_the_agents_authority(entry: &MachineCatalogEntry) -> Result<(), ApiError> {
    entry.billing_minimum().map_or(Ok(()), |minimum| {
        Err(ApiError::LicenseBoundResizeNeedsApproval {
            machine_type: entry.machine_type.clone(),
            hours: minimum.hours,
        })
    })
}

/// Performs a resolved resize: the provider call, the new price, the notice.
///
/// The price is rewritten from the entry the machine actually became, not
/// left at what the old type cost: every budget signal after this is
/// computed from it, and a session billed at its previous rate would pause
/// at the wrong moment.
#[expect(
    clippy::too_many_arguments,
    reason = "a resize reaches both room namespaces — the session's, to tell the \
              agent, and the host's, because a machine somebody owns is changed by \
              asking the machine — as well as the row it is changing and the entry \
              it is changing to. Bundling them would hide which of them a resize \
              actually touches"
)]
async fn apply(
    db: &Db,
    config: &ApiConfig,
    github: &impl GithubOauth,
    rooms: &Rooms,
    hosts: &HostRooms,
    user: UserId,
    session: SessionId,
    row: &MachineRow,
    entry: &MachineCatalogEntry,
) -> Result<(), ApiError> {
    let updated = run(
        db,
        config,
        github,
        hosts,
        user,
        session,
        provisioning::Operation::Resize {
            machine_type: &entry.machine_type,
        },
    )
    .await?;

    let spot = updated.capacity_mode.is_spot();
    let built = SessionMachine::of(entry, spot);
    let storage_hourly = entry.pricing.storage_hourly(row.disk_gib);
    let vcpus = built.capacity.as_ref().map(|capacity| capacity.vcpus);
    let memory_mib = built.capacity.as_ref().map(|capacity| capacity.memory_mib);
    let minimum_hours = built.minimum.map(|minimum| minimum.hours);
    let minimum_charge = built.minimum.map(|minimum| minimum.charge);
    sql!(
        db,
        "UPDATE machines SET hourly_micros = {built.hourly}, \
         storage_hourly_micros = {storage_hourly}, vcpus = {vcpus}, \
         memory_mib = {memory_mib}, minimum_hours = {minimum_hours}, \
         minimum_charge_micros = {minimum_charge} WHERE id = {row.id}"
    )
    .execute()
    .await?;

    announce_machine_change(db, rooms, session, &built).await;
    Ok(())
}

/// Reads the machine a session runs on, as its agent is told about it.
///
/// One row and one session column: what the machine is, and who chose it.
/// The second is what the agent's instructions turn on — a machine the user
/// picked is not one to trade away for a faster build (docs/ux.md §9.5).
///
/// # Errors
///
/// Returns [`ApiError::MachineNotFound`] if the session owns no machine, or
/// [`ApiError`] if the database fails.
pub(crate) async fn agent_view(
    db: &Db,
    user: UserId,
    session: SessionId,
) -> Result<AgentMachineView, ApiError> {
    let row = load(db, user, session).await?;
    Ok(AgentMachineView {
        origin: sessions::machine_origin(db, session).await?,
        machine: row.session_machine(),
        state: row.state,
        region: row.region,
    })
}

/// Tells the session's browsers and its daemon that the machine changed.
///
/// Two messages for two audiences and neither is optional: the transcript
/// says `Switched to … · restarted the machine · disk kept` (docs/ux.md
/// §9.5), and the agent has to be told in the conversation that every
/// process it started is gone. The daemon's copy is held for it while it is
/// away — a resize restarts the machine, so the daemon is *always*
/// disconnected at this moment and a command dropped for want of a listener
/// would be the only one that ever mattered.
///
/// A room that cannot be reached costs the notice, not the resize: the
/// machine has already changed, and failing the request here would tell the
/// caller a resize did not happen when it did.
async fn announce_machine_change(
    db: &Db,
    rooms: &Rooms,
    session: SessionId,
    built: &SessionMachine,
) {
    let event = ClientEvent::MachineChanged {
        machine_type: built.machine_type.clone(),
        hourly: built.hourly,
        spot: built.spot,
        restarted: true,
    };
    if let Err(error) = rooms.broadcast(db, session, &event).await {
        tracing::warn!(%session, %error, "a machine change did not reach the session's watchers");
    }

    let command = ControlToDaemon::MachineChanged {
        machine_type: built.machine_type.clone(),
        hourly: built.hourly,
        spot: built.spot,
        restarted: true,
    };
    if let Err(error) = rooms.command(db, session, &command).await {
        tracing::warn!(%session, %error, "a machine change was not held for the session's daemon");
    }
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
    github: &impl GithubOauth,
    hosts: &HostRooms,
    user: UserId,
    session: SessionId,
) -> Result<(), ApiError> {
    let row = load(db, user, session).await?;
    if row.state == MachineState::Destroyed {
        return Ok(());
    }

    if row.native_id.is_some() {
        let machine = row.as_provider_machine()?;
        let account =
            provisioning::account(db, config, github, user, row.provider_account_id).await?;
        provisioning::require_host_online(hosts, &account).await?;
        provisioning::operate(hosts, &account, &machine, provisioning::Operation::Destroy)
            .await
            .map_err(|error| ApiError::Provisioning(error.to_string()))?;
    }

    let destroyed = MachineState::Destroyed;
    sql!(
        db,
        "UPDATE machines SET state = {destroyed}, hourly_micros = NULL, \
         storage_hourly_micros = NULL, native_id = NULL, \
         address = NULL, bootstrap_enc = NULL WHERE id = {row.id}"
    )
    .execute()
    .await?;
    Ok(())
}

/// Releases the row of a machine that was never built.
///
/// A session that fails before its provider handed anything over has a
/// row in `provisioning` with no native id: a reservation, not a machine.
/// A row that names a real resource is left alone — it is torn down
/// through its own account on archive — so a stray `UPDATE` here can never
/// make the database forget a machine that is still running up a bill.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn release_unbuilt(db: &Db, session: SessionId) -> Result<(), ApiError> {
    let destroyed = MachineState::Destroyed;
    let provisioning = MachineState::Provisioning;
    sql!(
        db,
        "UPDATE machines SET state = {destroyed}, hourly_micros = NULL, \
         storage_hourly_micros = NULL, bootstrap_enc = NULL \
         WHERE session_id = {session} \
         AND state = {provisioning} AND native_id IS NULL"
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
        "SELECT id, session_id, provider_account_id, provider, machine_type, runtime, region, \
         disk_gib, requested_spot, spot, state, hourly_micros, storage_hourly_micros, \
         vcpus, memory_mib, minimum_hours, minimum_charge_micros, native_id, volume_name, \
         address, created_at_unix \
         FROM machines \
         WHERE session_id = (SELECT id FROM sessions WHERE id = {session} AND user_id = {user})"
    )
    .fetch_optional()
    .await?
    .ok_or(ApiError::MachineNotFound)
}

/// Whether reading the catalog may also ask for it to be brought up to date.
///
/// A read that finds an account unread or stale can put a refresh on the
/// provisioning queue — but only the reads a *person* is waiting on should.
/// A resize resolving the type it is moving to reads the catalog of an
/// account that was read long before the machine it is resizing existed, and
/// asking for a refresh there would put a message on the queue for every
/// resize and every `machine_catalog` call an agent makes in a loop.
#[derive(Debug, Clone, Copy)]
pub enum Refresh<'a> {
    /// Ask the provisioning queue to read anything missing or stale.
    Ask(&'a Queue),
    /// Read what is there, and ask for nothing.
    ReadOnly,
}

/// Narrows the machine catalog.
///
/// Every field is optional: an unfiltered catalog is the honest default,
/// because a user with one linked provider should not have to name it.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
pub struct CatalogFilter {
    /// Only machines from this provider.
    pub provider: Option<CloudProviderKind>,
    /// Only machines this linked account can deploy.
    ///
    /// Narrower than [`Self::provider`], and a different question: a user
    /// with two Azure subscriptions is choosing between two bills, and a
    /// card that spoke for both of them would name a machine the account it
    /// sits on cannot create.
    pub account: Option<ProviderAccountId>,
    /// Only machines in this provider-native region.
    pub region: Option<String>,
    /// Only machines running this operating system family.
    pub os: Option<OsFamily>,
    /// Only virtual machines, or only managed containers.
    ///
    /// What a *resize* narrows by, for the same reason it narrows by
    /// account and region: those are facts about the machine that already
    /// exists, and a resize changes its size rather than what it is. The
    /// composer's picker leaves this open, because a new session may be
    /// either.
    pub runtime: Option<Runtime>,
}

/// Lists the machine types the caller can provision, with their prices.
#[skyzen::openapi]
async fn get_catalog(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(github): State<GithubClient>,
    Query(filter): Query<CatalogFilter>,
    db: Db,
    kv: Kv,
    queue: Queue,
) -> Outcome<Json<MachineCatalog>> {
    catalog(
        &db,
        &config,
        &github,
        &kv,
        Refresh::Ask(&queue),
        user.id,
        &filter,
    )
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
    github: &impl GithubOauth,
    kv: &Kv,
    refresh: Refresh<'_>,
    user: UserId,
    filter: &CatalogFilter,
) -> Result<MachineCatalog, ApiError> {
    let mut accounts =
        provisioning::accounts_for(db, config, github, user, filter.provider).await?;
    // Narrowed before the reads rather than after: reading the document of
    // an account nobody will look at is a round trip for nothing, and
    // asking for its refresh would put a message on the queue for nothing.
    accounts.retain(|account| filter.account.is_none_or(|wanted| account.id == wanted));

    let now = now_unix();
    let mut entries = Vec::new();
    let mut pending_accounts = Vec::new();
    for account in accounts {
        if !account.catalog_is_remote() {
            // Hardware the user owns answers from the row this request has
            // already loaded, and its online/offline state changes by the
            // minute — see `provisioning::catalog_of`. There is no read to
            // cache and nothing a cache could do but go stale.
            match provisioning::catalog(&account).await {
                Ok(mut offered) => entries.append(&mut offered),
                Err(error) => tracing::warn!(
                    account = %account.id,
                    %error,
                    "skipping a machine the user owns that could not describe itself"
                ),
            }
            continue;
        }

        // Nothing read yet is "not yet" rather than "nothing", and the
        // caller is told which; either way the account is due to be read.
        let due = if let Some(document) = catalog::read(kv, account.id).await? {
            entries.extend(document.entries().cloned());
            document.is_stale(now)
        } else {
            pending_accounts.push(account.id);
            true
        };
        if let (true, Refresh::Ask(queue)) = (due, refresh)
            && catalog::ask_for_refresh(kv, queue, user, account.id).await?
        {
            tracing::info!(account = %account.id, "asked for a catalog refresh");
        }
    }

    entries.retain(|entry| {
        filter
            .region
            .as_ref()
            .is_none_or(|region| entry.region.eq_ignore_ascii_case(region))
            && filter.os.is_none_or(|os| entry.os == os)
            && filter
                .runtime
                .is_none_or(|runtime| entry.runtime == runtime)
    });

    // Curation is applied after filtering, not before: a frontier computed
    // over every region and then narrowed to one would hide types that are
    // on the frontier *of that region*, which is the only frontier a user
    // choosing a region can act on.
    Ok(MachineCatalog {
        entries: curate(entries),
        pending_accounts,
    })
}

/// Whether a caller who names no machine wants interruptible capacity.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
pub struct DefaultMachineQuery {
    /// Whether to price and pick against spot capacity. Spot is the default
    /// because it is cheaper and flyco handles eviction.
    pub spot: Option<bool>,
    /// Answer for this linked account alone.
    ///
    /// What a compute card asks: "if this were the only account, what would
    /// flyco run on?" Absent, the answer is drawn from every linked account,
    /// which is what the composer's chip shows.
    pub account: Option<ProviderAccountId>,
}

/// Describes the machine flyco would provision if the caller named none.
///
/// The one honest way to show a user what "let flyco choose" means before
/// they commit to it: the same function `POST /v1/sessions` runs, answered
/// with the catalog entry behind it so the price and the size come from the
/// choice rather than from a second lookup that could disagree with it.
#[skyzen::openapi]
#[expect(
    clippy::too_many_arguments,
    reason = "previewing the default machine needs the catalog services, the \
              caller, and where the caller is"
)]
async fn get_default_machine(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(github): State<GithubClient>,
    Query(query): Query<DefaultMachineQuery>,
    caller: CallerLocation,
    db: Db,
    kv: Kv,
    queue: Queue,
) -> Outcome<Json<MachineDefault>> {
    automatic(
        &db,
        &config,
        &github,
        &kv,
        &queue,
        user.id,
        query.spot.unwrap_or(true),
        query.account,
        caller.0,
    )
    .await
    .map(Json)
    .into()
}

/// Picks the machine flyco provisions when the caller names none.
///
/// # Errors
///
/// Returns [`ApiError::CatalogNotReady`] while an account that could still
/// offer one has not been read, and
/// [`ApiError::NoDeployableLinuxMachine`] once every account has been read
/// and none offers a Linux type big enough for flyco to choose on its own.
/// The two are deliberately different: the first ends by itself in seconds,
/// and the second is a fact the user has to act on.
#[expect(
    clippy::too_many_arguments,
    reason = "choosing a machine reads the catalog, the queue, and the \
              caller's accounts"
)]
pub(crate) async fn automatic(
    db: &Db,
    config: &ApiConfig,
    github: &impl GithubOauth,
    kv: &Kv,
    queue: &Queue,
    user: UserId,
    spot: bool,
    account: Option<ProviderAccountId>,
    near: Option<RegionLocation>,
) -> Result<MachineDefault, ApiError> {
    let MachineCatalog {
        entries,
        pending_accounts,
    } = catalog(
        db,
        config,
        github,
        kv,
        Refresh::Ask(queue),
        user,
        &CatalogFilter {
            provider: None,
            account,
            region: None,
            os: Some(OsFamily::Linux),
            // Both, because `auto_linux_choice` is what chooses between
            // them: a container the provider gives away this month beats
            // every price a virtual machine can quote.
            runtime: None,
        },
    )
    .await?;
    let nothing_yet = || {
        if pending_accounts.is_empty() {
            nothing_big_enough()
        } else {
            ApiError::CatalogNotReady {
                accounts: pending_accounts.len(),
            }
        }
    };
    let entry = auto_linux_choice(&entries, spot, near).ok_or_else(nothing_yet)?;
    // `auto_linux_choice` only ever returns an entry with an account; the
    // read is written as a refusal rather than an unwrap so the invariant
    // is enforced here too, where it is used.
    let account = entry.account.ok_or_else(nothing_big_enough)?;
    Ok(MachineDefault {
        choice: MachineChoice {
            provider_account: account,
            machine_type: entry.machine_type.clone(),
            runtime: entry.runtime,
            region: entry.region.clone(),
            // Spot only where the catalog quoted it: the cheapest type may
            // be one the spot pool cannot fund, and asking the provider for
            // that is a machine that fails minutes later in the queue.
            spot: spot && entry.pricing.offers_spot(),
            disk_gib: DEFAULT_DISK_GIB,
        },
        entry: entry.clone(),
        pending_accounts,
    })
}

/// Resolves a session's chosen machine against the cached catalog.
///
/// What `POST /v1/sessions` checks before it reserves anything. The cached
/// document is the authority here — it is what the picker offered — and it
/// is read, never refreshed: asking the provider for a region in the
/// request is the twelve seconds the send button used to hang for.
///
/// The runtime is checked as well as the type, and separately, because the
/// two failures are different sentences. A type this account does not offer
/// here is a choice against a catalog it never had; a type it offers as
/// something other than what was asked for is a caller working from a
/// catalog that has since changed — and quietly provisioning the runtime on
/// offer would give a session that asked for a disk one that loses its
/// working tree every time the platform stops it.
///
/// # Errors
///
/// Returns [`ApiError::CatalogNotReady`] while the account has not been
/// read, [`ApiError::MachineUnavailable`] when the account's catalog does
/// not offer this type in this region, and
/// [`ApiError::MachineRuntimeMismatch`] when it offers it as a different
/// runtime from the one asked for.
pub(crate) async fn deployable(
    db: &Db,
    config: &ApiConfig,
    github: &impl GithubOauth,
    kv: &Kv,
    user: UserId,
    choice: &MachineChoice,
) -> Result<MachineCatalogEntry, ApiError> {
    let MachineCatalog {
        entries,
        pending_accounts,
    } = catalog(
        db,
        config,
        github,
        kv,
        Refresh::ReadOnly,
        user,
        &CatalogFilter {
            provider: None,
            account: Some(choice.provider_account),
            region: Some(choice.region.clone()),
            os: None,
            // Read whole and checked below, so a request naming the right
            // type and the wrong runtime is refused as the contradiction it
            // is rather than as a type this account does not offer.
            runtime: None,
        },
    )
    .await?;
    let entry = entries
        .into_iter()
        .find(|entry| entry.machine_type == choice.machine_type)
        .ok_or_else(|| {
            if pending_accounts.contains(&choice.provider_account) {
                ApiError::CatalogNotReady { accounts: 1 }
            } else {
                ApiError::MachineUnavailable(format!(
                    "{} in {} is not something this account can deploy",
                    choice.machine_type, choice.region
                ))
            }
        })?;
    if entry.runtime != choice.runtime {
        return Err(ApiError::MachineRuntimeMismatch {
            machine_type: choice.machine_type.clone(),
            requested: choice.runtime,
            offered: entry.runtime,
        });
    }
    Ok(entry)
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
#[expect(
    clippy::too_many_arguments,
    reason = "a resize names the session, the caller, the type, and every \
              service it touches"
)]
async fn resize_session_machine(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(github): State<GithubClient>,
    params: Params,
    Json(request): Json<ResizeMachine>,
    rooms: Rooms,
    hosts: HostRooms,
    db: Db,
    kv: Kv,
) -> Outcome<Accepted> {
    user_resize(
        &db, &config, &github, &kv, &rooms, &hosts, user.id, &params, &request,
    )
    .await
    .into()
}

#[expect(
    clippy::too_many_arguments,
    reason = "a resize names the session, the caller, the type, and every \
              service it touches"
)]
async fn user_resize(
    db: &Db,
    config: &ApiConfig,
    github: &impl GithubOauth,
    kv: &Kv,
    rooms: &Rooms,
    hosts: &HostRooms,
    user: UserId,
    params: &Params,
    request: &ResizeMachine,
) -> Result<Accepted, ApiError> {
    let session: SessionId = path_id(params, "id")?;
    resize(
        db,
        config,
        github,
        kv,
        rooms,
        hosts,
        user,
        session,
        &request.machine_type,
    )
    .await?;
    Ok(Accepted)
}

/// Deallocates a session's machine, keeping its disk.
///
/// The session stops costing compute and keeps everything on disk, which is
/// what makes a paused session cheap rather than lost.
#[skyzen::openapi]
async fn stop_session_machine(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(github): State<GithubClient>,
    params: Params,
    hosts: HostRooms,
    db: Db,
) -> Outcome<Accepted> {
    lifecycle(
        &db,
        &config,
        &github,
        &hosts,
        user.id,
        &params,
        provisioning::Operation::Stop,
    )
    .await
    .into()
}

/// Releases a session's compute and keeps its disk, on flyco's own say-so.
///
/// The same operation the user's Stop button performs, called by the sweep
/// that puts a session waiting out a spent plan window off its machine: the
/// wait can be days long and paying for idle compute for days is the whole
/// thing issue #244 exists to stop. It is scoped by owner like every other
/// lifecycle call — the credentials that act on a machine are the account
/// owner's — and the owner comes from the session row rather than from a
/// caller, because nobody is watching.
///
/// A machine that is already off is left alone rather than stopped again:
/// stopping is idempotent at the provider, but a second call spends tens of
/// seconds of a cron's budget saying nothing.
///
/// # Errors
///
/// Returns [`ApiError::MachineNotFound`] if the session has no machine row,
/// or [`ApiError::Provisioning`] if the provider or the host refuses.
pub(crate) async fn stop_for_flyco(
    db: &Db,
    config: &ApiConfig,
    github: &impl GithubOauth,
    hosts: &HostRooms,
    user: UserId,
    session: SessionId,
) -> Result<bool, ApiError> {
    let row = for_session(db, session)
        .await?
        .ok_or(ApiError::MachineNotFound)?;
    if row.state != MachineState::Running {
        return Ok(false);
    }
    run(
        db,
        config,
        github,
        hosts,
        user,
        session,
        provisioning::Operation::Stop,
    )
    .await?;
    Ok(true)
}

/// Runs one lifecycle operation for a route that names its session in a path.
async fn lifecycle(
    db: &Db,
    config: &ApiConfig,
    github: &impl GithubOauth,
    hosts: &HostRooms,
    user: UserId,
    params: &Params,
    operation: provisioning::Operation<'_>,
) -> Result<Accepted, ApiError> {
    let session: SessionId = path_id(params, "id")?;
    run(db, config, github, hosts, user, session, operation).await?;
    Ok(Accepted)
}

/// Brings a stopped session's machine back, on the same disk.
#[skyzen::openapi]
async fn start_session_machine(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(github): State<GithubClient>,
    params: Params,
    hosts: HostRooms,
    db: Db,
) -> Outcome<Accepted> {
    lifecycle(
        &db,
        &config,
        &github,
        &hosts,
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
         (id, session_id, provider_account_id, provider, machine_type, runtime, region, \
          disk_gib, requested_spot, spot, state, created_at_unix) \
         VALUES ({id}, {session}, {account}, {spec.provider}, {spec.machine_type.clone()}, \
                 {spec.runtime}, {spec.region.clone()}, {spec.disk_gib}, {spec.spot}, \
                 {spec.spot}, {provisioning}, {now})"
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
        "SELECT id, session_id, provider_account_id, provider, machine_type, runtime, region, \
         disk_gib, requested_spot, spot, state, hourly_micros, storage_hourly_micros, \
         vcpus, memory_mib, minimum_hours, minimum_charge_micros, native_id, volume_name, \
         address, created_at_unix \
         FROM machines WHERE session_id = {session}"
    )
    .fetch_optional()
    .await?)
}

/// Reads one machine row by its own id, without scoping it to an owner.
///
/// The host's read: a container job names the machine it acted on, and the
/// host presenting the result proved which host it is with its own token —
/// so the scope check is the account comparison the caller makes, not a
/// user id the machine does not have.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn find(db: &Db, machine: MachineId) -> Result<Option<MachineRow>, ApiError> {
    Ok(sql!(
        db,
        "SELECT id, session_id, provider_account_id, provider, machine_type, runtime, region, \
         disk_gib, requested_spot, spot, state, hourly_micros, storage_hourly_micros, \
         vcpus, memory_mib, minimum_hours, minimum_charge_micros, native_id, volume_name, \
         address, created_at_unix \
         FROM machines WHERE id = {machine}"
    )
    .fetch_optional()
    .await?)
}

/// The machine a codespace names, joined to the account it provisions
/// through — the bootstrap endpoint's whole read.
///
/// There is deliberately no user scoping on this query: the caller is not
/// a user, it is the codespace itself, and the join is what the endpoint's
/// own credential check is made against rather than a scope it could be
/// pre-applied.
#[derive(Debug, skyzen::FromRow)]
pub struct CodespaceRow {
    /// The machine row's own id.
    pub machine: MachineId,
    /// Who owns the account — the unseal path is scoped by it.
    pub user: UserId,
    /// Which linked account the machine provisions through.
    pub account: ProviderAccountId,
    /// The sealed daemon configuration this codespace is asking for.
    ///
    /// `NULL` while the provision that writes it is still running — the
    /// `409` the codespace's own retry is written for.
    pub bootstrap_enc: Option<String>,
}

/// Finds the machine a codespace calls itself by.
///
/// `native_id` is the codespace's generated name, which is what
/// `CODESPACE_NAME` reports inside it; the `provider` clause keeps the
/// lookup honest — a codespace name can only ever name a codespaces
/// machine, and matching another provider's row would serve one account's
/// configuration to a token scoped to another's repository.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn codespace(db: &Db, name: &str) -> Result<Option<CodespaceRow>, ApiError> {
    let provider = CloudProviderKind::Codespaces;
    Ok(sql!(
        db,
        "SELECT machines.id AS machine, provider_accounts.user_id AS user, \
         machines.provider_account_id AS account, machines.bootstrap_enc \
         FROM machines JOIN provider_accounts \
         ON provider_accounts.id = machines.provider_account_id \
         WHERE machines.native_id = {name} AND machines.provider = {provider}"
    )
    .fetch_optional()
    .await?)
}

/// Stores the configuration a codespace fetches on `postStart`, sealed
/// exactly as the account credentials beside it are.
///
/// Written by the provision that created the codespace, before the
/// provider call is even made: the machine may boot and ask before the
/// provisioning leg has recorded its name, and both halves of that race
/// are the codespace's own `404`/`409` retries.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn store_bootstrap(
    db: &Db,
    machine: MachineId,
    bootstrap_enc: &str,
) -> Result<(), ApiError> {
    sql!(
        db,
        "UPDATE machines SET bootstrap_enc = {bootstrap_enc} WHERE id = {machine}"
    )
    .execute()
    .await?;
    Ok(())
}

/// Every machine still holding resources on one linked account.
///
/// What a host removal counts before it refuses, and what it stops when the
/// caller insists.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn live_on_account(
    db: &Db,
    account: ProviderAccountId,
) -> Result<Vec<MachineRow>, ApiError> {
    let destroyed = MachineState::Destroyed;
    Ok(sql!(
        db,
        "SELECT id, session_id, provider_account_id, provider, machine_type, runtime, region, \
         disk_gib, requested_spot, spot, state, hourly_micros, storage_hourly_micros, \
         vcpus, memory_mib, minimum_hours, minimum_charge_micros, native_id, volume_name, \
         address, created_at_unix \
         FROM machines WHERE provider_account_id = {account} AND state != {destroyed} \
         ORDER BY created_at_unix"
    )
    .fetch_all()
    .await?)
}

/// Records the container and volume a host actually created.
///
/// The completion of a host provision, and the exact counterpart of
/// [`record`] for a cloud driver's answer: what the machine now *is* comes
/// from the machine rather than from what was asked for, because these two
/// names are what every later stop, start and removal has to address.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn record_container(
    db: &Db,
    machine: MachineId,
    container: &str,
    volume: &str,
) -> Result<(), ApiError> {
    let running = MachineState::Running;
    sql!(
        db,
        "UPDATE machines SET state = {running}, native_id = {container}, \
         volume_name = {volume}, stopping_since_unix = NULL, stopping_reason = NULL \
         WHERE id = {machine}"
    )
    .execute()
    .await?;
    Ok(())
}

/// Records that this machine's own daemon says it is stopping.
///
/// What `POST /v1/sessions/{id}/stopping` writes, and it is written *after*
/// the daemon has flushed the transcript and stored the working tree — so a
/// row carrying an instant here is one whose session is safe to start
/// somewhere else.
///
/// Not a [`MachineState`]: the machine has not stopped yet and may not stop
/// cleanly, and a fifth lifecycle state would have every reader of that
/// column learn a word for a moment rather than for a condition. The
/// container drivers of issue #235 read this to tell an execution that was
/// asked to go from one that died, and it is cleared the moment anything
/// acts on the machine again.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn mark_stopping(
    db: &Db,
    machine: MachineId,
    reason: StopReason,
) -> Result<(), ApiError> {
    let now = now_unix();
    sql!(
        db,
        "UPDATE machines SET stopping_since_unix = {now}, stopping_reason = {reason} \
         WHERE id = {machine}"
    )
    .execute()
    .await?;
    Ok(())
}

/// Records that a machine's compute is gone and its disk is not.
///
/// What draining a host leaves behind: the container is stopped and its
/// volume kept, which is exactly [`MachineState::Deallocated`].
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn deallocate(db: &Db, machine: MachineId) -> Result<(), ApiError> {
    let deallocated = MachineState::Deallocated;
    sql!(
        db,
        "UPDATE machines SET state = {deallocated}, compute_metered_at_unix = {now_unix()} \
         WHERE id = {machine}"
    )
    .execute()
    .await?;
    Ok(())
}

// ── What the Codespaces reconcile writes ──
//
// GitHub suspends a codespace on its own idle clock and deletes it on its
// own retention clock, and neither is delivered to anything flyco runs.
// The only truth about whether one still exists is `GET
// /user/codespaces/{name}`, so `crate::codespaces::reconcile` asks it of
// every machine below and these are the writes its answers produce.

/// A codespace flyco believes it is holding, joined to what the reconcile
/// needs to know about the session on it.
#[derive(Debug, skyzen::FromRow)]
pub struct HeldCodespace {
    /// The machine row's own id.
    pub machine: MachineId,
    /// The session it serves.
    pub session: SessionId,
    /// Who owns it — the linked account is unsealed under this id.
    pub user_id: UserId,
    /// Which linked account the machine provisions through.
    pub account: ProviderAccountId,
    /// The codespace's generated name — never `NULL` in this set, because a
    /// machine GitHub has not yet named has nothing to reconcile.
    pub native_id: String,
    /// What flyco last recorded of the machine's lifecycle.
    pub machine_state: MachineState,
    /// What the session on it is doing.
    pub session_state: flyco_core::SessionState,
    /// The codespace's geography, which its create request named it by.
    region: String,
    /// Whether it was provisioned on interruptible capacity. Codespaces has
    /// none, so this is always `false` — kept so the provider machine the
    /// row rebuilds is honest rather than derived.
    spot: bool,
    /// The codespace's `github.com/codespaces/{name}` URL.
    address: Option<String>,
}

impl HeldCodespace {
    /// This codespace as a driver takes it, for the delete a reconcile
    /// issues against one GitHub still holds in a dead state.
    #[must_use]
    pub fn as_provider_machine(&self) -> flyco_provider::Machine {
        flyco_provider::Machine {
            id: self.machine,
            native_id: self.native_id.clone(),
            runtime: Runtime::Vm,
            region: self.region.clone(),
            state: self.machine_state,
            capacity_mode: if self.spot {
                flyco_provider::CapacityMode::Spot
            } else {
                flyco_provider::CapacityMode::OnDemand
            },
            address: self.address.clone(),
        }
    }
}

/// Every codespace machine worth asking GitHub about, once per sweep.
///
/// Held means *exists and bills*: a machine still being built is the
/// provisioning queue's concern, a destroyed one is already released, and
/// a session in anything past `interrupted` has had its machine dealt with
/// by the path that ended it. A `paused` session's machine is selected
/// deliberately: its suspension changes the machine's billing either way.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn held_codespaces(db: &Db) -> Result<Vec<HeldCodespace>, ApiError> {
    let provider = CloudProviderKind::Codespaces;
    let running = MachineState::Running;
    let deallocated = MachineState::Deallocated;
    let active = flyco_core::SessionState::Active;
    let paused = flyco_core::SessionState::Paused;
    let interrupted = flyco_core::SessionState::Interrupted;
    Ok(sql!(
        db,
        "SELECT machines.id AS machine, machines.session_id AS session, \
         sessions.user_id, machines.provider_account_id AS account, \
         machines.native_id, machines.state AS machine_state, \
         sessions.state AS session_state, machines.region, machines.spot, \
         machines.address \
         FROM machines JOIN sessions ON sessions.id = machines.session_id \
         WHERE machines.provider = {provider} AND machines.native_id IS NOT NULL \
         AND (machines.state = {running} OR machines.state = {deallocated}) \
         AND (sessions.state = {active} OR sessions.state = {paused} \
              OR sessions.state = {interrupted})"
    )
    .fetch_all()
    .await?)
}

/// Records that the provider suspended a machine it was holding.
///
/// The reconcile's write for a codespace GitHub stopped on its own: the
/// compute meter ends here — GitHub stopped billing when it suspended, not
/// when flyco noticed, so what little gap remains is the floor the
/// one-minute windows already truncate — and the storage meter is left
/// alone, because the disk is kept and billed either way. Guarded on
/// `running` so a machine already off compute is not suspended a second
/// time, and the boolean is how the caller tells "learned" from "already
/// knew".
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn suspended(db: &Db, machine: MachineId) -> Result<bool, ApiError> {
    let running = MachineState::Running;
    let deallocated = MachineState::Deallocated;
    let written = sql!(
        db,
        "UPDATE machines SET state = {deallocated}, \
         compute_metered_at_unix = {now_unix()} \
         WHERE id = {machine} AND state = {running}"
    )
    .execute()
    .await?;
    Ok(written.rows_written > 0)
}

/// Records that a machine off compute is running again.
///
/// The reconcile's write for a suspended codespace somebody started
/// themselves — through github.com, say, where a session's codespace is one
/// click. The compute meter restarts from the instant the run is learned,
/// which under-bills by at most a sweep's worth of minutes, and the
/// session's own state is *not* moved here: only the daemon's attach
/// proves the machine is serving, so the session's move back is
/// [`sessions::daemon_arrived`]'s.
///
/// Guarded on `deallocated` so a stale "running" answer cannot un-destroy
/// a machine, and the boolean is how the caller tells whether anything
/// changed.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn note_running(db: &Db, machine: MachineId) -> Result<bool, ApiError> {
    let running = MachineState::Running;
    let deallocated = MachineState::Deallocated;
    let now = now_unix();
    let written = sql!(
        db,
        "UPDATE machines SET state = {running}, \
         compute_meter_started_at_unix = {now}, compute_metered_at_unix = {now} \
         WHERE id = {machine} AND state = {deallocated}"
    )
    .execute()
    .await?;
    Ok(written.rows_written > 0)
}

/// Records that a machine's provider-side resource is gone for good.
///
/// [`destroy_for_archive`]'s write without its provider call, which is the
/// whole of the difference: the resource this row named is already gone —
/// deleted past its retention, failed past starting, or left behind on an
/// account flyco can no longer unseal — and asking for it again is how that
/// was learned. Everything billable is released with it, because nothing is
/// left to bill against, and the provider-native names are cleared so a
/// resume reads `native_id IS NULL` and provisions rather than starting a
/// name that answers 404.
///
/// Guarded on not-`destroyed` so a redelivery cannot clear a machine that
/// was rebuilt in the meantime.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn mark_lost(db: &Db, machine: MachineId) -> Result<(), ApiError> {
    let destroyed = MachineState::Destroyed;
    sql!(
        db,
        "UPDATE machines SET state = {destroyed}, hourly_micros = NULL, \
         storage_hourly_micros = NULL, native_id = NULL, \
         address = NULL, bootstrap_enc = NULL \
         WHERE id = {machine} AND state != {destroyed}"
    )
    .execute()
    .await?;
    Ok(())
}

/// Records what the provider has created of a machine it has not finished
/// building: the id the control plane can destroy it by, and nothing else.
///
/// The row stays `provisioning`, so the session is still waiting for a
/// machine, the stall sweep still counts it, and the queue knows the build
/// is owned by a continuation rather than open for another attempt.
///
/// The write is skipped on a row already running: a redelivered leg hands
/// back `Pending` while its sibling's `Ready` has already landed, and
/// writing the pending row's shorter native id over the resolved one
/// would leave a running machine naming only its job until the next leg
/// rewrote it.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn record_pending(db: &Db, machine: &flyco_provider::Machine) -> Result<(), ApiError> {
    let running = MachineState::Running;
    sql!(
        db,
        "UPDATE machines SET native_id = {machine.native_id.clone()} \
         WHERE id = {machine.id} AND state != {running}"
    )
    .execute()
    .await?;
    Ok(())
}

/// Records what the provider actually built.
///
/// `spot` and every number in `built` come from the machine that exists
/// rather than from the request that asked for it: a spot request a provider
/// cannot honour is answered with on-demand capacity, and both the price
/// billed and the size the agent is told about follow what was obtained.
///
/// `built.machine_type` is not written back. The type is this row's
/// identity — the job claimed it and every provider-native resource name is
/// derived from it — and rewriting an identity from a second source is how
/// two attempts end up naming different machines.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn record(
    db: &Db,
    machine: &flyco_provider::Machine,
    built: &SessionMachine,
    storage_hourly: Option<Usd>,
) -> Result<(), ApiError> {
    let spot = machine.capacity_mode.is_spot();
    let now = now_unix();
    let vcpus = built.capacity.as_ref().map(|capacity| capacity.vcpus);
    let memory_mib = built.capacity.as_ref().map(|capacity| capacity.memory_mib);
    let minimum_hours = built.minimum.map(|minimum| minimum.hours);
    let minimum_charge = built.minimum.map(|minimum| minimum.charge);
    sql!(
        db,
        "UPDATE machines SET state = {machine.state}, spot = {spot}, \
         hourly_micros = {built.hourly}, storage_hourly_micros = {storage_hourly}, \
         vcpus = {vcpus}, memory_mib = {memory_mib}, \
         minimum_hours = {minimum_hours}, minimum_charge_micros = {minimum_charge}, \
         compute_meter_started_at_unix = {now}, compute_metered_at_unix = {now}, \
         storage_meter_started_at_unix = {now}, storage_metered_at_unix = {now}, \
         native_id = {machine.native_id.clone()}, address = {machine.address.clone()} \
         WHERE id = {machine.id}"
    )
    .execute()
    .await?;
    Ok(())
}

/// Starts a machine a provider stopped, on the disk it kept.
///
/// The queue's own lifecycle call, and unlike [`run`] it is not scoped to a
/// user: a recovery is performed by a consumer nobody is watching, on a
/// session whose ownership was established when the job was enqueued.
///
/// Nothing about the machine's identity is rewritten — not the type, not
/// the disk, not the provider-native names — because none of it changed.
/// What is written back is what a start actually decides: the state, and
/// the address, which several providers hand out afresh because a stopped
/// instance releases its ephemeral one. The compute meter restarts from
/// here, and the storage meter is deliberately left alone: the disk was
/// kept and therefore billed throughout, which is exactly what the user is
/// paying for while a session is off its machine.
///
/// # Errors
///
/// Returns [`ApiError::MachineNotReady`] if the row names no
/// provider-native machine to start, or [`ApiError`] if the provider
/// refuses or the write fails.
pub async fn restart(
    db: &Db,
    provisioner: &mut impl provisioning::Provisioner,
    account: &provisioning::LinkedAccount,
    row: &MachineRow,
) -> Result<flyco_provider::Machine, flyco_provider::ProviderError> {
    let machine = row.as_provider_machine().map_err(|_| {
        flyco_provider::ProviderError::Malformed(
            "this machine has no provider-native name to start",
        )
    })?;
    let started = provisioner.restart(account, &machine).await?;

    let now = now_unix();
    sql!(
        db,
        "UPDATE machines SET state = {started.state}, native_id = {started.native_id.clone()}, \
         address = {started.address.clone()}, compute_meter_started_at_unix = {now}, \
         compute_metered_at_unix = {now} WHERE id = {row.id}"
    )
    .execute()
    .await
    .map_err(|error| {
        flyco_provider::ProviderError::Rejected(format!(
            "the restarted machine could not be recorded: {error}"
        ))
    })?;

    Ok(started)
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
        "UPDATE machines SET state = {provisioning}, bootstrap_enc = NULL WHERE id = {row.id}"
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

#[cfg(test)]
mod tests {
    use flyco_core::{
        BillingMinimum, CloudProviderKind, MachineCapacity, MachineCatalogEntry, MachinePricing,
        OsFamily, Runtime, StoragePricing, Usd,
    };

    use super::on_the_agents_authority;
    use crate::error::ApiError;

    fn entry(machine_type: &str, minimum: Option<BillingMinimum>) -> MachineCatalogEntry {
        MachineCatalogEntry {
            provider: CloudProviderKind::Aws,
            account: None,
            region: "us-east-1".to_owned(),
            location: None,
            machine_type: machine_type.to_owned(),
            runtime: Runtime::Vm,
            free_grant: None,
            os: OsFamily::Linux,
            capacity: Some(MachineCapacity {
                vcpus: 8,
                memory_mib: 32 * 1024,
            }),
            lineage: None,
            pricing: MachinePricing::Metered {
                on_demand_hourly: Usd::from_cents(65),
                spot_hourly: None,
                minimum,
                storage: StoragePricing::PerGibHourly {
                    rate: Usd::from_micros(100),
                },
            },
        }
    }

    #[test]
    fn an_agent_moves_to_an_ordinary_type_on_its_own() {
        assert!(on_the_agents_authority(&entry("m7i.2xlarge", None)).is_ok());
    }

    #[test]
    fn an_agent_may_not_commit_the_user_to_a_minimum_charge() {
        // The refusal names the hours rather than the money, because the
        // hours are what the user is being asked to agree to; the charge is
        // quoted on the approval card the daemon raises instead.
        let refused = on_the_agents_authority(&entry(
            "mac2.metal",
            Some(BillingMinimum::new(24, Usd::from_cents(65))),
        ))
        .expect_err("a license-bound type is the user's decision");

        assert!(matches!(
            refused,
            ApiError::LicenseBoundResizeNeedsApproval {
                ref machine_type,
                hours: 24,
            } if machine_type == "mac2.metal"
        ));
    }

    #[test]
    fn hardware_the_user_owns_carries_no_minimum_to_gate_on() {
        let owned = MachineCatalogEntry {
            pricing: MachinePricing::UserOwned,
            ..entry("build.lexo.cool", None)
        };
        assert!(on_the_agents_authority(&owned).is_ok());
    }
}
