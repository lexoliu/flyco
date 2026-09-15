//! The Azure driver.
//!
//! Plain HTTPS against Azure Resource Manager, so the whole of it runs
//! inside the Cloudflare Worker. Written against
//! `docs/research/azure-arm.md`, which measured a real subscription; where
//! that document contradicts an assumption, the document wins.
//!
//! # The provisioning sequence
//!
//! The resource group, the virtual network and the network security group
//! are **workspace** infrastructure: one set per region, created once and
//! reused. Per session it is three `PUT`s, three round trips deep — public
//! IP, then network interface, then virtual machine. The resource group is
//! not among them: it is created once, when the account is linked, by
//! [`AzureProvider::ensure_resource_group`], so the driver's steady state
//! has no group `PUT` in it. That is possible because the service principal
//! the wizard mints is `Contributor` on the whole subscription — a
//! resource-group-scoped role could not create the group it is scoped to,
//! which is why the user used to have to make one by hand.
//!
//! The network security group is not optional. Basic public IPs were retired
//! on 2025-09-30, Standard ones are closed to inbound traffic by default, and
//! a machine provisioned without one comes up perfectly and answers nothing.
//!
//! # Three gates, not two
//!
//! Whether a machine can actually be created is the intersection of three
//! independent things, and each of them is invisible from the others:
//!
//! 1. **SKU restrictions** ([`skus`]) — `Location` means unusable in that
//!    region, `Zone` means usable but only as a regional deployment.
//! 2. **Quota** ([`skus::Quotas`]) — on-demand spends the machine type's
//!    family and the region's `cores`; spot spends only
//!    `lowPriorityCores` and no family quota at all.
//! 3. **The subscription's own policy** ([`policy`]) — an
//!    allowed-regions assignment refuses every `PUT` into a region outside
//!    its list, including the virtual network, and the SKU list says
//!    nothing about it. Measured: the region with the most deployable
//!    machine types on the reference subscription is forbidden outright.
//!
//! [`AzureProvider::region_report`] is the whole computation with its
//! workings shown, so a user can be told *why* a machine is not on offer —
//! "your subscription's policy forbids this region" is a different problem
//! from "no quota" and from "not sold here".
//!
//! # Spot is attempted, and usually works
//!
//! Spot is requested by default. When the subscription or the machine type
//! turns out not to support it the identical body is re-sent with the three
//! spot fields removed, and the capacity actually obtained is recorded on
//! the [`Machine`], because that — not what was asked for — is what the
//! price follows. The two codes that trigger the fallback are
//! [`SPOT_UNSUPPORTED_CODES`]; every other refusal is a real failure. The
//! fallback is defensive rather than the expected path: spot was verified
//! working on the reference subscription, and the B-series is genuinely
//! spot-ineligible whatever its SKU and price meters advertise.
//!
//! # `zones` is never sent
//!
//! Zone-restricted-but-usable is the dominant pattern for the machine types
//! a small subscription can run, and a regional deployment is what succeeds.
//! The field is not modelled at all — see [`bodies`].
//!
//! # Two runtimes, one subscription
//!
//! Everything above is the [`Runtime::Vm`] half. The same subscription also
//! sells [`Runtime::Container`]: a session there is one execution of an
//! Azure Container Apps job, provisioned, stopped, started, resized and
//! destroyed through [`containers`]. It is one driver rather than two
//! because it is one account, one token and one resource group — what
//! differs is the resource provider a call is addressed to and what a stop
//! means, and [`MachineSpec::runtime`] and [`Machine::runtime`] are what
//! say which of the two a caller is asking for.
//!
//! None of the three gates above applies to it. Container Apps publishes no
//! SKU list and spends no vCPU quota (its own quota is per environment and
//! Azure exposes it only through a support request), so the only gate a
//! container passes is the subscription's region policy — which does apply,
//! because a policy refuses the environment's `PUT` exactly as it refuses a
//! virtual network's.

pub mod arm;
pub mod auth;
pub mod bodies;
pub mod containers;
pub mod costs;
pub mod policy;
pub mod pricing;
pub mod skus;

#[cfg(test)]
mod tests;

use flyco_core::machine::{
    CloudProviderKind, CpuArchitecture, FreeGrant, MachineCapacity, MachineCatalogEntry,
    MachinePricing, MachineSpec, MachineState, OsFamily, Runtime, StoragePricing,
};
use flyco_core::money::Usd;
use flyco_core::{CloudSpend, MachineId};

use crate::clock::{MonotonicClock, SystemClock, SystemTimer, Timer};
use crate::http::{HttpRequest, HttpResponse, HttpTransport, Method};
use crate::login_key::LoginKey;
use crate::polling::{MAX_POLL_ATTEMPTS, POLLS_PER_INVOCATION, poll_delay};
use crate::{
    CapacityMode, CloudProvider, Continuation, LiveTransport, Machine, ProviderError,
    ProvisionRequest, Provisioning, cloud_init, flycod,
};

use arm::{ErrorBody, Follow, OperationBody, OperationStatus, ProviderRegistration, api_version};
use auth::{ServicePrincipal, TokenCache};
use costs::{CostQuery, CostResult};
use policy::{AssignmentPage, RegionPolicy};
use pricing::PriceCatalog;
use skus::{Availability, Quotas, Sku, SkuPage, UsagePage};

/// Driver name, as it appears in [`ProviderError::Unsupported`].
pub const PROVIDER: &str = "azure";

/// What Azure Container Apps gives away per subscription per calendar
/// month, jobs included.
///
/// The published Consumption-plan allowance: 180,000 vCPU-seconds and
/// 360,000 GiB-seconds
/// ([learn.microsoft.com/azure/container-apps/billing](https://learn.microsoft.com/azure/container-apps/billing)),
/// which is about twelve and a half hours of a 4 vCPU / 8 GiB job before the
/// vCPU half — the binding one at that shape — runs out.
///
/// Declared here, beside the driver that will publish it on its container
/// entries, rather than in `flyco_core`: it is a fact about one provider's
/// price list, and the type that carries it
/// ([`flyco_core::FreeGrant`]) is what the catalog shares.
pub const CONTAINER_APPS_FREE_GRANT: FreeGrant = FreeGrant {
    vcpu_seconds_per_month: 180_000,
    gib_seconds_per_month: 360_000,
};

/// Resource-provider path of a Container Apps managed environment.
pub const CONTAINER_ENVIRONMENTS_PATH: &str = "Microsoft.App/managedEnvironments";

/// Resource-provider path of a Container Apps job.
pub const CONTAINER_JOBS_PATH: &str = "Microsoft.App/jobs";

/// Regions a catalog covers when the subscription restricts none and the
/// caller names none either.
///
/// Deliberately a short, overridable starting point rather than an attempt
/// at every Azure region: a catalog is three reads per region, and an
/// unrestricted subscription can deploy to sixty of them. Where the
/// subscription *does* carry an allowed-regions policy this list is never
/// consulted — the policy's own list is the catalog's region set, which is
/// the only answer that cannot be wrong for somebody else's account.
pub const DEFAULT_CANDIDATE_REGIONS: [&str; 3] = ["eastus", "westeurope", "southeastasia"];

/// The resource group flyco creates and owns in every linked subscription.
///
/// Not a user input. The service principal the wizard mints is
/// `Contributor` on the subscription, which is the scope a resource-group
/// creation needs, so flyco makes the group itself the moment the account is
/// linked — see [`AzureProvider::ensure_resource_group`]. Its name is
/// recorded on the account rather than assumed at read time, so a
/// subscription linked under one name keeps the group it actually owns if
/// this constant ever changes.
pub const RESOURCE_GROUP: &str = "flyco";

/// Address space of a workspace virtual network.
pub const VNET_ADDRESS_SPACE: &str = "10.42.0.0/16";

/// Address prefix of the one subnet inside it.
pub const SUBNET_ADDRESS_PREFIX: &str = "10.42.0.0/24";

/// Name of that subnet.
pub const SUBNET_NAME: &str = "default";

/// Name of the inbound SSH rule on the workspace security group.
pub const SSH_RULE_NAME: &str = "allow-ssh-inbound";

/// The administrative login a machine is created with.
pub const ADMIN_USERNAME: &str = "flyco";

/// Ubuntu 24.04 LTS, the image family every flyco machine boots.
pub const IMAGE_PUBLISHER: &str = "Canonical";

/// The Ubuntu 24.04 offer.
pub const IMAGE_OFFER: &str = "ubuntu-24_04-lts";

/// Image SKU for an x64 machine type.
pub const IMAGE_SKU_X64: &str = "server";

/// Image SKU for an Arm64 machine type.
///
/// Chosen from the machine type's own `CpuArchitectureType`, never
/// hardcoded: several of the only-deployable types on a small subscription
/// are Arm64, and pairing one with the x64 image fails at deploy time.
pub const IMAGE_SKU_ARM64: &str = "server-arm64";

/// Storage tier of the OS disk.
pub const OS_DISK_TYPE: &str = "StandardSSD_LRS";

/// The two refusals that mean "ask again without the spot fields".
///
/// The first is the subscription's offer type — Azure for Students is not a
/// supported spot offer — and the second is the machine type, because the
/// B-series is excluded from spot regardless of offer even though both the
/// SKU list and the price list advertise spot meters for it. Everything else
/// is a genuine failure.
pub const SPOT_UNSUPPORTED_CODES: [&str; 2] = [
    "AzureSpotFeatureNotEnabledForSubscription",
    "AzureSpotIsNotSupportedForThisVMSize",
];

/// The code a Container Apps job answers a write with while an earlier
/// write to it is still being carried out — the signature of a second leg
/// of one build running beside the first.
const JOB_BUSY_CODE: &str = "ContainerAppsJobOperationInProgress";

/// How many polls a leg joining a build waits for an execution to appear
/// before starting one itself. The live leg it shadows needs a single
/// request to reach `start`, so an empty executions list past this means
/// that leg died in between rather than that there is nothing to join.
const JOIN_GRACE_POLLS: usize = 3;

/// The names every flyco resource in one workspace answers to.
///
/// Derived from the machine id rather than allocated, so a name is
/// recoverable without a column to store it in and two callers cannot
/// disagree. Workspace names carry their region because a virtual network's
/// location is immutable: one name per region, or the second region's `PUT`
/// fails against the first region's network.
pub mod names {
    use flyco_core::MachineId;

