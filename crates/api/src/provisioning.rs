//! Turning a stored provider account into a live driver.
//!
//! Credentials are sealed at rest, so every provisioning path starts here:
//! unseal exactly one account's credentials, build the concrete driver they
//! open, and hand it back. The [`CloudProvider`](flyco_provider::CloudProvider)
//! trait is deliberately not object-safe — it keeps every future free of
//! boxing on wasm32 — so the dispatch is a `match` on the credential variant
//! rather than a trait object, and each operation names the shapes it
//! supports.

use flyco_core::{
    CloudProviderKind, CloudSpend, HostFacts, HostId, HostState, MachineCatalogEntry, MachineSpec,
    ProviderAccountId, ProviderCredentials, UserId,
};
use flyco_provider::aws::sigv4::AccessKey;
use flyco_provider::aws::{AwsProvider, AwsWorkspace};
use flyco_provider::azure::auth::ServicePrincipal;
use flyco_provider::azure::{AzureProvider, Workspace};
use flyco_provider::gcp::auth::ServiceAccountKey;
use flyco_provider::gcp::{GcpProvider, GcpWorkspace};
use flyco_provider::host::{ContainerJob, ControlToHost, Host};
use flyco_provider::{CloudProvider, Machine, ProviderError, ProvisionRequest};
use skyzen::sql;
use skyzen_services::Db;

use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::rooms::HostRooms;

/// One linked account, with its credentials unsealed for immediate use.
///
/// Held only for the length of an operation. Nothing here is serialized,
/// logged, or returned: `Debug` is written by hand for the same reason the
/// credentials themselves are.
pub struct LinkedAccount {
    /// Which account this is, for correlating a failure with a row.
    pub id: ProviderAccountId,
    /// The unsealed credentials.
    credentials: ProviderCredentials,
    /// The Azure resource group flyco created in this subscription when the
    /// account was linked, and `None` for every other provider.
    ///
    /// Beside the credentials rather than inside them: it is a resource
    /// flyco owns, not a secret the user typed, and reading it needs no
    /// unsealing. An Azure account without one is a row no driver can be
    /// built from — see [`LinkedAccount::azure_workspace`].
    resource_group: Option<String>,
    /// The enrolled machine this account provisions onto, for a host
    /// account.
    ///
    /// Read in the same query as the account rather than fetched per use:
    /// the credential names the host, but it is sealed and no `SELECT` can
    /// see inside it, so `provider_accounts.host_id` is the join key and
    /// this is what it joined to.
    host: Option<HostSnapshot>,
}

/// The enrolled machine behind a host account, as one query read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSnapshot {
    /// Which machine it is.
    pub id: HostId,
    /// Where the control plane last recorded it in its life.
    pub state: HostState,
    /// What it last said about itself.
    pub facts: HostFacts,
}

impl core::fmt::Debug for LinkedAccount {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LinkedAccount")
            .field("id", &self.id)
            .field("kind", &self.credentials.kind())
            .finish_non_exhaustive()
    }
}

impl LinkedAccount {
    /// Which provider this account opens.
    #[must_use]
    pub const fn kind(&self) -> CloudProviderKind {
        self.credentials.kind()
    }

    /// The unsealed credentials, for a driver that is about to use them.
    ///
    /// Public because [`Provisioner`] is a trait: an implementation outside
    /// this module still has to open the account it was handed. It is the
    /// same borrow the dispatch below takes, and it is still true that
    /// nothing serializes, logs, or returns what it points at.
    #[must_use]
    pub const fn credentials(&self) -> &ProviderCredentials {
        &self.credentials
    }

    /// The enrolled machine behind this account, if it is a host account.
    #[must_use]
    pub const fn host(&self) -> Option<&HostSnapshot> {
        self.host.as_ref()
    }

    /// The machine this account can provision onto *right now*.
    ///
    /// A host that is offline, draining or removed offers nothing: flyco
    /// cannot reach it, and a catalog entry for it would be a machine the
    /// user could pick and never get.
    #[must_use]
    pub fn schedulable_host(&self) -> Option<&HostSnapshot> {
        self.host
            .as_ref()
            .filter(|host| host.state.is_schedulable())
    }

    /// The planner for this account's machine, when it is a host account.
    #[must_use]
    pub fn host_planner(&self) -> Option<Host> {
        self.host
            .as_ref()
            .map(|host| Host::new(host.id, host.facts.clone()))
    }

