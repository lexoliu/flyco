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
    MachineState, MachineView, OsFamily, ProviderAccountId, ResizeMachine, SessionId,
    SessionMachine, Usd, UserId, auto_linux_choice, curate,
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
use crate::extract::path_id;
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
    hosts: &HostRooms,
    user: UserId,
    session: SessionId,
    operation: provisioning::Operation<'_>,
) -> Result<flyco_provider::Machine, ApiError> {
    let row = load(db, user, session).await?;
    let machine = row.as_provider_machine()?;
    let account = provisioning::account(db, config, user, row.provider_account_id).await?;

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
        "UPDATE machines SET state = {updated.state}, machine_type = {machine_type}, \
         spot = {updated.capacity_mode.is_spot()}, native_id = {updated.native_id.clone()}, \
         address = {updated.address.clone()}, compute_metered_at_unix = {now_unix()} \
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
/// filter rather than a preference. What comes back is the curated list of
/// docs/ux.md §7.6, the same one the user's slider and the agent's
/// `machine_resize` tool read.
fn resize_filter(row: &MachineRow) -> CatalogFilter {
    CatalogFilter {
        provider: Some(row.provider),
        account: Some(row.provider_account_id),
        region: Some(row.region.clone()),
        os: None,
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
    kv: &Kv,
    user: UserId,
    session: SessionId,
) -> Result<Vec<MachineCatalogEntry>, ApiError> {
    let row = load(db, user, session).await?;
    Ok(catalog(
        db,
        config,
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
    kv: &Kv,
    user: UserId,
    row: &MachineRow,
    machine_type: &str,
) -> Result<MachineCatalogEntry, ApiError> {
    catalog(db, config, kv, Refresh::ReadOnly, user, &resize_filter(row))
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
    kv: &Kv,
    rooms: &Rooms,
    hosts: &HostRooms,
    user: UserId,
    session: SessionId,
    machine_type: &str,
) -> Result<(), ApiError> {
    let row = load(db, user, session).await?;
    let entry = offered(db, config, kv, user, &row, machine_type).await?;
    apply(db, config, rooms, hosts, user, session, &row, &entry).await
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
    kv: &Kv,
    rooms: &Rooms,
    hosts: &HostRooms,
    user: UserId,
    session: SessionId,
    machine_type: &str,
) -> Result<(), ApiError> {
    let row = load(db, user, session).await?;
    let entry = offered(db, config, kv, user, &row, machine_type).await?;
    on_the_agents_authority(&entry)?;
    apply(db, config, rooms, hosts, user, session, &row, &entry).await
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

    announce_machine_change(rooms, session, &built).await;
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
async fn announce_machine_change(rooms: &Rooms, session: SessionId, built: &SessionMachine) {
    let event = ClientEvent::MachineChanged {
        machine_type: built.machine_type.clone(),
        hourly: built.hourly,
        spot: built.spot,
        restarted: true,
    };
    if let Err(error) = rooms.broadcast(session, &event).await {
        tracing::warn!(%session, %error, "a machine change did not reach the session's watchers");
    }

    let command = ControlToDaemon::MachineChanged {
        machine_type: built.machine_type.clone(),
        hourly: built.hourly,
        spot: built.spot,
        restarted: true,
    };
    if let Err(error) = rooms.command(session, &command).await {
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
        let account = provisioning::account(db, config, user, row.provider_account_id).await?;
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
}

/// Lists the machine types the caller can provision, with their prices.
#[skyzen::openapi]
async fn get_catalog(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    Query(filter): Query<CatalogFilter>,
    db: Db,
    kv: Kv,
    queue: Queue,
) -> Outcome<Json<MachineCatalog>> {
    catalog(&db, &config, &kv, Refresh::Ask(&queue), user.id, &filter)
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
    kv: &Kv,
    refresh: Refresh<'_>,
    user: UserId,
    filter: &CatalogFilter,
) -> Result<MachineCatalog, ApiError> {
    let mut accounts = provisioning::accounts_for(db, config, user, filter.provider).await?;
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
async fn get_default_machine(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    Query(query): Query<DefaultMachineQuery>,
    db: Db,
    kv: Kv,
    queue: Queue,
) -> Outcome<Json<MachineDefault>> {
    automatic(
        &db,
        &config,
        &kv,
        &queue,
        user.id,
        query.spot.unwrap_or(true),
        query.account,
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
pub(crate) async fn automatic(
    db: &Db,
    config: &ApiConfig,
    kv: &Kv,
    queue: &Queue,
    user: UserId,
    spot: bool,
    account: Option<ProviderAccountId>,
) -> Result<MachineDefault, ApiError> {
    let MachineCatalog {
        entries,
        pending_accounts,
    } = catalog(
        db,
        config,
        kv,
        Refresh::Ask(queue),
        user,
        &CatalogFilter {
            provider: None,
            account,
            region: None,
            os: Some(OsFamily::Linux),
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
    let entry = auto_linux_choice(&entries, spot).ok_or_else(nothing_yet)?;
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
        pending_accounts,
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
#[expect(
    clippy::too_many_arguments,
    reason = "a resize names the session, the caller, the type, and every \
              service it touches"
)]
async fn resize_session_machine(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    params: Params,
    Json(request): Json<ResizeMachine>,
    rooms: Rooms,
    hosts: HostRooms,
    db: Db,
    kv: Kv,
) -> Outcome<Accepted> {
    user_resize(
        &db, &config, &kv, &rooms, &hosts, user.id, &params, &request,
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
    params: Params,
    hosts: HostRooms,
    db: Db,
) -> Outcome<Accepted> {
    lifecycle(
        &db,
        &config,
        &hosts,
        user.id,
        &params,
        provisioning::Operation::Stop,
    )
    .await
    .into()
}

/// Runs one lifecycle operation for a route that names its session in a path.
async fn lifecycle(
    db: &Db,
    config: &ApiConfig,
    hosts: &HostRooms,
    user: UserId,
    params: &Params,
    operation: provisioning::Operation<'_>,
) -> Result<Accepted, ApiError> {
    let session: SessionId = path_id(params, "id")?;
    run(db, config, hosts, user, session, operation).await?;
    Ok(Accepted)
}

/// Brings a stopped session's machine back, on the same disk.
#[skyzen::openapi]
async fn start_session_machine(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    params: Params,
    hosts: HostRooms,
    db: Db,
) -> Outcome<Accepted> {
    lifecycle(
        &db,
        &config,
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
        "SELECT id, session_id, provider_account_id, provider, machine_type, region, \
         disk_gib, requested_spot, spot, state, hourly_micros, storage_hourly_micros, \
         vcpus, memory_mib, minimum_hours, minimum_charge_micros, native_id, volume_name, \
         address, created_at_unix \
         FROM machines WHERE id = {machine}"
    )
    .fetch_optional()
    .await?)
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
        "SELECT id, session_id, provider_account_id, provider, machine_type, region, \
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
         volume_name = {volume} WHERE id = {machine}"
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

#[cfg(test)]
mod tests {
    use flyco_core::{
        BillingMinimum, CloudProviderKind, MachineCapacity, MachineCatalogEntry, MachinePricing,
        OsFamily, StoragePricing, Usd,
    };

    use super::on_the_agents_authority;
    use crate::error::ApiError;

    fn entry(machine_type: &str, minimum: Option<BillingMinimum>) -> MachineCatalogEntry {
        MachineCatalogEntry {
            provider: CloudProviderKind::Aws,
            account: None,
            region: "us-east-1".to_owned(),
            machine_type: machine_type.to_owned(),
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
