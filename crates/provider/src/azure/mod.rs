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

pub mod arm;
pub mod auth;
pub mod bodies;
pub mod costs;
pub mod policy;
pub mod pricing;
pub mod skus;

#[cfg(test)]
mod tests;

use flyco_core::machine::{
    CloudProviderKind, CpuArchitecture, MachineCapacity, MachineCatalogEntry, MachinePricing,
    MachineSpec, MachineState, OsFamily, StoragePricing,
};
use flyco_core::{CloudSpend, MachineId};

use crate::clock::{MonotonicClock, SystemClock, SystemTimer, Timer};
use crate::http::{HttpRequest, HttpResponse, HttpTransport, Method};
use crate::login_key::LoginKey;
use crate::polling::{MAX_POLL_ATTEMPTS, poll_delay};
use crate::{
    CapacityMode, CloudProvider, LiveTransport, Machine, ProviderError, ProvisionRequest,
    cloud_init, flycod,
};

use arm::{ErrorBody, Follow, OperationBody, OperationStatus, api_version};
use auth::{ServicePrincipal, TokenCache};
use costs::{CostQuery, CostResult};
use policy::{AssignmentPage, RegionPolicy};
use pricing::PriceCatalog;
use skus::{Availability, Quotas, Sku, SkuPage, UsagePage};

/// Driver name, as it appears in [`ProviderError::Unsupported`].
pub const PROVIDER: &str = "azure";

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
        self.await_operation(&response).await
    }

    /// Follows an asynchronous operation to a terminal state.
    ///
    /// `Azure-AsyncOperation` is preferred over `Location` and the terminal
    /// set is exactly `{Succeeded, Failed, Canceled}` — anything else means
    /// keep polling, including in-flight values a resource provider invents.
    /// The operation's status is what is trusted, never the resource body: a
    /// failed resize leaves the machine reporting the size it was asked for
    /// while still running on the old one.
    async fn await_operation(&mut self, accepted: &HttpResponse) -> Result<(), ProviderError> {
        let mut follow = arm::follow(accepted)?;

        for attempt in 0..MAX_POLL_ATTEMPTS {
            let (url, retry_after, by_status) = match &follow {
                Follow::Finished => return Ok(()),
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
                    return Ok(());
                }
                return Err(refusal(&polled));
            }

            if !polled.is_success() {
                return Err(refusal(&polled));
            }

            let body: OperationBody = polled.json()?;
            match OperationStatus::parse(&body.status) {
                OperationStatus::Succeeded => return Ok(()),
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

        Err(ProviderError::Rejected(format!(
            "an Azure operation was still running after {MAX_POLL_ATTEMPTS} polls"
        )))
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
        Ok(location)
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

    /// What one region offers, and why everything else was left out.
    ///
    /// The three gates, applied in the order that makes the answer cheapest
    /// and the explanation clearest: the policy can rule out a whole region
    /// without a single SKU being read, and a machine type that is not sold
    /// here is a different sentence from one with no quota.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] if Azure refuses any of the three reads.
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

    async fn provision(&mut self, request: &ProvisionRequest) -> Result<Machine, ProviderError> {
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
    async fn resize(
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

    async fn deallocate(&mut self, machine: &Machine) -> Result<(), ProviderError> {
        self.post_action(machine.id, "deallocate").await
    }

    async fn start(&mut self, machine: &Machine) -> Result<Machine, ProviderError> {
        self.post_action(machine.id, "start").await?;
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
    async fn destroy(&mut self, machine: &Machine) -> Result<(), ProviderError> {
        let id = machine.id;
        self.send_and_await(HttpRequest::new(Method::Delete, self.machine_url(id)))
            .await?;

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
            self.send_and_await(HttpRequest::new(Method::Delete, url))
                .await?;
        }

        tracing::info!(machine = %id, "destroyed an Azure machine and its resources");
        Ok(())
    }
}