    /// The resource group an Azure driver for this account must use.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Malformed`] for an Azure row with no group
    /// recorded, which is an account linked before flyco created the group
    /// itself. Guessing a name here would point the driver at whatever
    /// resources happen to answer to it in somebody's subscription, so the
    /// account is refused and relinked instead.
    fn azure_workspace(&self) -> Result<&str, ProviderError> {
        self.resource_group
            .as_deref()
            .ok_or(ProviderError::Malformed(
                "this Azure account has no resource group recorded; relink it",
            ))
    }
}

/// The Azure driver one set of Azure credentials opens.
///
/// Every dispatch below needs the same construction, so it is written once:
/// a credential field added or renamed is one edit rather than three that
/// can drift apart.
pub(crate) fn azure_driver(
    tenant_id: &str,
    client_id: &str,
    client_secret: &str,
    subscription_id: &str,
    resource_group: &str,
    admin_ssh_public_key: &str,
) -> AzureProvider {
    AzureProvider::new(
        ServicePrincipal {
            tenant_id: tenant_id.to_owned(),
            client_id: client_id.to_owned(),
            client_secret: client_secret.to_owned(),
            subscription_id: subscription_id.to_owned(),
        },
        Workspace::new(resource_group, admin_ssh_public_key),
    )
}

/// The AWS driver one access key opens.
pub(crate) fn aws_driver(
    access_key_id: &str,
    secret_access_key: &str,
    session_token: Option<&str>,
    key_name: Option<&str>,
) -> AwsProvider {
    let mut key = AccessKey::new(access_key_id, secret_access_key);
    if let Some(token) = session_token {
        key = key.with_session_token(token);
    }

    let mut workspace = AwsWorkspace::new();
    if let Some(key_name) = key_name {
        workspace = workspace.with_key_pair(key_name);
    }
    AwsProvider::new(key, workspace)
}

/// The GCP driver one service-account key opens.
///
/// Fallible where the other two are not: a Google credential is a *document*
/// rather than a set of fields, and one that is not a service-account key is
/// a credential to fix rather than a failure to retry.
///
/// # Errors
///
/// Returns [`ProviderError::Malformed`] if the document is not a
/// service-account key.
pub(crate) fn gcp_driver(service_account_json: &str) -> Result<GcpProvider, ProviderError> {
    Ok(GcpProvider::new(
        ServiceAccountKey::parse(service_account_json)?,
        GcpWorkspace::new(),
    ))
}

#[derive(Debug, skyzen::FromRow)]
struct SealedRow {
    id: ProviderAccountId,
    credentials_enc: String,
    resource_group: Option<String>,
    host_id: Option<HostId>,
    host_state: Option<HostState>,
    #[row(json)]
    host_facts: Option<HostFacts>,
}

impl SealedRow {
    /// The enrolled machine this row joined to, when it is a host account.
    ///
    /// All three halves or none: a host account whose row is half-written is
    /// a row enrollment could not have produced, and treating it as a cloud
    /// account would provision nothing anywhere.
    fn host(&self) -> Option<HostSnapshot> {
        Some(HostSnapshot {
            id: self.host_id?,
            state: self.host_state?,
            facts: self.host_facts.clone()?,
        })
    }
}

/// Loads the caller's linked accounts, optionally narrowed to one provider.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails, or if a stored credential
/// cannot be unsealed or decoded — which means the row was written by a
/// different key and is not something to paper over.
pub async fn accounts_for(
    db: &Db,
    config: &ApiConfig,
    user: UserId,
    kind: Option<CloudProviderKind>,
) -> Result<Vec<LinkedAccount>, ApiError> {
    // A NULL filter matches every provider, which keeps one statement for
    // both callers rather than assembling SQL per request.
    // A drained host is skipped rather than returned: its account row
    // survives so the machines that ran there still name something, but it
    // can offer no catalog and meter nothing.
    let removed = HostState::Removed;
    let rows: Vec<SealedRow> = sql!(
        db,
        "SELECT provider_accounts.id, credentials_enc, resource_group, host_id, \
         hosts.state AS host_state, hosts.facts AS host_facts \
         FROM provider_accounts LEFT JOIN hosts ON hosts.id = provider_accounts.host_id \
         WHERE provider_accounts.user_id = {user} AND ({kind} IS NULL OR kind = {kind}) \
         AND (host_id IS NULL OR hosts.state != {removed}) \
         ORDER BY linked_at_unix"
    )
    .fetch_all()
    .await?;

    let cipher = config.token_cipher();
    rows.into_iter()
        .map(|row| {
            let credentials: ProviderCredentials =
                serde_json::from_str(&cipher.open(&row.credentials_enc)?).map_err(|_| {
                    ApiError::CorruptRecord("provider_accounts.credentials_enc is not credentials")
                })?;
            Ok(LinkedAccount {
                id: row.id,
                credentials,
                host: row.host(),
                resource_group: row.resource_group,
            })
        })
        .collect()
}