    /// The workspace virtual network in one region.
    #[must_use]
    pub fn vnet(region: &str) -> String {
        format!("flyco-{region}-vnet")
    }

    /// The workspace network security group in one region.
    #[must_use]
    pub fn nsg(region: &str) -> String {
        format!("flyco-{region}-nsg")
    }

    /// A machine's virtual machine, which is also its hostname.
    #[must_use]
    pub fn machine(id: MachineId) -> String {
        format!("flyco-{id}")
    }

    /// A machine's public IP.
    #[must_use]
    pub fn public_ip(id: MachineId) -> String {
        format!("flyco-{id}-pip")
    }

    /// A machine's network interface.
    #[must_use]
    pub fn network_interface(id: MachineId) -> String {
        format!("flyco-{id}-nic")
    }

    /// A machine's OS disk.
    #[must_use]
    pub fn os_disk(id: MachineId) -> String {
        format!("flyco-{id}-osdisk")
    }

    /// A machine's DNS label, which must be unique within its region.
    #[must_use]
    pub fn dns_label(id: MachineId) -> String {
        machine(id)
    }

    /// The address a machine answers on once its public IP exists.
    #[must_use]
    pub fn fqdn(id: MachineId, region: &str) -> String {
        format!("{}.{region}.cloudapp.azure.com", dns_label(id))
    }
}

/// Why a machine type is not on offer.
///
/// Named rather than dropped, because the three answers ask the user for
/// three different things: change region, ask for a quota increase, or pick
/// another machine type.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExclusionReason {
    /// The subscription's own policy forbids the whole region.
    #[error("{0}")]
    RegionForbidden(String),
    /// Azure does not offer this machine type to this subscription here.
    #[error("this machine type is not offered here: {0}")]
    NotOffered(String),
    /// Neither the on-demand pools nor the spot pool has room for it.
    #[error("{0}")]
    NoQuota(String),
    /// The retail-prices API publishes no Linux price for it, so flyco
    /// cannot tell the user what an hour would cost.
    #[error("no published price")]
    Unpriced,
    /// ARM described a machine type without a usable size.
    #[error("Azure reported this machine type without a size")]
    Unreadable,
}

/// What one region offers, and why everything else was left out.
#[derive(Debug, Clone)]
pub struct RegionReport {
    /// The region this covers.
    pub region: String,
    /// The machine types a session can actually be started on.
    pub offered: Vec<MachineCatalogEntry>,
    /// Everything else, paired with the reason. When the region itself is
    /// forbidden this holds one entry naming the region rather than one per
    /// machine type: the answer is the same for all of them.
    pub excluded: Vec<(String, ExclusionReason)>,
}

/// The parts of an Azure account that are configuration rather than
/// credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// The resource group every flyco resource in this subscription lives
    /// in.
    ///
    /// [`RESOURCE_GROUP`] for an account linked through the wizard, which
    /// creates it. Carried here rather than assumed so an account linked
    /// under a different name still resolves to the group it owns.
    pub resource_group: String,
    /// The login key installed on every machine this workspace builds.
    ///
    /// Flyco's, not the user's: see [`LoginKey`].
    pub login_key: LoginKey,
    /// Where a machine fetches `flycod` from on first boot.
    pub flycod_installer_url: String,
    /// Regions [`AzureProvider::catalog`] reports on.
    ///
    /// Empty — the default — means "whatever the subscription's own policy
    /// allows", which is the only region set that is right for an account
    /// this code has never seen. A non-empty list narrows that further, and
    /// a named region the policy forbids is dropped rather than attempted.
    pub regions: Vec<String>,
}

impl Workspace {
    /// A workspace covering whatever regions the subscription allows,
    /// installing `flycod` from [`DEFAULT_FLYCOD_INSTALLER_URL`].
    #[must_use]
    pub fn new(resource_group: impl Into<String>, login_key: LoginKey) -> Self {
        Self {
            resource_group: resource_group.into(),
            login_key,
            flycod_installer_url: cloud_init::DEFAULT_FLYCOD_INSTALLER_URL.to_owned(),
            regions: Vec::new(),
        }
    }

    /// Reports a catalog for these regions instead.
    #[must_use]
    pub fn with_regions(mut self, regions: Vec<String>) -> Self {
        self.regions = regions;
        self
    }

    /// Installs `flycod` from somewhere else.
    #[must_use]
    pub fn with_installer(mut self, url: impl Into<String>) -> Self {
        self.flycod_installer_url = url.into();
        self
    }
}

/// The Azure driver.
///
/// Generic over its transport, clock and timer so the whole of it is
/// testable against recorded exchanges — see `crate::testing`.
#[derive(Debug)]
pub struct AzureProvider<T = LiveTransport, C = SystemClock, K = SystemTimer> {
    transport: T,
    clock: C,
    timer: K,
    tokens: TokenCache,
    workspace: Workspace,
    prices: PriceCatalog,
    region_policy: Option<RegionPolicy>,
    /// Whether this instance has seen the subscription registered for
    /// [`containers::PROVIDER_NAMESPACE`]. Registration is permanent once
    /// done, so one read per driver instance is one too many only in the
    /// steady state — and that read is a subscription-level `GET` that
    /// costs nothing next to the environment `PUT` it guards.
    container_provider_registered: bool,
}

impl AzureProvider {
    /// The driver as it is deployed: the live transport, the host clock, a real timer.
    #[must_use]
    pub fn new(principal: ServicePrincipal, workspace: Workspace) -> Self {
        Self::with_parts(
            LiveTransport::new(),
            SystemClock::new(),
            SystemTimer::new(),
            principal,
            workspace,
        )
    }
}

/// Where a job start had got to after one invocation's polls.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Started {
    /// The execution came up, and this is Azure's name for it.
    Running(String),
    /// Still starting: how to keep following it.
    Pending(Follow),
    /// No start to follow yet: the job's own write is still being carried
    /// out by another leg of this build, and what carries on is a join.
    Joining,
}

/// Where an operation had got to after a bounded number of polls.
#[derive(Debug)]
enum Followed {
    /// Finished, with the response that describes the resource where the
    /// follow pattern hands one back (`Location`), and nothing where it
    /// only describes the operation (`Azure-AsyncOperation`).
    Done(Option<HttpResponse>),
    /// Still running: where to carry on from.
    Still(Follow),
}

impl<T: HttpTransport, C: MonotonicClock, K: Timer> AzureProvider<T, C, K> {
    /// The driver over an explicit transport, clock and timer.
    pub const fn with_parts(
        transport: T,
        clock: C,
        timer: K,
        principal: ServicePrincipal,
        workspace: Workspace,
    ) -> Self {
        Self {
            transport,
            clock,
            timer,
            tokens: TokenCache::new(principal),
            workspace,
            prices: PriceCatalog::new(),
            region_policy: None,
            container_provider_registered: false,
        }
    }

    /// The transport this driver sends through.
    ///
    /// Exposed so a test can read back exactly what was put on the wire; a
    /// driver whose requests cannot be inspected can only be checked against
    /// a live subscription.
    pub const fn transport(&self) -> &T {
        &self.transport
    }

    /// The timer this driver waits on, for the same reason.
    pub const fn timer(&self) -> &K {
        &self.timer
    }

    /// The subscription every URL is built under.
    fn subscription(&self) -> &str {
        &self.tokens.principal().subscription_id
    }

    fn resource_url(&self, provider_path: &str, name: &str, api_version: &str) -> String {
        arm::resource_url(
            self.subscription(),
            &self.workspace.resource_group,
            provider_path,
            name,
            api_version,
        )
    }

    fn resource_id(&self, provider_path: &str, name: &str) -> String {
        format!(
            "/subscriptions/{}/resourceGroups/{}/providers/{provider_path}/{name}",
            self.subscription(),
            self.workspace.resource_group,
        )
    }

    /// Sends one authenticated request, re-minting the token on a 401.
    ///
    /// A 401 is retried exactly once: the service is the authority on
    /// whether a token still works, and a second refusal after a fresh token
    /// is about the principal rather than the credential's age.
    async fn send(&mut self, request: HttpRequest) -> Result<HttpResponse, ProviderError> {
        let token = self
            .tokens
            .access_token(&self.transport, &self.clock)
            .await?;
        let response = self.transport.send(request.clone().bearer(&token)).await?;
        if response.status != 401 {
            return Ok(response);
        }

        tracing::debug!("Azure rejected a management token; minting a fresh one");
        self.tokens.invalidate();
        let token = self
            .tokens
            .access_token(&self.transport, &self.clock)
            .await?;
        Ok(self.transport.send(request.bearer(&token)).await?)
    }

    /// Sends a mutating request and waits for the operation it started.
    async fn send_and_await(&mut self, request: HttpRequest) -> Result<(), ProviderError> {
        let response = self.send(request).await?;
        if !response.is_success() {
            return Err(refusal(&response));
        }
        self.await_operation(response).await.map(drop)
    }

    /// Deletes a resource and waits for the deletion, or answers early when
    /// it was already gone.
    ///
    /// Delete is the one operation whose refusal can mean success: a machine
    /// being destroyed may have lost resources already — an execution that
    /// expired, a disk an earlier destroy attempt removed — and asking again
    /// for what is not there is the end state, not an error.
    async fn delete_and_await(&mut self, url: String) -> Result<(), ProviderError> {
        let response = self.send(HttpRequest::new(Method::Delete, url)).await?;
        if gone(&response) {
            return Ok(());
        }
        if !response.is_success() {
            return Err(refusal(&response));
        }
        self.await_operation(response).await.map(drop)
    }

    /// Follows an asynchronous operation to a terminal state, answering
    /// with the response that describes the **resource**, where one does.
    ///
    /// `Azure-AsyncOperation` is preferred over `Location` and the terminal
    /// set is exactly `{Succeeded, Failed, Canceled}` — anything else means
    /// keep polling, including in-flight values a resource provider invents.
    /// The operation's status is what is trusted, never the resource body: a
    /// failed resize leaves the machine reporting the size it was asked for
    /// while still running on the old one.
    ///
    /// `None` is what an `Azure-AsyncOperation` poll ends as: that document
    /// describes the *operation* — its own id, its status — so a caller that
    /// needs something the service named (the execution a job start
    /// created) has nothing to read here, and must say so rather than read
    /// the operation's id as the resource's name. Every caller that only
    /// needs "did it work" ignores it.
    async fn await_operation(
        &mut self,
        accepted: HttpResponse,
    ) -> Result<Option<HttpResponse>, ProviderError> {
        let follow = arm::follow(&accepted)?;
        if follow == Follow::Finished {
            return Ok(Some(accepted));
        }
        match self.follow_operation(follow, MAX_POLL_ATTEMPTS).await? {
            Followed::Done(resource) => Ok(resource),
            Followed::Still(_) => Err(ProviderError::Rejected(format!(
                "an Azure operation was still running after {MAX_POLL_ATTEMPTS} polls"
            ))),
        }
    }