/// Loads exactly one of the caller's accounts.
///
/// # Errors
///
/// Returns [`ApiError::ProviderAccountNotFound`] when the account does not
/// exist or belongs to somebody else — the two are indistinguishable.
pub async fn account(
    db: &Db,
    config: &ApiConfig,
    user: UserId,
    id: ProviderAccountId,
) -> Result<LinkedAccount, ApiError> {
    let row: SealedRow = sql!(
        db,
        "SELECT provider_accounts.id, credentials_enc, resource_group, host_id, \
         hosts.state AS host_state, hosts.facts AS host_facts \
         FROM provider_accounts LEFT JOIN hosts ON hosts.id = provider_accounts.host_id \
         WHERE provider_accounts.id = {id} AND provider_accounts.user_id = {user}"
    )
    .fetch_optional()
    .await?
    .ok_or(ApiError::ProviderAccountNotFound)?;

    let credentials: ProviderCredentials =
        serde_json::from_str(&config.token_cipher().open(&row.credentials_enc)?).map_err(|_| {
            ApiError::CorruptRecord("provider_accounts.credentials_enc is not credentials")
        })?;

    Ok(LinkedAccount {
        id,
        credentials,
        host: row.host(),
        resource_group: row.resource_group,
    })
}

/// Reads what an account can actually deploy today.
///
/// For every cloud provider that is the intersection of the independent
/// gates its API exposes — availability, quota, and whatever the account
/// itself forbids — so an entry that comes back is one a provision request
/// will be allowed to create. A registered SSH host reports the single
/// machine it is, unpriced: the user already owns it.
///
/// # Errors
///
/// Returns [`ProviderError`] if the provider rejects the credentials or
/// cannot be reached, and for providers flyco has no driver for yet.
pub async fn catalog(account: &LinkedAccount) -> Result<Vec<MachineCatalogEntry>, ProviderError> {
    let mut entries = catalog_of(account).await?;
    // A driver holds credentials, not the row they came from, so the
    // account is stamped here — otherwise a choice from the merged list
    // could not name the account it must be provisioned through.
    for entry in &mut entries {
        entry.account = Some(account.id);
    }
    Ok(entries)
}

async fn catalog_of(account: &LinkedAccount) -> Result<Vec<MachineCatalogEntry>, ProviderError> {
    match &account.credentials {
        ProviderCredentials::Azure {
            tenant_id,
            client_id,
            client_secret,
            subscription_id,
            admin_ssh_public_key,
        } => {
            azure_driver(
                tenant_id,
                client_id,
                client_secret,
                subscription_id,
                account.azure_workspace()?,
                admin_ssh_public_key,
            )
            .catalog()
            .await
        }
        // One entry per *online* machine: an offline host is not a
        // cheaper machine, it is one that cannot be started, and offering it
        // would put a choice on the slider that fails the moment it is
        // taken.
        ProviderCredentials::Host { .. } => Ok(account
            .schedulable_host()
            .map(|host| Host::new(host.id, host.facts.clone()).catalog())
            .unwrap_or_default()),
        ProviderCredentials::Aws {
            access_key_id,
            secret_access_key,
            session_token,
            key_name,
        } => {
            aws_driver(
                access_key_id,
                secret_access_key,
                session_token.as_deref(),
                key_name.as_deref(),
            )
            .catalog()
            .await
        }
        ProviderCredentials::Gcp {
            service_account_json,
        } => gcp_driver(service_account_json)?.catalog().await,
    }
}

/// Reads what the provider's own meter says this account has been billed
/// over its current billing period.
///
/// `None` is not a failure and not a zero: it means flyco has no metered
/// number to report, for one of two reasons. A registered SSH host is
/// hardware the user already owns and already pays for, so there is nothing
/// to meter; a GCP project has plenty to meter and no API that will say so,
/// because Google delivers billing data through a `BigQuery` export the user
/// configures rather than through a running total a service account can
/// read. Either way the answer is silence rather than a `$0.00` that would
/// read as "this costs nothing".
///
/// Exposed here for the same reason [`catalog`] is: the driver trait is not
/// object-safe, so the dispatch on the credential variant lives in the
/// control plane and each arm names what it supports.
///
/// # Errors
///
/// Returns [`ProviderError`] if the provider refuses the read or cannot be
/// reached, and for providers flyco has no driver for yet.
pub async fn cloud_usage(
    account: &LinkedAccount,
    now_unix: u64,
) -> Result<Option<CloudSpend>, ProviderError> {
    match &account.credentials {
        ProviderCredentials::Azure {
            tenant_id,
            client_id,
            client_secret,
            subscription_id,
            admin_ssh_public_key,
        } => azure_driver(
            tenant_id,
            client_id,
            client_secret,
            subscription_id,
            account.azure_workspace()?,
            admin_ssh_public_key,
        )
        .billing_period_cost(now_unix)
        .await
        .map(Some),
        ProviderCredentials::Aws {
            access_key_id,
            secret_access_key,
            session_token,
            key_name,
        } => aws_driver(
            access_key_id,
            secret_access_key,
            session_token.as_deref(),
            key_name.as_deref(),
        )
        .billing_period_cost(now_unix)
        .await
        .map(Some),
        // Two providers contribute no row, for the two different reasons
        // this function's documentation gives: a machine the user owns has
        // nothing flyco meters, and a GCP project has plenty and no API that
        // will say so. The answer is the same because the honest answer to
        // "how much has this cost" is silence in both cases.
        ProviderCredentials::Host { .. } | ProviderCredentials::Gcp { .. } => Ok(None),
    }
}

/// One lifecycle operation against an already-provisioned machine.
///
/// Each arm dispatches on the credential variant for the same reason
/// [`catalog`] does: the driver trait is not object-safe, deliberately, and
/// what an operation *means* is written once in [`Operation::run`] rather
/// than once per provider. Every cloud driver implements these; a registered
/// SSH host does not, because it cannot be resized and its container
/// lifecycle is executed natively rather than from the Worker.
///
/// # Errors
///
/// Returns [`ProviderError`] if the provider refuses the operation or
/// cannot be reached.
pub async fn operate(
    hosts: &HostRooms,
    account: &LinkedAccount,
    machine: &flyco_provider::Machine,
    operation: Operation<'_>,
) -> Result<flyco_provider::Machine, ProviderError> {
    match &account.credentials {
        ProviderCredentials::Azure {
            tenant_id,
            client_id,
            client_secret,
            subscription_id,
            admin_ssh_public_key,
        } => {
            operation
                .run(
                    &mut azure_driver(
                        tenant_id,
                        client_id,
                        client_secret,
                        subscription_id,
                        account.azure_workspace()?,
                        admin_ssh_public_key,
                    ),
                    machine,
                )
                .await
        }
        ProviderCredentials::Host { .. } => run_on_host(hosts, account, machine, operation).await,
        ProviderCredentials::Aws {
            access_key_id,
            secret_access_key,
            session_token,
            key_name,
        } => {
            operation
                .run(
                    &mut aws_driver(
                        access_key_id,
                        secret_access_key,
                        session_token.as_deref(),
                        key_name.as_deref(),
                    ),
                    machine,
                )
                .await
        }
        ProviderCredentials::Gcp {
            service_account_json,
        } => {
            operation
                .run(&mut gcp_driver(service_account_json)?, machine)
                .await
        }
    }
}