    /// Polls an operation up to `budget` times, and says where it got to.
    ///
    /// The budget is what makes a build resumable: a caller with one
    /// invocation's worth of polls hands the [`Followed::Still`] back as a
    /// continuation instead of spending past its subrequest ceiling
    /// (issue #257), and carries on from it next time.
    async fn follow_operation(
        &mut self,
        mut follow: Follow,
        budget: usize,
    ) -> Result<Followed, ProviderError> {
        for attempt in 0..budget {
            let (url, retry_after, by_status) = match &follow {
                // Nothing to poll: the call that produced this was done.
                Follow::Finished => return Ok(Followed::Done(None)),
                Follow::Operation { url, retry_after } => (url.clone(), *retry_after, false),
                Follow::Location { url, retry_after } => (url.clone(), *retry_after, true),
            };

            self.timer.sleep(poll_delay(retry_after, attempt)).await;
            let polled = self
                .send(HttpRequest::new(Method::Get, url.clone()))
                .await?;
            let retry_after = polled
                .header_value(arm::RETRY_AFTER_HEADER)
                .and_then(|value| value.trim().parse::<u32>().ok());

            if by_status {
                // The `Location` pattern reports through the HTTP status:
                // `202` is still running, anything else successful is done.
                if polled.status == 202 {
                    follow = Follow::Location { url, retry_after };
                    continue;
                }
                if polled.is_success() {
                    return Ok(Followed::Done(Some(polled)));
                }
                return Err(refusal(&polled));
            }

            if !polled.is_success() {
                return Err(refusal(&polled));
            }

            let body: OperationBody = polled.json()?;
            match OperationStatus::parse(&body.status) {
                OperationStatus::Succeeded => return Ok(Followed::Done(None)),
                OperationStatus::Failed | OperationStatus::Canceled => {
                    let error = body.error.unwrap_or_default();
                    return Err(ProviderError::OperationFailed {
                        status: body.status,
                        code: error.code,
                        message: error.message,
                    });
                }
                OperationStatus::Running(_) => {
                    follow = Follow::Operation { url, retry_after };
                }
            }
        }

        Ok(Followed::Still(follow))
    }

    /// Creates the one-time infrastructure a region's machines share.
    ///
    /// Idempotent: both `PUT`s are create-or-update, so a second session in
    /// the same region re-sends the same bodies and Azure answers that
    /// nothing changed.
    async fn ensure_workspace(&mut self, region: &str) -> Result<(), ProviderError> {
        let vnet = bodies::VirtualNetwork {
            location: region.to_owned(),
            properties: bodies::VirtualNetworkProperties {
                address_space: bodies::AddressSpace {
                    address_prefixes: vec![VNET_ADDRESS_SPACE.to_owned()],
                },
                subnets: vec![bodies::Subnet {
                    name: SUBNET_NAME.to_owned(),
                    properties: bodies::SubnetProperties {
                        address_prefix: SUBNET_ADDRESS_PREFIX.to_owned(),
                    },
                }],
            },
        };
        self.send_and_await(
            HttpRequest::new(
                Method::Put,
                self.resource_url(
                    "Microsoft.Network/virtualNetworks",
                    &names::vnet(region),
                    api_version::NETWORK,
                ),
            )
            .json_body(&vnet)?,
        )
        .await?;

        let nsg = bodies::NetworkSecurityGroup {
            location: region.to_owned(),
            properties: bodies::NetworkSecurityGroupProperties {
                security_rules: vec![bodies::SecurityRule {
                    name: SSH_RULE_NAME.to_owned(),
                    properties: bodies::SecurityRuleProperties {
                        protocol: "Tcp",
                        source_port_range: "*",
                        destination_port_range: "22".to_owned(),
                        source_address_prefix: "Internet".to_owned(),
                        destination_address_prefix: "*",
                        access: "Allow",
                        priority: 1_000,
                        direction: "Inbound",
                    },
                }],
            },
        };
        self.send_and_await(
            HttpRequest::new(
                Method::Put,
                self.resource_url(
                    "Microsoft.Network/networkSecurityGroups",
                    &names::nsg(region),
                    api_version::NETWORK,
                ),
            )
            .json_body(&nsg)?,
        )
        .await
    }

    /// The subscription's allowed-regions policy, read once per driver.
    ///
    /// Cached for the driver's lifetime rather than per call: a policy
    /// assignment changes on a human timescale and a driver lives for one
    /// operation, so re-reading it before every `PUT` would be a request per
    /// resource for an answer that cannot have moved.
    async fn region_policy(&mut self) -> Result<RegionPolicy, ProviderError> {
        if let Some(policy) = &self.region_policy {
            return Ok(policy.clone());
        }

        let url = arm::subscription_url(
            self.subscription(),
            "providers/Microsoft.Authorization/policyAssignments",
            api_version::POLICY_ASSIGNMENTS,
        );
        let response = self.send(HttpRequest::new(Method::Get, url)).await?;
        if !response.is_success() {
            return Err(refusal(&response));
        }

        let page: AssignmentPage = response.json()?;
        let policy = RegionPolicy::from_page(&page);
        if let Some(regions) = policy.regions() {
            tracing::debug!(
                allowed = regions.join(","),
                "the subscription restricts where it may deploy"
            );
        }
        self.region_policy = Some(policy.clone());
        Ok(policy)
    }