/// Performs one lifecycle operation on a machine the user owns.
///
/// The machine is a container, so the operation is a container job posted
/// down the host's own socket — flyco never reaches the machine any other
/// way. What comes back is what the row must say *now*: the host answers
/// asynchronously with a `JobResult`, and a job that fails there fails the
/// session with what Podman said (see [`crate::hosts::record_job_result`]).
///
/// A resize is refused rather than attempted, exactly as the planner refuses
/// it: the machine has the cores it has.
async fn run_on_host(
    hosts: &HostRooms,
    account: &LinkedAccount,
    machine: &flyco_provider::Machine,
    operation: Operation<'_>,
) -> Result<flyco_provider::Machine, ProviderError> {
    let planner = account.host_planner().ok_or(ProviderError::Malformed(
        "this host account names no enrolled machine; enroll it again",
    ))?;
    let job = planner.plan(&operation.on(machine))?;
    post(hosts, planner.id(), &job).await?;

    let mut updated = machine.clone();
    updated.state = operation.leaves();
    if matches!(operation, Operation::Destroy) {
        updated.address = None;
    }
    Ok(updated)
}

/// Posts one container job to a host's room.
async fn post(hosts: &HostRooms, host: HostId, job: &ContainerJob) -> Result<(), ProviderError> {
    hosts
        .command(host, &ControlToHost::Run { job: job.clone() })
        .await
        .map_err(|error| {
            ProviderError::Rejected(format!("this host's room would not take a job: {error}"))
        })
}

/// Refuses an operation on a machine whose host is not connected.
///
/// The check the *user-facing* routes make before they ask for anything: a
/// host holds its own socket, so flyco cannot wake it, and answering
/// [`ApiError::HostOffline`] tells the user the one thing that fixes it. A
/// non-host account passes straight through.
///
/// # Errors
///
/// Returns [`ApiError::HostOffline`] when the machine's room reports no
/// connected host.
pub async fn require_host_online(
    hosts: &HostRooms,
    account: &LinkedAccount,
) -> Result<(), ApiError> {
    let Some(host) = account.host() else {
        return Ok(());
    };
    if !hosts.status(host.id).await?.connected {
        return Err(ApiError::HostOffline);
    }
    Ok(())
}

/// What [`operate`] should do to a machine.
#[derive(Debug, Clone, Copy)]
pub enum Operation<'a> {
    /// Move to another machine type, keeping the disk.
    Resize {
        /// Provider-native machine type to move to.
        machine_type: &'a str,
    },
    /// Release compute, keep the disk.
    Stop,
    /// Put a deallocated machine back on compute.
    Start,
    /// Release compute and disk permanently.
    Destroy,
}

impl Operation<'_> {
    /// The same operation, as the thing a planner turns into container work.
    fn on(self, machine: &flyco_provider::Machine) -> flyco_provider::MachineOperation {
        let machine = machine.clone();
        match self {
            Self::Resize { machine_type } => flyco_provider::MachineOperation::Resize {
                machine,
                machine_type: machine_type.to_owned(),
            },
            Self::Stop => flyco_provider::MachineOperation::Deallocate { machine },
            Self::Start => flyco_provider::MachineOperation::Start { machine },
            Self::Destroy => flyco_provider::MachineOperation::Destroy { machine },
        }
    }

    /// The state a machine is in once this operation has been asked for.
    ///
    /// On a host the answer is not a guess: the container the job named is
    /// stopped, started or gone, and a job the room is still holding is one
    /// the machine performs the moment it is back.
    const fn leaves(self) -> flyco_core::MachineState {
        match self {
            Self::Stop => flyco_core::MachineState::Deallocated,
            Self::Start | Self::Resize { .. } => flyco_core::MachineState::Running,
            Self::Destroy => flyco_core::MachineState::Destroyed,
        }
    }

    /// Performs this operation against one driver.
    ///
    /// Generic over the driver rather than repeated per credential variant:
    /// the trait is not object-safe on purpose, and a generic function is
    /// what keeps every future unboxed while still writing "what a stop
    /// means" exactly once. A stop is the one that has something to say —
    /// the driver answers with nothing, and the machine a caller gets back
    /// has to say it is deallocated.
    async fn run<P: CloudProvider>(
        self,
        provider: &mut P,
        machine: &flyco_provider::Machine,
    ) -> Result<flyco_provider::Machine, ProviderError> {
        match self {
            Self::Resize { machine_type } => provider.resize(machine, machine_type).await,
            Self::Stop => provider.deallocate(machine).await.map(|()| {
                let mut stopped = machine.clone();
                stopped.state = flyco_core::MachineState::Deallocated;
                stopped
            }),
            Self::Start => provider.start(machine).await,
            Self::Destroy => provider.destroy(machine).await.map(|()| {
                let mut destroyed = machine.clone();
                destroyed.state = flyco_core::MachineState::Destroyed;
                destroyed.address = None;
                destroyed
            }),
        }
    }
}

/// The Azure driver an account opens, when it is an Azure account.
///
/// A thin destructuring over [`azure_driver`], so a caller holding a whole
/// account does not repeat which fields matter. `Ok(None)` means "not an
/// Azure account"; an error means it is one and cannot be opened.
///
/// # Errors
///
/// Returns [`ProviderError::Malformed`] for an Azure account with no
/// resource group recorded — see [`LinkedAccount::azure_workspace`].
fn azure_driver_for(account: &LinkedAccount) -> Result<Option<AzureProvider>, ProviderError> {
    let ProviderCredentials::Azure {
        tenant_id,
        client_id,
        client_secret,
        subscription_id,
        admin_ssh_public_key,
    } = account.credentials()
    else {
        return Ok(None);
    };

    Ok(Some(azure_driver(
        tenant_id,
        client_id,
        client_secret,
        subscription_id,
        account.azure_workspace()?,
        admin_ssh_public_key,
    )))
}

/// Confirms an account can actually deploy the machine a session asked for,
/// and answers with the catalog entry that prices it.
///
/// Run before a session row exists, so an impossible choice fails where the
/// user made it rather than two minutes later inside a queue consumer. The
/// entry that comes back is what the machine row's hourly price is taken
/// from, which is why this returns it instead of a bare `bool`: reading the
/// catalog twice would let the price drift from the machine it was checked
/// against.
///
/// For Azure the answer is the region report rather than the merged catalog,
/// because the report keeps *why* a machine type is missing — the SKU is not
/// sold here, the quota does not cover it, or the subscription's own policy
/// forbids the whole region — and those are three different things for the
/// user to fix.
///
/// # Errors
///
/// Returns [`ProviderError::Unavailable`] naming the reason when the account
/// cannot deploy this machine type in this region, or another
/// [`ProviderError`] when the provider could not be asked at all.
pub async fn deployable(
    account: &LinkedAccount,
    spec: &MachineSpec,
) -> Result<MachineCatalogEntry, ProviderError> {
    if let Some(mut azure) = azure_driver_for(account)? {
        let report = azure.region_report(&spec.region).await?;
        return report
            .offered
            .into_iter()
            .find(|entry| entry.machine_type == spec.machine_type)
            .ok_or_else(|| ProviderError::Unavailable {
                machine_type: spec.machine_type.clone(),
                region: spec.region.clone(),
                reason: report
                    .excluded
                    .iter()
                    .find(|(name, _)| *name == spec.machine_type || *name == spec.region)
                    .map_or_else(
                        || "this subscription is not offered it".to_owned(),
                        |(_, reason)| reason.to_string(),
                    ),
            });
    }

    let offered = catalog(account).await?;
    offered
        .into_iter()
        .find(|entry| {
            entry.machine_type == spec.machine_type
                && entry.region.eq_ignore_ascii_case(&spec.region)
        })
        .ok_or_else(|| ProviderError::Unavailable {
            machine_type: spec.machine_type.clone(),
            region: spec.region.clone(),
            reason: "this account offers no such machine type there".to_owned(),
        })
}

/// What brings a session's machine into existence.
///
/// A trait rather than one more free function, because the transport under a
/// driver is not always one the caller has: byo-ssh needs a TCP connection
/// the Worker does not have and a test has no host to open one to. The queue
/// consumer therefore takes its provisioner as a parameter, and
/// [`CloudProvisioner`] is what the deployed control plane hands it.
///
/// Not object-safe, for the same reason
/// [`CloudProvider`](flyco_provider::CloudProvider) is not: every future on
/// this path stays free of boxing on wasm32.
pub trait Provisioner {
    /// Brings one machine into existence, or says why it could not.
    fn provision(
        &mut self,
        account: &LinkedAccount,
        request: &ProvisionRequest,
    ) -> impl Future<Output = Result<Machine, ProviderError>>;