    /// The region this subscription deploys to when nobody names one.
    ///
    /// The first region its own policy allows, or the first candidate when
    /// it restricts none. "Default" in the only sense a subscription has
    /// one: Azure publishes no such field, and the first region flyco would
    /// actually deploy into is the honest answer to "where does this
    /// account live".
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Unavailable`] when the subscription's policy
    /// allows no region at all, which is an account no machine can be
    /// created in and a fact the user has to act on.
    pub async fn default_region(&mut self) -> Result<String, ProviderError> {
        self.catalog_regions()
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| ProviderError::Unavailable {
                machine_type: "any".to_owned(),
                region: "any".to_owned(),
                reason: "this subscription's policy allows no region flyco can deploy into"
                    .to_owned(),
            })
    }

    /// Creates the resource group flyco owns in this subscription.
    ///
    /// Called once, when the account is linked, and never during
    /// provisioning: the service principal the wizard mints is
    /// `Contributor` on the whole subscription, so flyco can make the group
    /// itself rather than asking a user to name one they created by hand.
    /// The `PUT` is create-or-update, so linking the same subscription twice
    /// is a no-op rather than a conflict.
    ///
    /// Answers with the region the group's record was placed in, which is
    /// only where the record lives — machines inside it are created in
    /// whatever region a session asks for.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] if the subscription refuses the write,
    /// which is what an under-scoped service principal looks like.
    pub async fn ensure_resource_group(&mut self) -> Result<String, ProviderError> {
        let location = self.default_region().await?;
        let url = arm::subscription_url(
            self.subscription(),
            &format!("resourcegroups/{}", self.workspace.resource_group),
            api_version::RESOURCE_GROUPS,
        );
        let body = bodies::ResourceGroup {
            location: location.clone(),
        };

        let response = self
            .send(HttpRequest::new(Method::Put, url).json_body(&body)?)
            .await?;
        if !response.is_success() {
            return Err(refusal(&response));
        }

        tracing::info!(
            group = %self.workspace.resource_group,
            %location,
            "created or refreshed the resource group flyco owns"
        );

        // Asked for now, awaited later. Registering `Microsoft.App` is a
        // subscription-wide action that takes a minute or two the first
        // time, and this runs inside the request that links the account, so
        // it is started here and finished by the first container provision
        // — which by then usually finds it done.
        self.register_container_provider().await?;
        Ok(location)
    }

    /// The URL of the subscription's registration record for
    /// [`containers::PROVIDER_NAMESPACE`], with `action` appended when there
    /// is one.
    fn container_provider_url(&self, action: &str) -> String {
        arm::subscription_url(
            self.subscription(),
            &format!("providers/{}{action}", containers::PROVIDER_NAMESPACE),
            api_version::RESOURCE_PROVIDERS,
        )
    }

    /// Asks Azure to register the subscription for Container Apps.
    ///
    /// Idempotent: on a subscription that is already registered the action
    /// answers the current record and changes nothing. The state it answers
    /// with is logged and not waited on — see
    /// [`Self::ensure_container_provider`] for the wait.
    async fn register_container_provider(&mut self) -> Result<ProviderRegistration, ProviderError> {
        let response = self
            .send(HttpRequest::new(
                Method::Post,
                self.container_provider_url("/register"),
            ))
            .await?;
        if !response.is_success() {
            return Err(refusal(&response));
        }
        let registration: ProviderRegistration = response.json()?;
        tracing::info!(
            namespace = containers::PROVIDER_NAMESPACE,
            state = %registration.registration_state,
            "asked the subscription to register the Container Apps provider"
        );
        Ok(registration)
    }

    /// Makes sure the subscription can create Container Apps resources,
    /// registering [`containers::PROVIDER_NAMESPACE`] and waiting for the
    /// registration to land when it has not.
    ///
    /// Without this the first environment `PUT` on a fresh subscription is
    /// refused with `MissingSubscriptionRegistration`, and the session it
    /// was for fails for a reason that is nobody's fault and nothing a user
    /// can fix from flyco. Registration is one-time per subscription, so
    /// the read is skipped once this instance has seen it registered.
    ///
    /// # Errors
    ///
    /// [`ProviderError::Rejected`] when the registration has not landed
    /// after the driver's polling budget, and [`ProviderError::Refused`]
    /// when Azure refuses the read or the action outright.
    async fn ensure_container_provider(&mut self) -> Result<(), ProviderError> {
        if self.container_provider_registered {
            return Ok(());
        }
        let mut registration = self.read_container_provider().await?;
        if !registration.is_registered() {
            registration = self.register_container_provider().await?;
        }
        let mut attempt = 0;
        while !registration.is_registered() {
            if attempt == MAX_POLL_ATTEMPTS {
                return Err(ProviderError::Rejected(format!(
                    "the subscription is still `{}` for {} after {MAX_POLL_ATTEMPTS} reads",
                    registration.registration_state,
                    containers::PROVIDER_NAMESPACE
                )));
            }
            self.timer.sleep(poll_delay(None, attempt)).await;
            attempt += 1;
            registration = self.read_container_provider().await?;
        }
        tracing::info!(
            namespace = containers::PROVIDER_NAMESPACE,
            "the subscription is registered for Container Apps"
        );
        self.container_provider_registered = true;
        Ok(())
    }

    async fn read_container_provider(&mut self) -> Result<ProviderRegistration, ProviderError> {
        let response = self
            .send(HttpRequest::new(
                Method::Get,
                self.container_provider_url(""),
            ))
            .await?;
        if !response.is_success() {
            return Err(refusal(&response));
        }
        Ok(response.json()?)
    }

    /// The regions a catalog covers.
    ///
    /// The subscription's policy is the source of truth; a caller's own list
    /// narrows it. A region the caller named and the policy forbids is
    /// dropped here rather than producing an empty region report the user
    /// would have to interpret.
    ///
    /// Public because a catalog is refreshed one region per queue message:
    /// the control plane asks which regions this account has before it can
    /// fan those messages out, and that answer is the subscription's own
    /// policy rather than anything the caller may assume.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] if the subscription's policy assignments
    /// cannot be read.
    pub async fn catalog_regions(&mut self) -> Result<Vec<String>, ProviderError> {
        let policy = self.region_policy().await?;
        let asked: Vec<String> = self.workspace.regions.clone();

        Ok(match (asked.is_empty(), policy.regions()) {
            (true, Some(allowed)) => allowed.to_vec(),
            (true, None) => DEFAULT_CANDIDATE_REGIONS
                .iter()
                .map(|region| (*region).to_owned())
                .collect(),
            (false, _) => asked
                .into_iter()
                .filter(|region| policy.allows(region))
                .collect(),
        })
    }

    /// Every virtual-machine SKU the subscription lists for one region.
    async fn list_skus(&mut self, region: &str) -> Result<Vec<Sku>, ProviderError> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("api-version", api_version::SKUS)
            .append_pair("$filter", &format!("location eq '{region}'"))
            .finish();
        let mut next = Some(format!(
            "{}/subscriptions/{}/providers/Microsoft.Compute/skus?{query}",
            arm::MANAGEMENT_BASE,
            self.subscription()
        ));

        let mut collected = Vec::new();
        while let Some(url) = next {
            let response = self.send(HttpRequest::new(Method::Get, url)).await?;
            if !response.is_success() {
                return Err(refusal(&response));
            }
            let page: SkuPage = response.json()?;
            collected.extend(page.value.into_iter().filter(Sku::is_virtual_machine));
            next = page.next_link;
        }
        Ok(collected)
    }

    /// One region's quotas.
    async fn read_quotas(&mut self, region: &str) -> Result<Quotas, ProviderError> {
        let url = arm::subscription_url(
            self.subscription(),
            &format!("providers/Microsoft.Compute/locations/{region}/usages"),
            api_version::USAGES,
        );
        let response = self.send(HttpRequest::new(Method::Get, url)).await?;
        if !response.is_success() {
            return Err(refusal(&response));
        }
        let page: UsagePage = response.json()?;
        Ok(Quotas::from_page(page))
    }

    /// Finds a machine type and refuses it unless all three gates pass.
    ///
    /// Refusing here rather than at the `PUT` is the whole point: on a small
    /// subscription "offered here", "has quota" and "the policy permits this
    /// region" come apart constantly, and the failure Azure returns — two
    /// minutes into a deployment, or as an opaque `RequestDisallowedByAzure`
    /// on the *virtual network* — names none of them.
    ///
    /// The quotas come back with the SKU because the spot fallback needs to
    /// re-check them against the other capacity pool without a second read.
    async fn deployable_sku(
        &mut self,
        region: &str,
        machine_type: &str,
        mode: CapacityMode,
    ) -> Result<(Sku, Quotas), ProviderError> {
        let policy = self.region_policy().await?;
        if !policy.allows(region) {
            return Err(ProviderError::Unavailable {
                machine_type: machine_type.to_owned(),
                region: region.to_owned(),
                reason: policy.refusal(region),
            });
        }

        let sku = self
            .list_skus(region)
            .await?
            .into_iter()
            .find(|sku| sku.name == machine_type)
            .ok_or_else(|| ProviderError::Unavailable {
                machine_type: machine_type.to_owned(),
                region: region.to_owned(),
                reason: "the subscription lists no such machine type in this region".to_owned(),
            })?;

        if let Availability::Unavailable { reason } = sku.availability(region) {
            return Err(ProviderError::Unavailable {
                machine_type: machine_type.to_owned(),
                region: region.to_owned(),
                reason,
            });
        }

        let quotas = self.read_quotas(region).await?;
        quotas.require_capacity_for(&sku, region, mode)?;
        Ok((sku, quotas))
    }

    /// The cloud-config a machine boots with, base64-encoded for
    /// `customData`.
    fn cloud_init(&self, request: &ProvisionRequest) -> Result<String, ProviderError> {
        let config = flycod::render(&request.bootstrap)
            .map_err(|_| ProviderError::Malformed("the flycod configuration did not render"))?;
        cloud_init::render(&config, &self.workspace.flycod_installer_url)
    }

    /// The virtual-machine body for one request, asking for spot when the
    /// spec does.
    fn machine_body(
        &self,
        request: &ProvisionRequest,
        sku: &Sku,
    ) -> Result<bodies::VirtualMachine, ProviderError> {
        let id = request.machine;
        let region = &request.spec.region;
        let image_sku = match sku.architecture() {
            Some(CpuArchitecture::Arm64) => IMAGE_SKU_ARM64,
            Some(CpuArchitecture::X8664) => IMAGE_SKU_X64,
            None => {
                return Err(ProviderError::Malformed(
                    "an Azure VM SKU reported no CPU architecture, so no image can be chosen",
                ));
            }
        };

        let spot = request.spec.spot;
        Ok(bodies::VirtualMachine {
            location: region.clone(),
            tags: bodies::MachineTags {
                owner: "flyco",
                session: request.bootstrap.session.to_string(),
                machine: id.to_string(),
            },
            properties: bodies::VirtualMachineProperties {
                priority: spot.then_some("Spot"),
                eviction_policy: spot.then_some("Deallocate"),
                billing_profile: spot.then_some(bodies::BillingProfile { max_price: -1 }),
                hardware_profile: bodies::HardwareProfile {
                    vm_size: request.spec.machine_type.clone(),
                },
                storage_profile: bodies::StorageProfile {
                    image_reference: bodies::ImageReference {
                        publisher: IMAGE_PUBLISHER,
                        offer: IMAGE_OFFER,
                        sku: image_sku,
                        version: "latest",
                    },
                    os_disk: bodies::OsDisk {
                        name: names::os_disk(id),
                        create_option: "FromImage",
                        caching: "ReadWrite",
                        disk_size_gb: request.spec.disk_gib,
                        delete_option: bodies::DETACH,
                        managed_disk: bodies::ManagedDisk {
                            storage_account_type: OS_DISK_TYPE,
                        },
                    },
                    data_disks: Vec::new(),
                },
                os_profile: bodies::OsProfile {
                    computer_name: names::machine(id),
                    admin_username: ADMIN_USERNAME.to_owned(),
                    custom_data: self.cloud_init(request)?,
                    allow_extension_operations: false,
                    linux_configuration: bodies::LinuxConfiguration {
                        disable_password_authentication: true,
                        provision_vm_agent: true,
                        ssh: bodies::SshConfiguration {
                            public_keys: vec![bodies::SshPublicKey {
                                path: format!("/home/{ADMIN_USERNAME}/.ssh/authorized_keys"),
                                key_data: self.workspace.login_key.public_openssh(),
                            }],
                        },
                    },
                },
                network_profile: bodies::NetworkProfile {
                    network_interfaces: vec![bodies::AttachedInterface {
                        id: self.resource_id(
                            "Microsoft.Network/networkInterfaces",
                            &names::network_interface(id),
                        ),
                        properties: bodies::AttachedInterfaceProperties {
                            primary: true,
                            delete_option: bodies::DETACH,
                        },
                    }],
                },
                diagnostics_profile: bodies::DiagnosticsProfile {
                    boot_diagnostics: bodies::BootDiagnostics { enabled: true },
                },
            },
        })
    }

    /// The two per-session network resources, in the order they depend on
    /// each other: the address first, then the interface that holds it.
    async fn create_session_network(
        &mut self,
        id: MachineId,
        region: &str,
    ) -> Result<(), ProviderError> {
        let public_ip = bodies::PublicIpAddress {
            location: region.to_owned(),
            sku: bodies::PublicIpSku {
                name: "Standard",
                tier: "Regional",
            },
            properties: bodies::PublicIpProperties {
                public_ip_address_version: "IPv4",
                public_ip_allocation_method: "Static",
                idle_timeout_in_minutes: 4,
                dns_settings: bodies::DnsSettings {
                    domain_name_label: names::dns_label(id),
                },
            },
        };
        self.send_and_await(
            HttpRequest::new(
                Method::Put,
                self.resource_url(
                    "Microsoft.Network/publicIPAddresses",
                    &names::public_ip(id),
                    api_version::NETWORK,
                ),
            )
            .json_body(&public_ip)?,
        )
        .await?;

        let interface = bodies::NetworkInterface {
            location: region.to_owned(),
            properties: bodies::NetworkInterfaceProperties {
                network_security_group: bodies::ResourceRef {
                    id: self.resource_id(
                        "Microsoft.Network/networkSecurityGroups",
                        &names::nsg(region),
                    ),
                },
                ip_configurations: vec![bodies::IpConfiguration {
                    name: "ipconfig1",
                    properties: bodies::IpConfigurationProperties {
                        primary: true,
                        private_ip_allocation_method: "Dynamic",
                        subnet: bodies::ResourceRef {
                            id: format!(
                                "{}/subnets/{SUBNET_NAME}",
                                self.resource_id(
                                    "Microsoft.Network/virtualNetworks",
                                    &names::vnet(region)
                                )
                            ),
                        },
                        public_ip_address: bodies::AttachedResource {
                            id: self.resource_id(
                                "Microsoft.Network/publicIPAddresses",
                                &names::public_ip(id),
                            ),
                            properties: bodies::DeleteOption {
                                delete_option: bodies::DETACH,
                            },
                        },
                    },
                }],
            },
        };
        self.send_and_await(
            HttpRequest::new(
                Method::Put,
                self.resource_url(
                    "Microsoft.Network/networkInterfaces",
                    &names::network_interface(id),
                    api_version::NETWORK,
                ),
            )
            .json_body(&interface)?,
        )
        .await?;
        Ok(())
    }

    /// The URL of one machine's virtual machine resource.
    fn machine_url(&self, id: MachineId) -> String {
        self.resource_url(
            "Microsoft.Compute/virtualMachines",
            &names::machine(id),
            api_version::COMPUTE,
        )
    }

    /// A `POST` to one of a machine's action endpoints.
    fn action_url(&self, id: MachineId, action: &str) -> String {
        let base = arm::resource_group_scope(self.subscription(), &self.workspace.resource_group);
        format!(
            "{base}/providers/Microsoft.Compute/virtualMachines/{}/{action}?api-version={}",
            names::machine(id),
            api_version::COMPUTE
        )
    }

    async fn post_action(&mut self, id: MachineId, action: &str) -> Result<(), ProviderError> {
        self.send_and_await(HttpRequest::new(Method::Post, self.action_url(id, action)))
            .await
    }

    /// Creates the machine, falling back to on-demand when spot is refused.
    ///
    /// The fallback re-checks quota, because the two markets are funded from
    /// different pools: a machine the spot pool could afford may have no
    /// family quota behind it at all, and re-sending it as on-demand would
    /// trade a clear refusal for an opaque one two minutes later.
    async fn create_machine(
        &mut self,
        id: MachineId,
        body: bodies::VirtualMachine,
        sku: &Sku,
        region: &str,
        quotas: &Quotas,
    ) -> Result<CapacityMode, ProviderError> {
        let url = self.machine_url(id);
        let asked_for_spot = body.properties.is_spot();

        let attempt = self
            .send_and_await(HttpRequest::new(Method::Put, url.clone()).json_body(&body)?)
            .await;

        match attempt {
            Ok(()) if asked_for_spot => Ok(CapacityMode::Spot),
            Ok(()) => Ok(CapacityMode::OnDemand),
            Err(error) if asked_for_spot && spot_unsupported(&error) => {
                quotas.require_capacity_for(sku, region, CapacityMode::OnDemand)?;
                tracing::info!(
                    machine = %id,
                    code = error.code().unwrap_or_default(),
                    "Azure refused spot capacity; retrying the same machine on-demand"
                );
                let on_demand = bodies::VirtualMachine {
                    properties: body.properties.without_spot(),
                    ..body
                };
                self.send_and_await(HttpRequest::new(Method::Put, url).json_body(&on_demand)?)
                    .await?;
                Ok(CapacityMode::OnDemand)
            }
            Err(error) => Err(error),
        }
    }

    /// Provisions a session as a virtual machine: the three gates, the
    /// workspace, the two network resources, then the machine.
    async fn provision_vm(&mut self, request: &ProvisionRequest) -> Result<Machine, ProviderError> {
        let MachineSpec {
            region,
            machine_type,
            ..
        } = &request.spec;
        let id = request.machine;

        let requested = if request.spec.spot {
            CapacityMode::Spot
        } else {
            CapacityMode::OnDemand
        };
        let (sku, quotas) = self.deployable_sku(region, machine_type, requested).await?;
        self.ensure_workspace(region).await?;

        self.create_session_network(id, region).await?;

        let body = self.machine_body(request, &sku)?;
        let capacity_mode = self.create_machine(id, body, &sku, region, &quotas).await?;

        tracing::info!(
            machine = %id,
            machine_type = %machine_type,
            region = %region,
            capacity = ?capacity_mode,
            "provisioned an Azure machine"
        );
        Ok(Machine {
            id,
            native_id: self.resource_id("Microsoft.Compute/virtualMachines", &names::machine(id)),
            runtime: Runtime::Vm,
            region: region.clone(),
            state: MachineState::Running,
            capacity_mode,
            address: Some(names::fqdn(id, region)),
        })
    }

    /// Deallocate, `PATCH` the size, start.
    ///
    /// Always through a deallocation, even from a running machine: a size
    /// the current hardware cluster does not offer needs one anyway, and a
    /// deterministic sequence beats a conditional one that is only sometimes
    /// exercised. The result is read from the operations, never from the
    /// machine — a failed resize leaves the resource reporting the size it
    /// was asked for while still running on the old one.
    async fn resize_vm(
        &mut self,
        machine: &Machine,
        new_machine_type: &str,
    ) -> Result<Machine, ProviderError> {
        self.deployable_sku(&machine.region, new_machine_type, machine.capacity_mode)
            .await?;

        self.post_action(machine.id, "deallocate").await?;

        let patch = bodies::ResizePatch {
            properties: bodies::ResizeProperties {
                hardware_profile: bodies::HardwareProfile {
                    vm_size: new_machine_type.to_owned(),
                },
            },
        };
        self.send_and_await(
            HttpRequest::new(Method::Patch, self.machine_url(machine.id)).json_body(&patch)?,
        )
        .await?;

        self.post_action(machine.id, "start").await?;

        tracing::info!(machine = %machine.id, %new_machine_type, "resized an Azure machine");
        Ok(Machine {
            state: MachineState::Running,
            ..machine.clone()
        })
    }

    /// Deletes the machine and everything `Detach` kept alive.
    ///
    /// In dependency order: the machine, then the interface that referenced
    /// it, then the address the interface held, then the disk. Deleting out
    /// of order fails on a resource that is still referenced.
    async fn destroy_vm(&mut self, machine: &Machine) -> Result<(), ProviderError> {
        let id = machine.id;
        self.delete_and_await(self.machine_url(id)).await?;

        for (provider_path, name, version) in [
            (
                "Microsoft.Network/networkInterfaces",
                names::network_interface(id),
                api_version::NETWORK,
            ),
            (
                "Microsoft.Network/publicIPAddresses",
                names::public_ip(id),
                api_version::NETWORK,
            ),
            (
                "Microsoft.Compute/disks",
                names::os_disk(id),
                api_version::DISKS,
            ),
        ] {
            let url = self.resource_url(provider_path, &name, version);
            self.delete_and_await(url).await?;
        }

        tracing::info!(machine = %id, "destroyed an Azure machine and its resources");
        Ok(())
    }

    // ── Container Apps ──

    /// The ARM id of the environment a region's jobs run in.
    fn environment_id(&self, region: &str) -> String {
        self.resource_id(
            CONTAINER_ENVIRONMENTS_PATH,
            &containers::names::environment(region),
        )
    }

    /// Creates the Container Apps environment a region's jobs run in.
    ///
    /// Lazy and idempotent, exactly as [`ensure_workspace`](Self::ensure_workspace)
    /// is for the network a region's virtual machines share: the `PUT` is
    /// create-or-update, so the second session in a region re-sends the same
    /// body and Azure answers that nothing changed. The alternative — making
    /// it when the account is linked — would build an environment in every
    /// region the subscription allows for the one region a user turns out to
    /// use.
    /// Makes sure the region's managed environment exists and is ready
    /// for a job.
    ///
    /// Read before written: an environment takes minutes to build, and a
    /// `PUT` while that build is under way is refused with
    /// `ManagedEnvironmentOperationInProgress` — the queue's second attempt
    /// at a session would then fail on the first attempt's own work. So
    /// an environment that is being built is waited for, one that is ready
    /// is used as it is, and only a missing or failed one is written.
    ///
    /// # Errors
    ///
    /// [`ProviderError::Rejected`] when the environment is still building
    /// after the driver's polling budget; Azure's own refusals otherwise.
    async fn ensure_environment(&mut self, region: &str) -> Result<(), ProviderError> {
        let url = self.resource_url(
            CONTAINER_ENVIRONMENTS_PATH,
            &containers::names::environment(region),
            api_version::CONTAINER_APPS,
        );
        let mut attempt = 0;
        loop {
            let response = self
                .send(HttpRequest::new(Method::Get, url.clone()))
                .await?;
            if response.status == 404 {
                break;
            }
            if !response.is_success() {
                return Err(refusal(&response));
            }
            let record: containers::ManagedEnvironmentRecord = response.json()?;
            match record.state() {
                containers::EnvironmentState::Ready => return Ok(()),
                containers::EnvironmentState::Failed(state) => {
                    tracing::warn!(
                        region,
                        %state,
                        "the region's Container Apps environment is unusable and is being rewritten"
                    );
                    break;
                }
                containers::EnvironmentState::InProgress(state) => {
                    if attempt == MAX_POLL_ATTEMPTS {
                        return Err(ProviderError::Rejected(format!(
                            "the Container Apps environment for {region} is still `{state}` after \
                             {MAX_POLL_ATTEMPTS} reads"
                        )));
                    }
                    tracing::debug!(region, %state, "waiting for the Container Apps environment");
                    self.timer.sleep(poll_delay(None, attempt)).await;
                    attempt += 1;
                }
            }
        }
        tracing::info!(region, "creating the region's Container Apps environment");
        self.send_and_await(
            HttpRequest::new(Method::Put, url).json_body(&containers::environment_body(region))?,
        )
        .await
    }

    /// The URL of one job.
    fn job_url(&self, job: &str) -> String {
        self.resource_url(CONTAINER_JOBS_PATH, job, api_version::CONTAINER_APPS)
    }

    /// A `POST` to one of a job's action endpoints, or one of its
    /// executions'.
    fn job_action_url(&self, path: &str) -> String {
        let base = arm::resource_group_scope(self.subscription(), &self.workspace.resource_group);
        format!(
            "{base}/providers/{CONTAINER_JOBS_PATH}/{path}?api-version={}",
            api_version::CONTAINER_APPS
        )
    }

    /// Starts a new execution of a job, and answers with Azure's name for
    /// it.
    ///
    /// The name is not flyco's to choose and it is different every time,
    /// which is why it is recorded on the machine rather than derived: it is
    /// what every later call about the *running* half of this machine is
    /// addressed to.
    async fn start_execution(&mut self, job: &str) -> Result<String, ProviderError> {
        let mut started = self.begin_execution(job).await?;
        // A start refused because a write to the job is still under way is
        // joined, not retried: the machine that write is making is this
        // one. Joins spend their own poll budget, so the loop stays bounded.
        for _ in 0..MAX_POLL_ATTEMPTS / (2 * POLLS_PER_INVOCATION) {
            if !matches!(started, Started::Joining) {
                break;
            }
            started = self.join_container(job).await?;
        }
        match started {
            Started::Running(execution) => Ok(execution),
            Started::Pending(follow) => match self.follow_start(follow, MAX_POLL_ATTEMPTS).await? {
                Started::Running(execution) => Ok(execution),
                Started::Pending(_) | Started::Joining => Err(ProviderError::Rejected(format!(
                    "an Azure job execution was still starting after {MAX_POLL_ATTEMPTS} polls"
                ))),
            },
            Started::Joining => Err(ProviderError::Rejected(format!(
                "an Azure job was still being written after {MAX_POLL_ATTEMPTS} polls"
            ))),
        }
    }

    /// Starts a new execution of a job and follows it for one invocation's
    /// polls: the execution's name if it came up, or how to keep following
    /// it if it did not (issue #257).
    ///
    /// A start refused with [`JOB_BUSY_CODE`] means another leg of this
    /// build holds the job's write lock; [`Started::Joining`] is what the
    /// caller turns into a join rather than a second start beside it.
    async fn begin_execution(&mut self, job: &str) -> Result<Started, ProviderError> {
        let response = self
            .send(HttpRequest::new(
                Method::Post,
                self.job_action_url(&format!("{job}/start")),
            ))
            .await?;
        if job_busy(&response) {
            return Ok(Started::Joining);
        }
        if !response.is_success() {
            return Err(refusal(&response));
        }
        let follow = arm::follow(&response)?;
        if follow == Follow::Finished {
            return Self::started_name(&response).map(Started::Running);
        }
        self.follow_start(follow, POLLS_PER_INVOCATION).await
    }

    /// Keeps following a job start, within `budget` polls.
    async fn follow_start(
        &mut self,
        follow: Follow,
        budget: usize,
    ) -> Result<Started, ProviderError> {
        match self.follow_operation(follow, budget).await? {
            Followed::Done(Some(response)) => Self::started_name(&response).map(Started::Running),
            Followed::Done(None) => Err(ProviderError::Malformed(
                "Azure reported the operation that started a job execution but not the \
                 execution, so there is no name to address it by",
            )),
            Followed::Still(follow) => Ok(Started::Pending(follow)),
        }
    }

    /// The execution a job start named, out of the response that describes it.
    fn started_name(response: &HttpResponse) -> Result<String, ProviderError> {
        let started: containers::StartedExecution = response.json()?;
        if started.name.is_empty() {
            return Err(ProviderError::Malformed(
                "Azure started a job execution without naming it",
            ));
        }
        Ok(started.name)
    }

    /// A job's record as ARM sees it right now — what a join reads to
    /// learn whether the write it collided with has landed.
    async fn read_job(&mut self, job: &str) -> Result<containers::JobRecord, ProviderError> {
        let response = self
            .send(HttpRequest::new(Method::Get, self.job_url(job)))
            .await?;
        if !response.is_success() {
            return Err(refusal(&response));
        }
        Ok(response.json()?)
    }

    /// Every execution a job has that is still using compute.
    async fn live_executions(
        &mut self,
        job: &str,
    ) -> Result<Vec<containers::ExecutionRecord>, ProviderError> {
        let response = self
            .send(HttpRequest::new(
                Method::Get,
                self.job_action_url(&format!("{job}/executions")),
            ))
            .await?;
        // A job that is already gone runs nothing: the list a destroy stops
        // over is empty.
        if gone(&response) {
            return Ok(Vec::new());
        }
        if !response.is_success() {
            return Err(refusal(&response));
        }
        let list: containers::ExecutionList = response.json()?;
        Ok(list
            .value
            .into_iter()
            .filter(containers::ExecutionRecord::is_live)
            .collect())
    }

    /// Stops every live execution of a job but the earliest, and answers
    /// with the earliest's name.
    ///
    /// That rule is what two legs of one build both follow: a leg that
    /// started a second execution beside the first sees the same list,
    /// keeps the same earliest, and stops its own — so however the legs
    /// interleave, they converge on one machine.
    ///
    /// # Errors
    ///
    /// Fails only when a job a start just named shows no live execution —
    /// the execution ended between the two calls — and reports a stopped
    /// duplicate it could not stop instead of failing, since one extra
    /// execution still converges the build.
    async fn converge_executions(&mut self, job: &str) -> Result<String, ProviderError> {
        let live = self.live_executions(job).await?;
        let Some(earliest) = live.iter().min_by(|a, b| a.started_before(b)) else {
            return Err(ProviderError::Rejected(format!(
                "the Azure Container Apps job {job} shows no live execution where \
                 a start named one"
            )));
        };
        let keep = earliest.name.clone();
        for record in &live {
            if record.name == keep {
                continue;
            }
            let duplicate = containers::Execution {
                job,
                name: &record.name,
            };
            if let Err(error) = self.stop_execution(&duplicate).await {
                tracing::warn!(
                    %job,
                    execution = %record.name,
                    %error,
                    "could not stop a duplicate Azure Container Apps execution"
                );
            }
        }
        Ok(keep)
    }

    /// Joins a build a sibling leg is carrying out: waits for the job's
    /// write to land, then reports the execution that leg started — or
    /// starts one itself when that leg died before it could.
    ///
    /// A leg reaches here when its own write or start collided with the
    /// write a redelivered sibling leg is still carrying — or a leg that
    /// never got to a start resumes. The join is the contract that keeps
    /// one build to one machine: waiting legs watch the same executions
    /// list the starting leg does, and every leg keeps the earliest
    /// execution it finds there — [`Self::converge_executions`] is what
    /// reports and enforces that at the point a machine is reported.
    async fn join_container(&mut self, job: &str) -> Result<Started, ProviderError> {
        // A start issued over a job still being written is refused the
        // same way the write was, so the job has to settle first.
        let mut settled = false;
        for attempt in 0..POLLS_PER_INVOCATION {
            match self.read_job(job).await?.state() {
                containers::JobState::Ready => {
                    settled = true;
                    break;
                }
                containers::JobState::Writing => {}
                containers::JobState::Failed => {
                    return Err(ProviderError::Rejected(format!(
                        "the write to Azure Container Apps job {job} failed while \
                         a build was waiting to join it"
                    )));
                }
            }
            self.timer.sleep(poll_delay(None, attempt)).await;
        }
        if !settled {
            return Ok(Started::Joining);
        }

        // The execution the first leg's start names is the machine. One
        // still `Processing` is joined by waiting; none at all past the
        // grace polls means that leg died between writing the job and
        // starting it, and this leg stands in.
        for attempt in 0..POLLS_PER_INVOCATION {
            let live = self.live_executions(job).await?;
            if live
                .iter()
                .any(|record| record.properties.status == "Running")
            {
                let earliest = live
                    .iter()
                    .min_by(|a, b| a.started_before(b))
                    .expect("a live execution is running, so the list is not empty");
                return Ok(Started::Running(earliest.name.clone()));
            }
            if live.is_empty() && attempt >= JOIN_GRACE_POLLS {
                return self.begin_execution(job).await;
            }
            self.timer.sleep(poll_delay(None, attempt)).await;
        }
        Ok(Started::Joining)
    }

    /// Stops one execution, leaving the job it belongs to.
    ///
    /// What a container machine's *stop* is. The filesystem goes with the
    /// execution — the daemon has already written the `workdir-patch` on
    /// `SIGTERM` — and the job stays, which is what makes the next
    /// [`start`](CloudProvider::start) a start rather than a provision.
    async fn stop_execution(
        &mut self,
        execution: &containers::Execution<'_>,
    ) -> Result<(), ProviderError> {
        let response = self
            .send(HttpRequest::new(
                Method::Post,
                self.job_action_url(&format!(
                    "{}/executions/{}/stop",
                    execution.job, execution.name
                )),
            ))
            .await?;
        // An execution that is already gone has already stopped — Container
        // Apps answers that with its own 400 "not found" rather than ARM's
        // 404, and either is the state a stop was asking for.
        if gone(&response) {
            return Ok(());
        }
        if !response.is_success() {
            return Err(refusal(&response));
        }
        self.await_operation(response).await.map(drop)
    }

    /// The one gate a container passes, and the size it asked for.
    ///
    /// The region policy, because it refuses an environment's `PUT` exactly
    /// as it refuses a virtual network's, and the size table, because
    /// Container Apps publishes no SKU list to check a name against — a
    /// machine type outside [`containers::Size::OFFERED`] is one flyco never
    /// offered, and attempting it would send Azure a `cpu` and a `memory`
    /// nobody chose.
    async fn deployable_container(
        &mut self,
        region: &str,
        machine_type: &str,
    ) -> Result<containers::Size, ProviderError> {
        let policy = self.region_policy().await?;
        if !policy.allows(region) {
            return Err(ProviderError::Unavailable {
                machine_type: machine_type.to_owned(),
                region: region.to_owned(),
                reason: policy.refusal(region),
            });
        }

        containers::Size::named(machine_type).ok_or_else(|| ProviderError::Unavailable {
            machine_type: machine_type.to_owned(),
            region: region.to_owned(),
            reason: "flyco offers no container of that size on Azure Container Apps".to_owned(),
        })
    }

    /// Provisions a session as one execution of a Container Apps job.
    ///
    /// Three writes deep, the same shape the virtual-machine path has: the
    /// environment the region's jobs share, then this machine's job, then
    /// the execution that is the machine.
    ///
    /// A redelivered provisioning message reaches here while its sibling
    /// leg's `PUT` is still being carried out — the machine row carries no
    /// provider-native id until a leg returns, so the queue cannot suppress
    /// the second delivery that arrives first. Azure refuses that write
    /// with [`JOB_BUSY_CODE`], and the leg joins the build its sibling is
    /// running rather than failing on it.
    async fn provision_container(
        &mut self,
        request: &ProvisionRequest,
    ) -> Result<Provisioning, ProviderError> {
        let id = request.machine;
        let region = &request.spec.region;
        let size = self
            .deployable_container(region, &request.spec.machine_type)
            .await?;

        self.ensure_container_provider().await?;
        self.ensure_environment(region).await?;

        let job = containers::names::job(id);
        let body = containers::job_body(
            id,
            &request.bootstrap,
            region,
            self.environment_id(region),
            size,
        )?;

        // What the machine is before its execution has a name: enough to
        // destroy it by, should the build be given up.
        let pending = Machine {
            id,
            native_id: job.clone(),
            runtime: Runtime::Container,
            region: region.clone(),
            state: MachineState::Provisioning,
            // Container Apps sells no interruptible capacity, so a session
            // that asked for spot holds ordinary capacity and is billed for
            // it. Recorded rather than refused: what the machine actually
            // holds is what the price follows, and there is nothing here for
            // a user to act on.
            capacity_mode: CapacityMode::OnDemand,
            // Nothing dials a container. Its daemon opens the connection to
            // the control plane, and Container Apps gives a job no inbound
            // address at all.
            address: None,
        };

        let response = self
            .send(HttpRequest::new(Method::Put, self.job_url(&job)).json_body(&body)?)
            .await?;
        if job_busy(&response) {
            // A queue redelivery: a sibling leg's write to this same job is
            // still being carried out, and Azure refuses a second write over
            // it. This leg joins that build rather than failing on work its
            // own delivery caused — the continuation that carries on waits
            // the write out and reports the execution the sibling starts.
            tracing::info!(
                machine = %id,
                %job,
                "a sibling leg's write to this job is still under way; joining the build"
            );
            return Ok(Provisioning::Pending {
                machine: pending,
                continuation: Continuation::write(&containers::StartInProgress {
                    job,
                    follow: None,
                })?,
            });
        }
        if !response.is_success() {
            return Err(refusal(&response));
        }
        self.await_operation(response).await?;

        match self.begin_execution(&job).await? {
            Started::Running(_) => {
                let execution = self.converge_executions(&job).await?;
                tracing::info!(
                    machine = %id,
                    machine_type = %request.spec.machine_type,
                    region = %region,
                    %execution,
                    "provisioned an Azure Container Apps execution"
                );
                Ok(Provisioning::Ready(Self::container_running(
                    pending, &execution,
                )))
            }
            Started::Pending(follow) => {
                tracing::info!(
                    machine = %id,
                    machine_type = %request.spec.machine_type,
                    region = %region,
                    "an Azure Container Apps execution is still starting; handing the build back"
                );
                Ok(Provisioning::Pending {
                    machine: pending,
                    continuation: Continuation::write(&containers::StartInProgress {
                        job,
                        follow: Some(follow),
                    })?,
                })
            }
            Started::Joining => Ok(Provisioning::Pending {
                machine: pending,
                continuation: Continuation::write(&containers::StartInProgress {
                    job,
                    follow: None,
                })?,
            }),
        }
    }

    /// Carries on a container build whose execution was still starting — or
    /// joins one whose leg never reached a start.
    async fn resume_container(
        &mut self,
        machine: &Machine,
        continuation: &Continuation,
    ) -> Result<Provisioning, ProviderError> {
        let containers::StartInProgress { job, follow } = continuation.read()?;
        let started = match follow {
            Some(follow) => self.follow_start(follow, POLLS_PER_INVOCATION).await?,
            // The leg that wrote this never reached a start: it found its
            // sibling's write to the job still under way and handed back to
            // join it. Carrying on means joining that build.
            None => self.join_container(&job).await?,
        };
        match started {
            Started::Running(_) => {
                let execution = self.converge_executions(&job).await?;
                tracing::info!(
                    machine = %machine.id,
                    %execution,
                    "an Azure Container Apps execution came up on a resumed build"
                );
                Ok(Provisioning::Ready(Self::container_running(
                    machine.clone(),
                    &execution,
                )))
            }
            Started::Pending(follow) => Ok(Provisioning::Pending {
                machine: machine.clone(),
                continuation: Continuation::write(&containers::StartInProgress {
                    job,
                    follow: Some(follow),
                })?,
            }),
            Started::Joining => Ok(Provisioning::Pending {
                machine: machine.clone(),
                continuation: Continuation::write(&containers::StartInProgress {
                    job,
                    follow: None,
                })?,
            }),
        }
    }

    /// The machine a pending container becomes once its execution is named.
    fn container_running(pending: Machine, execution: &str) -> Machine {
        Machine {
            native_id: containers::Execution::native_id(&pending.native_id, execution),
            state: MachineState::Running,
            ..pending
        }
    }

    /// Moves a container machine to another size: stop, patch, start.
    ///
    /// A `PATCH` rather than a second `PUT` — see [`containers::JobPatch`] —
    /// and a stop that is not optional: a replica's size is fixed for its
    /// lifetime, so the execution running at the old size has to end before
    /// one at the new size can begin. That is also what makes this cheaper
    /// than the virtual-machine resize rather than worse: there is no disk
    /// to detach and re-attach, because there is no disk.
    async fn resize_container(
        &mut self,
        machine: &Machine,
        new_machine_type: &str,
    ) -> Result<Machine, ProviderError> {
        let size = self
            .deployable_container(&machine.region, new_machine_type)
            .await?;
        let execution = containers::Execution::parse(&machine.native_id)?;

        self.stop_execution(&execution).await?;

        let patch = containers::JobPatch {
            properties: containers::JobPatchProperties {
                template: containers::template(size),
            },
        };
        self.send_and_await(
            HttpRequest::new(Method::Patch, self.job_url(execution.job)).json_body(&patch)?,
        )
        .await?;

        let started = self.start_execution(execution.job).await?;
        tracing::info!(
            machine = %machine.id,
            %new_machine_type,
            execution = %started,
            "resized an Azure Container Apps job"
        );
        Ok(Machine {
            native_id: containers::Execution::native_id(execution.job, &started),
            state: MachineState::Running,
            ..machine.clone()
        })
    }

    /// Removes a container machine: the execution first where one is
    /// running, then the job.
    ///
    /// The stop is skipped for a machine already recorded as stopped rather
    /// than sent anyway, because that machine's execution is already gone
    /// and the request would name something that no longer exists.
    async fn destroy_container(&mut self, machine: &Machine) -> Result<(), ProviderError> {
        let job = match containers::Target::parse(&machine.native_id)? {
            containers::Target::Execution(execution) => {
                if machine.state == MachineState::Running {
                    self.stop_execution(&execution).await?;
                }
                execution.job.to_owned()
            }
            // A build given up before its execution was named: whatever the
            // job has started since is stopped, so nothing runs on behind
            // a machine the control plane has forgotten.
            containers::Target::Job(job) => {
                for record in self.live_executions(job).await? {
                    self.stop_execution(&containers::Execution {
                        job,
                        name: &record.name,
                    })
                    .await?;
                }
                job.to_owned()
            }
        };

        self.delete_and_await(self.job_url(&job)).await?;

        tracing::info!(machine = %machine.id, "destroyed an Azure Container Apps job");
        Ok(())
    }

    /// What one region offers, and why everything else was left out.
    ///
    /// The three gates, applied in the order that makes the answer cheapest
    /// and the explanation clearest: the policy can rule out a whole region
    /// without a single SKU being read, and a machine type that is not sold
    /// here is a different sentence from one with no quota.
    ///
    /// The region's containers are on the same menu, after its virtual
    /// machines: one fixed table of sizes ([`containers::Size::OFFERED`])
    /// against one pair of published rates. They are not a cheaper kind of
    /// virtual machine and are never compared with one — curation groups by
    /// runtime, because a machine whose filesystem ends with it is a
    /// different bargain rather than a better price.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] if Azure refuses any of the reads.
    pub async fn region_report(&mut self, region: &str) -> Result<RegionReport, ProviderError> {
        let policy = self.region_policy().await?;
        if !policy.allows(region) {
            return Ok(RegionReport {
                region: region.to_owned(),
                offered: Vec::new(),
                excluded: vec![(
                    region.to_owned(),
                    ExclusionReason::RegionForbidden(policy.refusal(region)),
                )],
            });
        }

        let skus = self.list_skus(region).await?;
        let quotas = self.read_quotas(region).await?;
        let priced = self
            .prices
            .region_prices(&self.transport, &self.clock, &self.timer, region)
            .await?
            .to_vec();
        let storage = self
            .prices
            .storage_pricing(&self.transport, &self.clock, &self.timer, region)
            .await?
            .clone();

        let mut report = RegionReport {
            region: region.to_owned(),
            offered: Vec::new(),
            excluded: Vec::new(),
        };

        for sku in skus {
            match Self::entry_for(&sku, region, &quotas, &priced, &storage) {
                Ok(entry) => report.offered.push(entry),
                Err(reason) => report.excluded.push((sku.name, reason)),
            }
        }

        let container_rates = self
            .prices
            .container_prices(&self.transport, &self.clock, &self.timer, region)
            .await?;
        for size in containers::Size::OFFERED {
            match container_rates {
                Some(rates) => report.offered.push(container_entry(size, region, rates)),
                // A region that publishes no Consumption meters is one where
                // the service is not sold, and flyco will not quote an hour
                // it cannot price.
                None => report
                    .excluded
                    .push((size.machine_type(), ExclusionReason::Unpriced)),
            }
        }
        Ok(report)
    }

    /// One machine type's catalog entry, or the reason it has none.
    fn entry_for(
        sku: &Sku,
        region: &str,
        quotas: &Quotas,
        priced: &[(String, pricing::MachinePrices)],
        storage: &StoragePricing,
    ) -> Result<MachineCatalogEntry, ExclusionReason> {
        if let Availability::Unavailable { reason } = sku.availability(region) {
            return Err(ExclusionReason::NotOffered(reason));
        }

        // Either market is enough to put a machine on the menu, but a spot
        // price is only quoted when the spot pool could actually fund it.
        let on_demand = quotas.require_capacity_for(sku, region, CapacityMode::OnDemand);
        let spot = quotas.require_capacity_for(sku, region, CapacityMode::Spot);
        if let (Err(on_demand), Err(_)) = (&on_demand, &spot) {
            return Err(ExclusionReason::NoQuota(on_demand.to_string()));
        }

        let (vcpus, memory_mib) = sku
            .vcpus()
            .zip(sku.memory_mib())
            .ok_or(ExclusionReason::Unreadable)?;
        // A SKU without a quota family or an architecture is one flyco can
        // neither place in the line-up nor pick an image for, so it is left
        // off the menu with a reason rather than guessed at.
        let lineage = sku.lineage().ok_or(ExclusionReason::Unreadable)?;
        let published = priced
            .iter()
            .find(|(name, _)| *name == sku.name)
            .map(|(_, prices)| *prices)
            .ok_or(ExclusionReason::Unpriced)?;
        let on_demand_hourly = published.on_demand.ok_or(ExclusionReason::Unpriced)?;

        Ok(MachineCatalogEntry {
            // Stamped by the control plane, which knows the row.
            account: None,
            provider: CloudProviderKind::Azure,
            region: region.to_owned(),
            machine_type: sku.name.clone(),
            runtime: Runtime::Vm,
            free_grant: None,
            os: OsFamily::Linux,
            capacity: Some(MachineCapacity { vcpus, memory_mib }),
            lineage: Some(lineage),
            pricing: MachinePricing::Metered {
                on_demand_hourly,
                spot_hourly: spot.is_ok().then_some(published.spot).flatten(),
                // Azure bills by the second with no floor; the field exists
                // for providers that impose one, such as EC2 Mac's 24-hour
                // Apple-licence minimum.
                minimum: None,
                storage: storage.clone(),
            },
        })
    }

    /// What Azure's own meter says this subscription has been billed this
    /// billing month.
    ///
    /// Cost Management is the authority: a total flyco assembled from its
    /// own machine records would miss the storage, egress and support
    /// charges on the same invoice, and would be a second opinion about a
    /// number the provider already publishes. The window reported back is
    /// the one the query covered, so the amount and the period beside it
    /// cannot disagree — see [`costs`].
    ///
    /// `now_unix` is supplied rather than read: the provider crate has a
    /// monotonic clock for token expiry and deliberately no wall clock, and
    /// a billing month is a wall-clock fact.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] if Azure refuses the query, if the result
    /// is not one this driver can read, or if the subscription is metered
    /// in a currency flyco does not account in.
    pub async fn billing_period_cost(
        &mut self,
        now_unix: u64,
    ) -> Result<CloudSpend, ProviderError> {
        let url = arm::subscription_url(
            self.subscription(),
            "providers/Microsoft.CostManagement/query",
            api_version::COST_MANAGEMENT,
        );
        let request = HttpRequest::new(Method::Post, url).json_body(&CostQuery::month_to_date())?;

        let response = self.send(request).await?;
        if !response.is_success() {
            return Err(refusal(&response));
        }

        // A `204` is Cost Management saying there is nothing in the period,
        // which is a metered zero rather than an unreadable answer.
        let properties = if response.status == 204 {
            costs::CostProperties::default()
        } else {
            response.json::<CostResult>()?.properties
        };
        costs::spend_of(&properties, now_unix)
    }
}

/// One container size's catalog entry.
///
/// Infallible, unlike [`AzureProvider::entry_for`]: the size table is
/// flyco's own and every entry in it is deployable wherever the service is
/// sold, so the only way a container is *not* on the menu is the region
/// publishing no price — which the caller has already established by
/// holding [`pricing::ContainerPrices`] at all.
fn container_entry(
    size: containers::Size,
    region: &str,
    prices: pricing::ContainerPrices,
) -> MachineCatalogEntry {
    MachineCatalogEntry {
        // Stamped by the control plane, which knows the row.
        account: None,
        provider: CloudProviderKind::Azure,
        region: region.to_owned(),
        machine_type: size.machine_type(),
        runtime: Runtime::Container,
        // Per subscription and per calendar month, drawn on by every
        // container this account runs rather than by this machine type.
        free_grant: Some(CONTAINER_APPS_FREE_GRANT),
        os: OsFamily::Linux,
        capacity: Some(MachineCapacity {
            vcpus: size.vcpus(),
            memory_mib: size.memory_mib(),
        }),
        lineage: Some(size.lineage()),
        pricing: MachinePricing::Metered {
            on_demand_hourly: prices.hourly(size.vcpus(), size.memory_gib()),
            // Container Apps has no interruptible market at all, so there is
            // no second price to quote — not one that happens to equal the
            // first.
            spot_hourly: None,
            // Nothing is billed before an execution runs: the job costs
            // nothing to exist.
            minimum: None,
            // A replica's filesystem is part of the replica and is billed
            // through its memory and cores, so every GiB of it costs
            // nothing extra. That is what the provider bills, not a
            // discount flyco is applying.
            storage: StoragePricing::PerGibHourly { rate: Usd::ZERO },
        },
    }
}

/// Whether a response is [`JOB_BUSY_CODE`]: a write refused because an
/// earlier write to the same job is still being carried out.
fn job_busy(response: &HttpResponse) -> bool {
    response.status == 409
        && ErrorBody::of(response).is_some_and(|error| error.code == JOB_BUSY_CODE)
}

/// Whether a refused response means the resource it named no longer exists.
///
/// ARM's own answer is a 404. The Container Apps job endpoints answer a
/// 400 instead, with a body that says the execution was "not found" in
/// prose rather than in an error code — so both shapes are read. A destroy
/// or stop that meets either is already done: the machine it was sent to
/// remove is gone, which is the state the caller was asking for.
fn gone(response: &HttpResponse) -> bool {
    response.status == 404 || (response.status == 400 && response.body_text().contains("not found"))
}

/// Turns a refused response into an error that keeps its code.
fn refusal(response: &HttpResponse) -> ProviderError {
    ErrorBody::of(response).map_or_else(
        || {
            ProviderError::Rejected(format!(
                "Azure answered HTTP {}: {}",
                response.status,
                response.body_text()
            ))
        },
        |error| ProviderError::Refused {
            code: error.code,
            message: error.message,
        },
    )
}

/// Whether a refusal means "the same machine, without the spot fields".
#[must_use]
pub fn spot_unsupported(error: &ProviderError) -> bool {
    error
        .code()
        .is_some_and(|code| SPOT_UNSUPPORTED_CODES.contains(&code))
}

impl<T: HttpTransport, C: MonotonicClock, K: Timer> CloudProvider for AzureProvider<T, C, K> {
    async fn catalog(&mut self) -> Result<Vec<MachineCatalogEntry>, ProviderError> {
        let mut entries = Vec::new();
        for region in self.catalog_regions().await? {
            let report = self.region_report(&region).await?;
            tracing::debug!(
                %region,
                offered = report.offered.len(),
                excluded = report.excluded.len(),
                "read an Azure region's catalog"
            );
            entries.extend(report.offered);
        }
        Ok(entries)
    }

    /// Provisions a virtual machine or a managed container, as the spec
    /// asks.
    ///
    /// The two runtimes disagree about the first thing a caller would want
    /// to know — whether the filesystem survives a stop — so a request whose
    /// spec and whose daemon bootstrap name different ones is refused rather
    /// than half-honoured. Satisfying it would put `runtime = "vm"` in a
    /// container's configuration, and that daemon would let the platform
    /// take the working tree away without writing the patch that is the only
    /// copy of it.
    async fn provision(
        &mut self,
        request: &ProvisionRequest,
    ) -> Result<Provisioning, ProviderError> {
        if request.spec.runtime != request.bootstrap.runtime {
            return Err(ProviderError::Malformed(
                "this request's machine spec and daemon bootstrap disagree about \
                 whether the machine is a virtual machine or a container",
            ));
        }
        match request.spec.runtime {
            Runtime::Vm => self.provision_vm(request).await.map(Provisioning::Ready),
            Runtime::Container => self.provision_container(request).await,
        }
    }

    async fn resume(
        &mut self,
        machine: &Machine,
        continuation: &Continuation,
    ) -> Result<Provisioning, ProviderError> {
        match machine.runtime {
            Runtime::Vm => Err(ProviderError::Malformed(
                "an Azure virtual machine is built in one call and has nothing to resume",
            )),
            Runtime::Container => self.resume_container(machine, continuation).await,
        }
    }

    async fn resize(
        &mut self,
        machine: &Machine,
        new_machine_type: &str,
    ) -> Result<Machine, ProviderError> {
        match machine.runtime {
            Runtime::Vm => self.resize_vm(machine, new_machine_type).await,
            Runtime::Container => self.resize_container(machine, new_machine_type).await,
        }
    }

    /// Releases compute: a deallocation on a virtual machine, and the end of
    /// the execution on a container.
    ///
    /// The two are the same request to the session and different requests to
    /// Azure. Which one it is cannot be read off the machine's identifier —
    /// it is [`Machine::runtime`], which the row already carries.
    async fn deallocate(&mut self, machine: &Machine) -> Result<(), ProviderError> {
        match machine.runtime {
            Runtime::Vm => self.post_action(machine.id, "deallocate").await,
            Runtime::Container => {
                let execution = containers::Execution::parse(&machine.native_id)?;
                self.stop_execution(&execution).await
            }
        }
    }

    /// Puts a machine back on compute.
    ///
    /// A virtual machine starts on the disk it kept. A container starts as a
    /// *new execution* of the job it kept, on an empty filesystem, which is
    /// why its `native_id` changes here: the job is the same and the replica
    /// is not.
    async fn start(&mut self, machine: &Machine) -> Result<Machine, ProviderError> {
        match machine.runtime {
            Runtime::Vm => {
                self.post_action(machine.id, "start").await?;
                Ok(Machine {
                    state: MachineState::Running,
                    ..machine.clone()
                })
            }
            Runtime::Container => {
                let job = containers::Execution::parse(&machine.native_id)?.job;
                let execution = self.start_execution(job).await?;
                tracing::info!(
                    machine = %machine.id,
                    %execution,
                    "started a new Azure Container Apps execution"
                );
                Ok(Machine {
                    native_id: containers::Execution::native_id(job, &execution),
                    state: MachineState::Running,
                    ..machine.clone()
                })
            }
        }
    }

    async fn destroy(&mut self, machine: &Machine) -> Result<(), ProviderError> {
        match machine.runtime {
            Runtime::Vm => self.destroy_vm(machine).await,
            Runtime::Container => self.destroy_container(machine).await,
        }
    }
}