    /// Puts an existing machine back on compute, on the disk it kept.
    ///
    /// What recovering from a spot reclamation is, in one call: the
    /// provider stopped the machine and kept its disk, and this starts the
    /// same machine again. It is on this trait rather than reached through
    /// [`operate`] directly for the same reason [`provision`](Self::provision)
    /// is — the queue consumer must be drivable without a cloud account, and
    /// a recovery is a step a test has to be able to observe.
    fn restart(
        &mut self,
        account: &LinkedAccount,
        machine: &Machine,
    ) -> impl Future<Output = Result<Machine, ProviderError>>;
}

/// The provisioner the deployed control plane uses: the real drivers, over
/// the real network — and, for a machine the user owns, that machine's own
/// room.
///
/// It holds the host-room namespace because provisioning onto a host is not
/// a call to anybody's API: it is a container job posted down a socket the
/// machine itself opened, and the Durable Object holding that socket is the
/// only thing that can reach it.
#[derive(Debug, Clone)]
pub struct CloudProvisioner {
    hosts: HostRooms,
}

impl CloudProvisioner {
    /// Builds the provisioner around the host rooms this Worker can reach.
    #[must_use]
    pub const fn new(hosts: HostRooms) -> Self {
        Self { hosts }
    }
}

impl Provisioner for CloudProvisioner {
    async fn provision(
        &mut self,
        account: &LinkedAccount,
        request: &ProvisionRequest,
    ) -> Result<Machine, ProviderError> {
        if let Some(mut azure) = azure_driver_for(account)? {
            return azure.provision(request).await;
        }

        match account.credentials() {
            ProviderCredentials::Host { .. } => {
                provision_on_host(&self.hosts, account, request).await
            }
            // Unreachable: `azure_driver` answered for the Azure variant.
            ProviderCredentials::Azure { .. } => Err(ProviderError::Malformed(
                "an Azure account produced no Azure driver",
            )),
            ProviderCredentials::Aws { .. } => Err(ProviderError::Unsupported {
                provider: "AWS",
                operation: "provision",
                reason: "flyco has no AWS driver yet",
            }),
            ProviderCredentials::Gcp { .. } => Err(ProviderError::Unsupported {
                provider: "GCP",
                operation: "provision",
                reason: "flyco has no GCP driver yet",
            }),
        }
    }

    async fn restart(
        &mut self,
        account: &LinkedAccount,
        machine: &Machine,
    ) -> Result<Machine, ProviderError> {
        operate(&self.hosts, account, machine, Operation::Start).await
    }
}

/// Asks a machine the user owns to build a session's container.
///
/// What comes back is the same claim a cloud driver makes when its API
/// accepts a create: the machine is flyco's now. It is true here for a
/// different reason — the host's room took the job *durably*, and holds it
/// until the machine answers, so a container that has not been built yet is
/// one that will be — and the machine is not ready either way, which is why
/// a session goes active when its daemon arrives rather than when this
/// returns.
///
/// The row is then *completed* by the host's `JobResult`: the container and
/// the volume Podman actually created, which is the pair every later stop,
/// start and removal has to name, and which nothing but the machine knows.
/// A job that failed comes back the same way and fails the session with what
/// Podman said. See [`crate::hosts::record_job_result`].
///
/// Unlike the lifecycle operations, this does *not* refuse a machine that is
/// not connected. A host offline at session creation offers no catalog and
/// is never chosen; reaching here means the socket dropped in the seconds
/// since, and the honest answer to that is the one the room already gives —
/// hold the job and hand it over when the machine is back — rather than
/// failing a session over a reconnect.
async fn provision_on_host(
    hosts: &HostRooms,
    account: &LinkedAccount,
    request: &ProvisionRequest,
) -> Result<Machine, ProviderError> {
    let planner = account.host_planner().ok_or(ProviderError::Malformed(
        "this host account names no enrolled machine; enroll it again",
    ))?;
    let job = planner.plan(&flyco_provider::MachineOperation::Provision(Box::new(
        request.clone(),
    )))?;
    post(hosts, planner.id(), &job).await?;

    Ok(Machine {
        id: request.machine,
        native_id: job.container().to_owned(),
        // A host is its own region, and there is nowhere else to put it —
        // the same answer its catalog gives.
        region: planner.machine_type().to_owned(),
        state: flyco_core::MachineState::Running,
        // A container on hardware the user owns is never interruptible,
        // whatever the session asked for.
        capacity_mode: flyco_provider::CapacityMode::OnDemand,
        address: Some(planner.machine_type().to_owned()),
    })
}
