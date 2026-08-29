//! The AWS driver.
//!
//! Plain signed HTTPS against EC2 and four smaller services, so the whole of
//! it runs inside the Cloudflare Worker. Nothing here is an SDK: a request is
//! an [`HttpRequest`] like every other driver's, signed by [`sigv4`] and sent
//! through the same [`HttpTransport`].
//!
//! # What is different from Azure, and why
//!
//! The shape is deliberately the same — three gates, spot by default with an
//! on-demand fallback, a disk that survives its machine, an explicit
//! teardown in dependency order — but three things genuinely differ and are
//! not papered over:
//!
//! * **There is no operation resource.** EC2 answers a mutating call with
//!   the instance's *previous* state (`StopInstances` says `stopping`), so
//!   every call is followed by reading [`ec2::InstanceState`] until it
//!   settles. The settled state is what is trusted, never a read-back of the
//!   attribute that was set — a `ModifyInstanceAttribute` that failed leaves
//!   the instance running on its old type with no error to be found later.
//! * **Two services publish the price.** On-demand comes from the Price List
//!   API and spot from EC2 itself; see [`pricing`].
//! * **Quota states a limit but not usage.** Service Quotas publishes the
//!   ceiling; what is under it is counted from the account's own running
//!   instances. See [`quotas`].
//!
//! # The three gates
//!
//! 1. **Region access** ([`regions`]) — every region introduced since 2019
//!    is disabled until the account opts in, and a disabled region's
//!    endpoint answers `AuthFailure` naming neither the region nor the
//!    reason. This is the direct analogue of Azure's deployment policy: a
//!    property of the account that the catalog says nothing about.
//! 2. **Availability** ([`ec2::DescribeInstanceTypeOfferings`]) — what this
//!    region actually offers, which is a strict subset of what EC2 sells.
//! 3. **Quota** ([`quotas`]) — and, exactly as on Azure, **spot draws on a
//!    different pool**: `All Standard … Spot Instance Requests` is a
//!    separate limit from `Running On-Demand Standard … instances`, so a
//!    machine the on-demand pool cannot fund is often perfectly runnable as
//!    spot.
//!
//! # Spot, and what survives an interruption
//!
//! Spot is requested by default, as a **persistent** request whose
//! interruption behaviour is **stop**. Both halves matter: `stop` is what
//! keeps the EBS root volume — `terminate`, the default, deletes the
//! session's work — and `persistent` is what lets the request bring the
//! machine back rather than expiring with it. On a refusal that means "not
//! on the spot market", the identical body is re-sent without its market
//! options, after re-checking the on-demand pool, and the capacity actually
//! obtained is recorded on the [`Machine`] because that is what the price
//! follows.
//!
//! # Nothing is orphaned
//!
//! The root volume is created with `DeleteOnTermination: false`, which is
//! this driver's `Detach`, and the address is an **elastic IP** rather than
//! an auto-assigned one — an auto-assigned address is lost on stop, and a
//! session that comes back from a spot interruption at a different address
//! is a session nothing can find. Both therefore outlive the instance, and
//! [`AwsProvider::destroy`] releases them explicitly, in dependency order.
//!
//! `clippy::future_not_send` is allowed across this module for the reason it
//! is in `azure::pricing`: `Send`-ness follows from the concrete transport —
//! the deployed one is a unit struct and the Worker is single-threaded — while
//! the recorded transport is deliberately not `Sync`, and bounding `T: Sync`
//! here would forbid the double the whole driver is tested against.
#![expect(clippy::future_not_send, reason = "see the module documentation")]

pub mod costs;
pub mod ec2;
pub mod identity;
pub mod image;
pub mod pricing;
pub mod query;
pub mod quotas;
pub mod regions;
pub mod sigv4;

#[cfg(test)]
mod tests;

use flyco_core::machine::{
    CloudProviderKind, MachineCapacity, MachineCatalogEntry, MachinePricing, MachineSpec,
    MachineState, OsFamily,
};
use flyco_core::{CloudSpend, MachineId};
use serde::Serialize;

use crate::clock::{MonotonicClock, SystemClock, SystemTimer, SystemWallClock, Timer, WallClock};
use crate::http::{HttpRequest, HttpResponse, HttpTransport, Method};
use crate::polling::{MAX_POLL_ATTEMPTS, poll_delay};
use crate::{
    CapacityMode, CloudProvider, Machine, ProviderError, ProvisionRequest, ZenwaveTransport,
    cloud_init, flycod,
};

use ec2::InstanceState;
use pricing::{MachinePrices, PriceCatalog};
use quotas::{Market, Quotas};
use regions::RegionAccess;
use sigv4::{AccessKey, Scope};

/// Driver name, as it appears in [`ProviderError::Unsupported`].
pub const PROVIDER: &str = "aws";

/// Regions a catalog covers when the caller names none.
///
/// Deliberately a short, overridable starting point rather than every region
/// the account has enabled: a catalog is five reads per region and an
/// ordinary account has seventeen enabled without ever opting into anything,
/// so "everything enabled" would be eighty-five calls for a menu nobody
/// reads. Unlike Azure — where the subscription's own policy names the set —
/// AWS's opt-in status is a *veto* rather than a list, so it narrows this
/// rather than replacing it.
pub const DEFAULT_CANDIDATE_REGIONS: [&str; 3] = ["us-west-2", "eu-west-1", "ap-southeast-1"];

/// Name of the inbound SSH rule's port.
pub const SSH_PORT: u16 = 22;

/// Where the workspace security group admits SSH from.
pub const SSH_SOURCE: &str = "0.0.0.0/0";

/// Storage tier of the root volume.
pub const ROOT_VOLUME_TYPE: &str = "gp3";

/// Tag every flyco resource carries, so an account shared with other work
/// stays legible.
pub const OWNER_TAG: &str = "owner";

/// Tag naming the session a resource serves.
pub const SESSION_TAG: &str = "flyco-session";

/// Media type that selects the AWS JSON-RPC protocol.
///
/// The version is part of the type and is what chooses how the request is
/// read, so `application/json` is refused outright by every service that
/// speaks it.
pub const JSON_RPC_MEDIA_TYPE: &str = "application/x-amz-json-1.1";

/// Tag naming the machine a resource belongs to.
///
/// This is what an elastic IP is found by at teardown: an association is
/// gone the moment its instance terminates, and a tag is not.
pub const MACHINE_TAG: &str = "flyco-machine";

/// The minimum billing commitment an EC2 Mac carries, in hours.
///
/// Apple's macOS licence requires a 24-hour minimum allocation of the
/// dedicated host a Mac instance runs on, so an hour of `mac2.metal` is
/// billed as a day. A budget told the hourly rate and nothing else would be
/// wrong by roughly sixteen dollars, which is why the flag is on the catalog
/// entry rather than in a note somewhere.
///
/// The entry exists even though [`AwsProvider::provision`] refuses one: what
/// the catalog is *for* is telling an agent what a machine would cost before
/// it asks for it, and an entry silently dropped would leave the number
/// unavailable rather than merely unbuyable. The refusal names the reason.
pub const MAC_MINIMUM_BILLING_HOURS: u32 = 24;

/// The refusals that mean "ask again without the market options".
///
/// Each is EC2 saying something about the *spot market* rather than about
/// the machine: no interruptible capacity for this type right now, the
/// account's spot request limit, a bid the market has moved past, and the
/// blanket refusal EC2 returns for a market configuration an instance type
/// does not support. Everything else — a bad AMI, a missing permission, an
/// unusable subnet — is a genuine failure, and retrying it on-demand would
/// turn one clear error into two.
pub const SPOT_UNSUPPORTED_CODES: [&str; 4] = [
    "InsufficientInstanceCapacity",
    "MaxSpotInstanceCountExceeded",
    "SpotMaxPriceTooLow",
    "Unsupported",
];

/// Suffix EC2 puts on the architecture of an instance type that runs macOS.
///
/// `arm64_mac` and `x86_64_mac`, which is the honest signal that a type is
/// not a Linux machine — its name is not (`mac2.metal` says nothing an
/// arbitrary future family would also say).
pub const MAC_ARCHITECTURE_SUFFIX: &str = "_mac";

/// The operating system an instance type runs, from its own architecture.
#[must_use]
pub fn os_family(architectures: &[String]) -> OsFamily {
    if architectures
        .iter()
        .any(|architecture| architecture.ends_with(MAC_ARCHITECTURE_SUFFIX))
    {
        return OsFamily::MacOs;
    }
    OsFamily::Linux
}

/// The names every flyco resource in one account answers to.
///
/// Derived from the machine id rather than allocated, so a name is
/// recoverable without a column to store it in and two callers cannot
/// disagree. The security group carries its region because a group belongs
/// to one VPC, which belongs to one region.
pub mod names {
    use flyco_core::MachineId;

    /// The workspace security group in one region.
    #[must_use]
    pub fn security_group(region: &str) -> String {
        format!("flyco-{region}-sg")
    }

    /// A machine's `Name` tag, which is what the console shows.
    #[must_use]
    pub fn machine(id: MachineId) -> String {
        format!("flyco-{id}")
    }
}

/// Why an instance type is not on offer.
///
/// Named rather than dropped, because the answers ask the user for
/// different things: enable a region, pick another type, or ask for a quota
/// increase.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExclusionReason {
    /// The account has not enabled the region at all.
    #[error("{0}")]
    RegionForbidden(String),
    /// EC2 does not offer this instance type in this region.
    #[error("this instance type is not offered here: {0}")]
    NotOffered(String),
    /// Neither the on-demand pool nor the spot pool has room for it.
    #[error("{0}")]
    NoQuota(String),
    /// The Price List publishes no Linux on-demand price for it, so flyco
    /// cannot tell the user what an hour would cost.
    #[error("no published price")]
    Unpriced,
    /// EC2 described an instance type without a usable size.
    #[error("EC2 reported this instance type without a size")]
    Unreadable,
}

/// What one region offers, and why everything else was left out.
#[derive(Debug, Clone)]
pub struct RegionReport {
    /// The region this covers.
    pub region: String,
    /// The instance types a session can actually be started on.
    pub offered: Vec<MachineCatalogEntry>,
    /// Everything else, paired with the reason. When the region itself is
    /// unavailable this holds one entry naming the region rather than one
    /// per instance type: the answer is the same for all of them.
    pub excluded: Vec<(String, ExclusionReason)>,
}

/// The parts of an AWS account that are configuration rather than
/// credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsWorkspace {
    /// The user's own EC2 key pair, for a break-glass login.
    ///
    /// Optional, unlike Azure's — EC2 creates an instance perfectly well
    /// without one, and a machine with no key is a machine nobody can log
    /// into out of band, which is a legitimate choice. It is the *user's*
    /// key pair either way: flyco never holds a private key for a machine it
    /// provisions, and a key pair lives in their account, not in flyco's.
    pub key_name: Option<String>,
    /// Where a machine fetches `flycod` from on first boot.
    pub flycod_installer_url: String,
    /// Regions [`AwsProvider::catalog`] reports on.
    ///
    /// Empty — the default — means [`DEFAULT_CANDIDATE_REGIONS`], narrowed
    /// to the ones the account has actually enabled. A named region the
    /// account has not enabled is dropped rather than attempted.
    pub regions: Vec<String>,
}

impl AwsWorkspace {
    /// A workspace with no break-glass key, installing `flycod` from
    /// [`cloud_init::DEFAULT_FLYCOD_INSTALLER_URL`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            key_name: None,
            flycod_installer_url: cloud_init::DEFAULT_FLYCOD_INSTALLER_URL.to_owned(),
            regions: Vec::new(),
        }
    }

    /// Creates machines with the user's own EC2 key pair authorized.
    #[must_use]
    pub fn with_key_pair(mut self, key_name: impl Into<String>) -> Self {
        self.key_name = Some(key_name.into());
        self
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

impl Default for AwsWorkspace {
    fn default() -> Self {
        Self::new()
    }
}

/// The one-time network a region's machines share.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RegionNetwork {
    region: String,
    subnet_id: String,
    security_group_id: String,
}

/// The AWS driver.
///
/// Generic over its transport, clocks and timer so the whole of it is
/// testable against recorded exchanges — including the signature, which is
/// only deterministic because the signing instant comes from a
/// [`WallClock`] rather than from the host.
#[derive(Debug)]
pub struct AwsProvider<T = ZenwaveTransport, C = SystemClock, K = SystemTimer, W = SystemWallClock>
{
    transport: T,
    clock: C,
    timer: K,
    wall_clock: W,
    key: AccessKey,
    workspace: AwsWorkspace,
    prices: PriceCatalog,
    region_access: Option<RegionAccess>,
    network: Option<RegionNetwork>,
}

impl AwsProvider {
    /// The driver as it is deployed: zenwave, the host clocks, a real timer.
    #[must_use]
    pub fn new(key: AccessKey, workspace: AwsWorkspace) -> Self {
        Self::with_parts(
            ZenwaveTransport::new(),
            SystemClock::new(),
            SystemTimer::new(),
            SystemWallClock::new(),
            key,
            workspace,
        )
    }
}

impl<T: HttpTransport, C: MonotonicClock, K: Timer, W: WallClock> AwsProvider<T, C, K, W> {
    /// The driver over an explicit transport, clocks and timer.
    pub const fn with_parts(
        transport: T,
        clock: C,
        timer: K,
        wall_clock: W,
        key: AccessKey,
        workspace: AwsWorkspace,
    ) -> Self {
        Self {
            transport,
            clock,
            timer,
            wall_clock,
            key,
            workspace,
            prices: PriceCatalog::new(),
            region_access: None,
            network: None,
        }
    }

    /// The transport this driver sends through.
    ///
    /// Exposed so a test can read back exactly what was put on the wire,
    /// signature included.
    pub const fn transport(&self) -> &T {
        &self.transport
    }

    /// The timer this driver waits on, for the same reason.
    pub const fn timer(&self) -> &K {
        &self.timer
    }

    /// The instant every signature in this operation is made at.
    fn now(&self) -> u64 {
        self.wall_clock.unix_seconds()
    }

    /// Sends one signed EC2 call and refuses anything that is not a success.
    async fn ec2_call<B: Serialize>(
        &self,
        region: &str,
        action: &'static str,
        body: &B,
    ) -> Result<HttpResponse, ProviderError> {
        let response =
            send_ec2(&self.transport, &self.key, region, action, body, self.now()).await?;
        if response.is_success() {
            Ok(response)
        } else {
            Err(ec2::refusal(&response))
        }
    }

    /// Sends one signed EC2 call and decodes the answer.
    async fn ec2<B: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        region: &str,
        action: &'static str,
        body: &B,
    ) -> Result<R, ProviderError> {
        ec2::decode(&self.ec2_call(region, action, body).await?)
    }

    /// Sends one signed JSON-RPC call to a service that speaks it.
    async fn json_rpc<B: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        endpoint: &str,
        scope: Scope<'_>,
        target: &str,
        body: &B,
    ) -> Result<R, ProviderError> {
        let request = json_rpc_request(endpoint, target, body)?;
        let signed = sigv4::sign(request, &self.key, scope, self.now())?;
        let response = self.transport.send(signed).await?;
        if !response.is_success() {
            return Err(json_refusal(&response));
        }
        Ok(response.json()?)
    }

    /// Which regions the account may deploy into, read once per driver.
    ///
    /// Cached for the driver's lifetime rather than per call, for the reason
    /// Azure caches its policy: a region's opt-in status changes on a human
    /// timescale and a driver lives for one operation.
    async fn region_access(&mut self) -> Result<RegionAccess, ProviderError> {
        if let Some(access) = &self.region_access {
            return Ok(access.clone());
        }

        // Signed against a region every account has enabled, because the
        // question being asked is which regions are enabled.
        let response: ec2::DescribeRegionsResponse = self
            .ec2(
                identity::REGION,
                "DescribeRegions",
                &ec2::DescribeRegions { all_regions: true },
            )
            .await?;

        let access = RegionAccess::from_response(&response);
        tracing::debug!(
            enabled = access.enabled().len(),
            "read which regions this AWS account has enabled"
        );
        self.region_access = Some(access.clone());
        Ok(access)
    }

    /// The regions a catalog covers.
    async fn catalog_regions(&mut self) -> Result<Vec<String>, ProviderError> {
        let access = self.region_access().await?;
        let asked = if self.workspace.regions.is_empty() {
            DEFAULT_CANDIDATE_REGIONS
                .iter()
                .map(|region| (*region).to_owned())
                .collect()
        } else {
            self.workspace.regions.clone()
        };

        Ok(asked
            .into_iter()
            .filter(|region| access.allows(region))
            .collect())
    }

    /// Every instance type EC2 describes for one region, with its shape.
    async fn list_instance_types(
        &self,
        region: &str,
    ) -> Result<Vec<ec2::InstanceTypeInfo>, ProviderError> {
        let mut collected = Vec::new();
        let mut next = None;
        loop {
            let page: ec2::DescribeInstanceTypesResponse = self
                .ec2(
                    region,
                    "DescribeInstanceTypes",
                    &ec2::DescribeInstanceTypes {
                        max_results: 100,
                        next_token: next,
                    },
                )
                .await?;
            collected.extend(page.instance_type_set.item);
            next = page.next_token.filter(|token| !token.is_empty());
            if next.is_none() {
                return Ok(collected);
            }
        }
    }

    /// The instance types this region actually offers.
    async fn list_offerings(&self, region: &str) -> Result<Vec<String>, ProviderError> {
        let mut collected = Vec::new();
        let mut next = None;
        loop {
            let page: ec2::DescribeInstanceTypeOfferingsResponse = self
                .ec2(
                    region,
                    "DescribeInstanceTypeOfferings",
                    &ec2::DescribeInstanceTypeOfferings {
                        location_type: "region",
                        filter: vec![ec2::Filter::is("location", region)],
                        max_results: 1_000,
                        next_token: next,
                    },
                )
                .await?;
            collected.extend(
                page.instance_type_offering_set
                    .item
                    .into_iter()
                    .map(|offering| offering.instance_type),
            );
            next = page.next_token.filter(|token| !token.is_empty());
            if next.is_none() {
                return Ok(collected);
            }
        }
    }

    /// One region's quotas, and how much of them the account is spending.
    ///
    /// Two reads because AWS splits what Azure publishes together: Service
    /// Quotas states the limit, and the instances already running state what
    /// is under it.
    async fn read_quotas(
        &self,
        region: &str,
        types: &[ec2::InstanceTypeInfo],
    ) -> Result<Quotas, ProviderError> {
        let mut limits = Vec::new();
        let mut next = None;
        loop {
            let page: quotas::QuotaPage = self
                .json_rpc(
                    &ec2::endpoint(quotas::SERVICE, region),
                    Scope {
                        region,
                        service: quotas::SERVICE,
                    },
                    quotas::LIST_TARGET,
                    &quotas::ListServiceQuotas {
                        service_code: quotas::EC2_SERVICE_CODE,
                        max_results: 100,
                        next_token: next,
                    },
                )
                .await?;
            limits.extend(page.quotas);
            next = page.next_token.filter(|token| !token.is_empty());
            if next.is_none() {
                break;
            }
        }

        let running = self.list_running(region).await?;
        let spending: Vec<(Market, &ec2::InstanceTypeInfo)> = running
            .iter()
            .filter_map(|instance| {
                let info = types
                    .iter()
                    .find(|info| info.instance_type == instance.instance_type)?;
                Some((
                    if instance.is_spot() {
                        Market::Spot
                    } else {
                        Market::OnDemand
                    },
                    info,
                ))
            })
            .collect();

        Ok(Quotas::new(limits, &spending))
    }

    /// Every instance already on compute in one region.
    async fn list_running(&self, region: &str) -> Result<Vec<ec2::Instance>, ProviderError> {
        let mut collected = Vec::new();
        let mut next = None;
        loop {
            let page: ec2::DescribeInstancesResponse = self
                .ec2(
                    region,
                    "DescribeInstances",
                    &ec2::DescribeInstances {
                        instance_id: Vec::new(),
                        // A stopped instance spends no quota, so the two
                        // states that do are the two that are counted.
                        filter: vec![ec2::Filter::any_of(
                            "instance-state-name",
                            &["pending", "running"],
                        )],
                        next_token: next,
                    },
                )
                .await?;
            for reservation in page.reservation_set.item {
                collected.extend(reservation.instances_set.item);
            }
            next = page.next_token.filter(|token| !token.is_empty());
            if next.is_none() {
                return Ok(collected);
            }
        }
    }

    /// One instance, as EC2 currently sees it.
    async fn describe_instance(
        &self,
        region: &str,
        instance: &str,
    ) -> Result<ec2::Instance, ProviderError> {
        let response: ec2::DescribeInstancesResponse = self
            .ec2(
                region,
                "DescribeInstances",
                &ec2::DescribeInstances {
                    instance_id: vec![instance.to_owned()],
                    filter: Vec::new(),
                    next_token: None,
                },
            )
            .await?;

        response
            .reservation_set
            .item
            .into_iter()
            .flat_map(|reservation| reservation.instances_set.item)
            .next()
            .ok_or(ProviderError::Malformed(
                "EC2 described no instance under an id it had just accepted",
            ))
    }

    /// Waits until an instance reaches one settled state, and refuses if it
    /// settles anywhere else.
    ///
    /// This is what stands in for Azure's operation resource. The settled
    /// state is the only trustworthy answer: EC2 answers `StopInstances`
    /// with `stopping`, and a `ModifyInstanceAttribute` sent to a
    /// still-running instance is refused with a code that says nothing about
    /// why.
    async fn await_state(
        &self,
        region: &str,
        instance: &str,
        wanted: &InstanceState,
    ) -> Result<ec2::Instance, ProviderError> {
        for attempt in 0..MAX_POLL_ATTEMPTS {
            // EC2 states no `Retry-After`, so the shared backoff is what
            // paces this.
            self.timer.sleep(poll_delay(None, attempt)).await;
            let described = self.describe_instance(region, instance).await?;
            let state = described.state();
            if state == *wanted {
                return Ok(described);
            }
            if state.is_settled() {
                return Err(ProviderError::OperationFailed {
                    status: described.instance_state.name.clone(),
                    code: String::new(),
                    message: format!(
                        "the instance settled as {} rather than reaching the state that was asked for",
                        described.instance_state.name
                    ),
                });
            }
        }

        Err(ProviderError::Rejected(format!(
            "an EC2 instance had not settled after {MAX_POLL_ATTEMPTS} polls"
        )))
    }

    /// Creates or finds the one-time network a region's machines share.
    ///
    /// The VPC and subnet are the account's own default ones, which is the
    /// AWS counterpart of Azure's out-of-band resource group: a VPC with its
    /// gateway and routes is account-shaped infrastructure, and an account
    /// that has deleted its default VPC has made a deliberate networking
    /// decision flyco must not silently undo.
    ///
    /// The security group is not optional, for exactly Azure's reason: a
    /// machine launched into a group that admits nothing comes up perfectly
    /// and answers nothing.
    async fn ensure_network(&mut self, region: &str) -> Result<RegionNetwork, ProviderError> {
        if let Some(network) = &self.network
            && network.region == region
        {
            return Ok(network.clone());
        }

        let vpcs: ec2::DescribeVpcsResponse = self
            .ec2(
                region,
                "DescribeVpcs",
                &ec2::DescribeVpcs {
                    filter: vec![ec2::Filter::is("isDefault", "true")],
                },
            )
            .await?;
        let vpc = vpcs
            .vpc_set
            .item
            .into_iter()
            .next()
            .ok_or(ProviderError::Malformed(
                "this account has no default VPC in the region, so flyco has nowhere to launch",
            ))?;

        let subnets: ec2::DescribeSubnetsResponse = self
            .ec2(
                region,
                "DescribeSubnets",
                &ec2::DescribeSubnets {
                    filter: vec![ec2::Filter::is("vpc-id", vpc.vpc_id.clone())],
                },
            )
            .await?;
        let mut available = subnets.subnet_set.item;
        // Sorted so two provisions in the same region land in the same zone:
        // an unordered "first" would move with the API's own ordering, and a
        // spot price is quoted per zone.
        available.sort_by(|left, right| left.availability_zone.cmp(&right.availability_zone));
        let subnet = available
            .into_iter()
            .next()
            .ok_or(ProviderError::Malformed(
                "this account's default VPC has no subnet in the region",
            ))?;

        let group_name = names::security_group(region);
        let groups: ec2::DescribeSecurityGroupsResponse = self
            .ec2(
                region,
                "DescribeSecurityGroups",
                &ec2::DescribeSecurityGroups {
                    filter: vec![
                        ec2::Filter::is("group-name", group_name.clone()),
                        ec2::Filter::is("vpc-id", vpc.vpc_id.clone()),
                    ],
                },
            )
            .await?;

        let security_group_id = match groups.security_group_info.item.into_iter().next() {
            Some(group) => group.group_id,
            None => {
                self.create_security_group(region, &vpc.vpc_id, &group_name)
                    .await?
            }
        };

        let network = RegionNetwork {
            region: region.to_owned(),
            subnet_id: subnet.subnet_id,
            security_group_id,
        };
        self.network = Some(network.clone());
        Ok(network)
    }

    /// Creates the workspace security group and opens SSH on it.
    async fn create_security_group(
        &self,
        region: &str,
        vpc: &str,
        name: &str,
    ) -> Result<String, ProviderError> {
        let created: ec2::CreateSecurityGroupResponse = self
            .ec2(
                region,
                "CreateSecurityGroup",
                &ec2::CreateSecurityGroup {
                    group_name: name.to_owned(),
                    group_description: "flyco session machines".to_owned(),
                    vpc_id: vpc.to_owned(),
                },
            )
            .await?;

        self.ec2_call(
            region,
            "AuthorizeSecurityGroupIngress",
            &ec2::AuthorizeSecurityGroupIngress {
                group_id: created.group_id.clone(),
                ip_permissions: vec![ec2::IpPermission {
                    ip_protocol: "tcp",
                    from_port: SSH_PORT,
                    to_port: SSH_PORT,
                    ip_ranges: vec![ec2::IpRange {
                        cidr_ip: SSH_SOURCE,
                    }],
                }],
            },
        )
        .await?;

        tracing::info!(%region, group = %created.group_id, "created the flyco security group");
        Ok(created.group_id)
    }

    /// Finds an instance type and refuses it unless all three gates pass.
    ///
    /// The quotas come back with the type because the spot fallback needs to
    /// re-check them against the other pool without a second read.
    async fn deployable_type(
        &mut self,
        region: &str,
        instance_type: &str,
        mode: CapacityMode,
    ) -> Result<(ec2::InstanceTypeInfo, Quotas), ProviderError> {
        let access = self.region_access().await?;
        if !access.allows(region) {
            return Err(ProviderError::Unavailable {
                machine_type: instance_type.to_owned(),
                region: region.to_owned(),
                reason: access.refusal(region),
            });
        }

        let types = self.list_instance_types(region).await?;
        let info = types
            .iter()
            .find(|info| info.instance_type == instance_type)
            .cloned()
            .ok_or_else(|| ProviderError::Unavailable {
                machine_type: instance_type.to_owned(),
                region: region.to_owned(),
                reason: "EC2 describes no such instance type".to_owned(),
            })?;

        let offerings = self.list_offerings(region).await?;
        if !offerings.iter().any(|offered| offered == instance_type) {
            return Err(ProviderError::Unavailable {
                machine_type: instance_type.to_owned(),
                region: region.to_owned(),
                reason: "EC2 does not offer this instance type in this region".to_owned(),
            });
        }

        // Flyco's session image is Ubuntu, and there is no Ubuntu for a
        // Mac. The type stays in the catalog — an agent deciding between
        // machines needs its price and its 24-hour minimum — but asking for
        // one is refused here, with the reason, rather than failing later on
        // an image lookup that cannot succeed.
        if os_family(&info.processor_info.supported_architectures.item) != OsFamily::Linux {
            return Err(ProviderError::Unsupported {
                provider: PROVIDER,
                operation: "provision",
                reason: "flyco boots Ubuntu and this instance type runs macOS",
            });
        }

        let quotas = self.read_quotas(region, &types).await?;
        quotas.require_capacity_for(&info, region, mode)?;
        Ok((info, quotas))
    }

    /// The AMI a machine boots, and the device its root volume attaches at.
    ///
    /// Both are read rather than assumed: the id is region- and
    /// architecture-specific, and the root device name differs between
    /// images, so a block-device mapping naming the wrong one is silently
    /// ignored and the volume comes out the image's default size.
    async fn boot_image(
        &self,
        region: &str,
        info: &ec2::InstanceTypeInfo,
    ) -> Result<(String, String), ProviderError> {
        let architecture = info.architecture().ok_or(ProviderError::Malformed(
            "EC2 reported an instance type with no instruction set flyco has an image for",
        ))?;

        let parameter: image::GetParameterResponse = self
            .json_rpc(
                &ec2::endpoint(image::SERVICE, region),
                Scope {
                    region,
                    service: image::SERVICE,
                },
                image::TARGET,
                &image::parameter_for(architecture)?,
            )
            .await?;
        let image_id = parameter.parameter.value;

        let described: ec2::DescribeImagesResponse = self
            .ec2(
                region,
                "DescribeImages",
                &ec2::DescribeImages {
                    image_id: vec![image_id.clone()],
                },
            )
            .await?;
        let root_device = described
            .images_set
            .item
            .into_iter()
            .next()
            .and_then(|image| image.root_device_name)
            .ok_or(ProviderError::Malformed(
                "the boot image names no root device, so a volume could not be sized",
            ))?;

        Ok((image_id, root_device))
    }

    /// The tags every resource one machine owns carries.
    fn tags(request: &ProvisionRequest, resource_type: &'static str) -> ec2::TagSpecification {
        ec2::TagSpecification {
            resource_type,
            tag: vec![
                ec2::Tag {
                    key: OWNER_TAG.to_owned(),
                    value: PROVIDER.to_owned(),
                },
                ec2::Tag {
                    key: SESSION_TAG.to_owned(),
                    value: request.bootstrap.session.to_string(),
                },
                ec2::Tag {
                    key: MACHINE_TAG.to_owned(),
                    value: request.machine.to_string(),
                },
                ec2::Tag {
                    key: "Name".to_owned(),
                    value: names::machine(request.machine),
                },
            ],
        }
    }

    /// The `RunInstances` body for one request, asking for spot when the
    /// spec does.
    fn run_body(
        &self,
        request: &ProvisionRequest,
        network: &RegionNetwork,
        image_id: String,
        root_device: String,
    ) -> Result<ec2::RunInstances, ProviderError> {
        let config = flycod::render(&request.bootstrap)
            .map_err(|_| ProviderError::Malformed("the flycod configuration did not render"))?;

        Ok(ec2::RunInstances {
            image_id,
            instance_type: request.spec.machine_type.clone(),
            min_count: 1,
            max_count: 1,
            subnet_id: network.subnet_id.clone(),
            security_group_id: vec![network.security_group_id.clone()],
            key_name: self.workspace.key_name.clone(),
            user_data: cloud_init::render(&config, &self.workspace.flycod_installer_url)?,
            block_device_mapping: vec![ec2::BlockDeviceMapping {
                device_name: root_device,
                ebs: ec2::Ebs {
                    volume_size: request.spec.disk_gib,
                    volume_type: ROOT_VOLUME_TYPE,
                    // This driver's `Detach`: the disk outlives the machine,
                    // and `destroy` is what deletes it.
                    delete_on_termination: false,
                },
            }],
            instance_market_options: request.spec.spot.then_some(ec2::InstanceMarketOptions {
                market_type: ec2::MarketType::Spot,
                spot_options: ec2::SpotOptions {
                    spot_instance_type: ec2::SpotInstanceType::Persistent,
                    instance_interruption_behavior: ec2::InterruptionBehavior::Stop,
                },
            }),
            tag_specification: vec![
                Self::tags(request, "instance"),
                Self::tags(request, "volume"),
            ],
        })
    }

    /// Launches the machine, falling back to on-demand when spot is refused.
    ///
    /// The fallback re-checks quota, because the two markets are funded from
    /// different pools: a machine the spot pool could afford may have no
    /// on-demand headroom at all, and re-sending it would trade a clear
    /// refusal for an opaque one.
    async fn launch(
        &self,
        region: &str,
        body: ec2::RunInstances,
        info: &ec2::InstanceTypeInfo,
        quotas: &Quotas,
    ) -> Result<ec2::Instance, ProviderError> {
        let asked_for_spot = body.is_spot();
        let attempt: Result<ec2::RunInstancesResponse, ProviderError> =
            self.ec2(region, "RunInstances", &body).await;

        let launched = match attempt {
            Ok(launched) => launched,
            Err(error) if asked_for_spot && spot_unsupported(&error) => {
                quotas.require_capacity_for(info, region, CapacityMode::OnDemand)?;
                tracing::info!(
                    code = error.code().unwrap_or_default(),
                    "EC2 refused spot capacity; retrying the same machine on-demand"
                );
                self.ec2(region, "RunInstances", &body.without_spot())
                    .await?
            }
            Err(error) => return Err(error),
        };

        launched
            .instances_set
            .item
            .into_iter()
            .next()
            .ok_or(ProviderError::Malformed(
                "EC2 accepted a launch and named no instance",
            ))
    }

    /// The elastic IP one machine owns, found by its tag.
    ///
    /// By tag rather than by association: an association is gone the moment
    /// the instance terminates, and an address nothing can find is an
    /// address nobody stops paying for.
    async fn address_of(
        &self,
        region: &str,
        machine: MachineId,
    ) -> Result<Option<ec2::Address>, ProviderError> {
        let response: ec2::DescribeAddressesResponse = self
            .ec2(
                region,
                "DescribeAddresses",
                &ec2::DescribeAddresses {
                    filter: vec![ec2::Filter::is(
                        &format!("tag:{MACHINE_TAG}"),
                        machine.to_string(),
                    )],
                },
            )
            .await?;
        Ok(response.addresses_set.item.into_iter().next())
    }

    /// Waits until a volume has detached and can be deleted.
    async fn await_detached(&self, region: &str, volume: &str) -> Result<(), ProviderError> {
        for attempt in 0..MAX_POLL_ATTEMPTS {
            let described: ec2::DescribeVolumesResponse = self
                .ec2(
                    region,
                    "DescribeVolumes",
                    &ec2::DescribeVolumes {
                        volume_id: vec![volume.to_owned()],
                    },
                )
                .await?;
            if described
                .volume_set
                .item
                .first()
                .is_some_and(ec2::Volume::is_available)
            {
                return Ok(());
            }
            self.timer.sleep(poll_delay(None, attempt)).await;
        }

        Err(ProviderError::Rejected(format!(
            "an EBS volume had not detached after {MAX_POLL_ATTEMPTS} polls"
        )))
    }

    /// What one region offers, and why everything else was left out.
    ///
    /// The three gates, applied in the order that makes the answer cheapest
    /// and the explanation clearest: a region the account has not enabled
    /// rules out everything without a single instance type being read.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] if AWS refuses any of the reads.
    pub async fn region_report(&mut self, region: &str) -> Result<RegionReport, ProviderError> {
        let access = self.region_access().await?;
        if !access.allows(region) {
            return Ok(RegionReport {
                region: region.to_owned(),
                offered: Vec::new(),
                excluded: vec![(
                    region.to_owned(),
                    ExclusionReason::RegionForbidden(access.refusal(region)),
                )],
            });
        }

        let types = self.list_instance_types(region).await?;
        let offerings = self.list_offerings(region).await?;
        let quotas = self.read_quotas(region, &types).await?;
        let now = self.now();
        let priced = self
            .prices
            .region_prices(&self.transport, &self.clock, &self.key, region, now)
            .await?
            .to_vec();

        let mut report = RegionReport {
            region: region.to_owned(),
            offered: Vec::new(),
            excluded: Vec::new(),
        };

        for info in types {
            match Self::entry_for(&info, region, &offerings, &quotas, &priced) {
                Ok(entry) => report.offered.push(entry),
                Err(reason) => report.excluded.push((info.instance_type, reason)),
            }
        }
        Ok(report)
    }

    /// One instance type's catalog entry, or the reason it has none.
    fn entry_for(
        info: &ec2::InstanceTypeInfo,
        region: &str,
        offerings: &[String],
        quotas: &Quotas,
        priced: &[(String, MachinePrices)],
    ) -> Result<MachineCatalogEntry, ExclusionReason> {
        let name = info.instance_type.as_str();
        if !offerings.iter().any(|offered| offered == name) {
            return Err(ExclusionReason::NotOffered(region.to_owned()));
        }

        // Either market is enough to put a machine on the menu, but a spot
        // price is only quoted when the spot pool could actually fund it.
        let on_demand = quotas.require_capacity_for(info, region, CapacityMode::OnDemand);
        let spot = quotas.require_capacity_for(info, region, CapacityMode::Spot);
        if let (Err(refused), Err(_)) = (&on_demand, &spot) {
            return Err(ExclusionReason::NoQuota(refused.to_string()));
        }

        if info.vcpu_info.default_vcpus == 0 || info.memory_info.size_in_mib == 0 {
            return Err(ExclusionReason::Unreadable);
        }

        let published = priced
            .iter()
            .find(|(priced_name, _)| priced_name == name)
            .map(|(_, prices)| *prices)
            .ok_or(ExclusionReason::Unpriced)?;
        let on_demand_hourly = published.on_demand.ok_or(ExclusionReason::Unpriced)?;

        Ok(MachineCatalogEntry {
            // Stamped by the control plane, which knows the row.
            account: None,
            provider: CloudProviderKind::Aws,
            region: region.to_owned(),
            machine_type: name.to_owned(),
            os: os_family(&info.processor_info.supported_architectures.item),
            capacity: Some(MachineCapacity {
                vcpus: info.vcpu_info.default_vcpus,
                memory_mib: info.memory_info.size_in_mib,
            }),
            pricing: MachinePricing::Metered {
                on_demand_hourly,
                spot_hourly: spot.is_ok().then_some(published.spot).flatten(),
                // EC2 bills a Linux instance by the second past a one-minute
                // floor, with one exception: a Mac runs on a dedicated host
                // that Apple's licence requires be allocated for 24 hours,
                // so an hour of it is billed as a day.
                minimum_billing_hours: quotas
                    .needs_dedicated_host(name)
                    .then_some(MAC_MINIMUM_BILLING_HOURS),
            },
        })
    }

    /// What AWS's own meter says this account has been billed this month.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] if Cost Explorer refuses the query — which
    /// it does when the account has never opted into it — or if the account
    /// is metered in a currency flyco does not account in.
    pub async fn billing_period_cost(
        &mut self,
        now_unix: u64,
    ) -> Result<CloudSpend, ProviderError> {
        let result: costs::CostResult = self
            .json_rpc(
                costs::COST_EXPLORER_ENDPOINT,
                Scope {
                    region: costs::COST_EXPLORER_REGION,
                    service: costs::SERVICE,
                },
                costs::TARGET,
                &costs::GetCostAndUsage::month_to_date(now_unix)?,
            )
            .await?;
        costs::spend_of(&result, now_unix)
    }

    /// Proves the access key is real, and answers with the account it opens.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] if STS refuses the credentials.
    pub async fn caller_identity(&mut self) -> Result<identity::CallerIdentity, ProviderError> {
        let request = HttpRequest::new(Method::Post, identity::ENDPOINT).form_body(&[
            ("Action", "GetCallerIdentity"),
            ("Version", identity::API_VERSION),
        ]);
        let signed = sigv4::sign(
            request,
            &self.key,
            Scope {
                region: identity::REGION,
                service: identity::SERVICE,
            },
            self.now(),
        )?;

        let response = self.transport.send(signed).await?;
        if !response.is_success() {
            return Err(ec2::refusal(&response));
        }
        let answered: identity::GetCallerIdentityResponse = ec2::decode(&response)?;
        Ok(answered.result)
    }
}

/// One signed EC2 call, as the free function the pricing catalog also uses.
///
/// # Errors
///
/// Returns [`ProviderError`] if the request cannot be encoded or signed, or
/// if no response was obtained.
pub async fn send_ec2<T: HttpTransport, B: Serialize>(
    transport: &T,
    key: &AccessKey,
    region: &str,
    action: &'static str,
    body: &B,
    now_unix: u64,
) -> Result<HttpResponse, ProviderError> {
    let mut fields = vec![
        ("Action".to_owned(), action.to_owned()),
        ("Version".to_owned(), ec2::API_VERSION.to_owned()),
    ];
    fields.extend(query::to_pairs(body).map_err(|error| {
        tracing::error!(%error, %action, "an EC2 request could not be encoded");
        ProviderError::Malformed("an EC2 request could not be encoded")
    })?);

    let pairs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let request =
        HttpRequest::new(Method::Post, ec2::endpoint(ec2::SERVICE, region)).form_body(&pairs);

    Ok(transport
        .send(sigv4::sign(
            request,
            key,
            Scope {
                region,
                service: ec2::SERVICE,
            },
            now_unix,
        )?)
        .await?)
}

/// One JSON-RPC request to a service that speaks `awsJson1_1`.
///
/// # Errors
///
/// Returns [`ProviderError`] if the body does not serialize.
pub fn json_rpc_request<B: Serialize>(
    endpoint: &str,
    target: &str,
    body: &B,
) -> Result<HttpRequest, ProviderError> {
    Ok(HttpRequest::new(Method::Post, endpoint)
        .header("x-amz-target", target)
        // The JSON-RPC services refuse `application/json`: the version is
        // part of the media type, and it is what selects the protocol.
        .body(JSON_RPC_MEDIA_TYPE, crate::http::encode_json(body)?))
}

/// Turns a refused JSON-RPC response into an error that keeps its code.
///
/// The AWS JSON protocols state the code in a `__type` that is a shape id —
/// `com.amazonaws.ec2#RequestLimitExceeded` — so the name after the `#` is
/// the code the caller can act on.
#[must_use]
pub fn json_refusal(response: &HttpResponse) -> ProviderError {
    let Ok(document) = response.json::<serde_json::Value>() else {
        return ProviderError::Rejected(format!(
            "AWS answered HTTP {}: {}",
            response.status,
            response.body_text()
        ));
    };

    let code = document
        .get("__type")
        .and_then(serde_json::Value::as_str)
        .map(|shape| shape.rsplit('#').next().unwrap_or(shape).to_owned())
        .unwrap_or_default();
    let message = document
        .get("message")
        .or_else(|| document.get("Message"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();

    if code.is_empty() {
        return ProviderError::Rejected(format!(
            "AWS answered HTTP {}: {}",
            response.status,
            response.body_text()
        ));
    }
    ProviderError::Refused { code, message }
}

/// Whether a refusal means "the same machine, without the market options".
#[must_use]
pub fn spot_unsupported(error: &ProviderError) -> bool {
    error
        .code()
        .is_some_and(|code| SPOT_UNSUPPORTED_CODES.contains(&code))
}

impl<T: HttpTransport, C: MonotonicClock, K: Timer, W: WallClock> CloudProvider
    for AwsProvider<T, C, K, W>
{
    async fn catalog(&mut self) -> Result<Vec<MachineCatalogEntry>, ProviderError> {
        let mut entries = Vec::new();
        for region in self.catalog_regions().await? {
            let report = self.region_report(&region).await?;
            tracing::debug!(
                %region,
                offered = report.offered.len(),
                excluded = report.excluded.len(),
                "read an AWS region's catalog"
            );
            entries.extend(report.offered);
        }
        Ok(entries)
    }

    /// The three gates, the workspace network, the address, the machine.
    ///
    /// The address is allocated before the launch for Azure's reason — a
    /// machine has to be created *with* the network it will answer on — and
    /// released again if the launch fails, because an elastic IP nothing is
    /// using is still billed by the hour.
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
        let (info, quotas) = self
            .deployable_type(region, machine_type, requested)
            .await?;
        let network = self.ensure_network(region).await?;
        let (image_id, root_device) = self.boot_image(region, &info).await?;

        let allocated: ec2::AllocateAddressResponse = self
            .ec2(
                region,
                "AllocateAddress",
                &ec2::AllocateAddress {
                    domain: "vpc",
                    tag_specification: vec![Self::tags(request, "elastic-ip")],
                },
            )
            .await?;

        let body = self.run_body(request, &network, image_id, root_device)?;
        let launched = match self.launch(region, body, &info, &quotas).await {
            Ok(launched) => launched,
            Err(error) => {
                // An address nothing is attached to is still billed, so a
                // failed launch takes it with it rather than leaving one
                // behind that only a console visit would ever find.
                self.release_address(region, &allocated.allocation_id).await;
                return Err(error);
            }
        };

        let instance = self
            .await_state(region, &launched.instance_id, &InstanceState::Running)
            .await?;
        self.ec2_call(
            region,
            "AssociateAddress",
            &ec2::AssociateAddress {
                allocation_id: allocated.allocation_id.clone(),
                instance_id: launched.instance_id.clone(),
            },
        )
        .await?;

        let capacity_mode = if instance.is_spot() {
            CapacityMode::Spot
        } else {
            CapacityMode::OnDemand
        };
        tracing::info!(
            machine = %id,
            %machine_type,
            %region,
            capacity = ?capacity_mode,
            "provisioned an AWS machine"
        );

        Ok(Machine {
            id,
            native_id: launched.instance_id,
            region: region.clone(),
            state: MachineState::Running,
            capacity_mode,
            // The elastic IP, which is the whole reason there is one: it
            // survives a stop, so a machine that comes back from a spot
            // interruption is still reachable where it was.
            address: Some(allocated.public_ip),
        })
    }

    /// Stop, change the type, start.
    ///
    /// Always through a stop, because the attribute is settable only on a
    /// stopped instance, and the disk survives because it is a separate
    /// volume that was never being deleted. The result is read from the
    /// settled states, never from the instance's own reported type: a
    /// modification that failed leaves the instance running on the old type
    /// with nothing to distinguish it.
    async fn resize(
        &mut self,
        machine: &Machine,
        new_machine_type: &str,
    ) -> Result<Machine, ProviderError> {
        let region = machine.region.clone();
        self.deployable_type(&region, new_machine_type, machine.capacity_mode)
            .await?;

        self.ec2_call(
            &region,
            "StopInstances",
            &ec2::InstanceAction::on(&machine.native_id),
        )
        .await?;
        self.await_state(&region, &machine.native_id, &InstanceState::Stopped)
            .await?;

        self.ec2_call(
            &region,
            "ModifyInstanceAttribute",
            &ec2::ModifyInstanceType {
                instance_id: machine.native_id.clone(),
                instance_type: ec2::AttributeValue {
                    value: new_machine_type.to_owned(),
                },
            },
        )
        .await?;

        self.ec2_call(
            &region,
            "StartInstances",
            &ec2::InstanceAction::on(&machine.native_id),
        )
        .await?;
        self.await_state(&region, &machine.native_id, &InstanceState::Running)
            .await?;

        tracing::info!(machine = %machine.id, %new_machine_type, "resized an AWS machine");
        Ok(Machine {
            state: MachineState::Running,
            ..machine.clone()
        })
    }

    async fn deallocate(&mut self, machine: &Machine) -> Result<(), ProviderError> {
        let region = machine.region.clone();
        self.ec2_call(
            &region,
            "StopInstances",
            &ec2::InstanceAction::on(&machine.native_id),
        )
        .await?;
        self.await_state(&region, &machine.native_id, &InstanceState::Stopped)
            .await?;
        Ok(())
    }

    async fn start(&mut self, machine: &Machine) -> Result<Machine, ProviderError> {
        let region = machine.region.clone();
        self.ec2_call(
            &region,
            "StartInstances",
            &ec2::InstanceAction::on(&machine.native_id),
        )
        .await?;
        self.await_state(&region, &machine.native_id, &InstanceState::Running)
            .await?;

        Ok(Machine {
            state: MachineState::Running,
            ..machine.clone()
        })
    }

    /// Terminates the machine and removes everything that outlived it.
    ///
    /// In dependency order, and reading what has to be released *before*
    /// anything is destroyed: the root volume's id is only on the instance,
    /// and an elastic IP's association is gone the moment the instance is.
    /// Then the instance, then the address, then the volume — which is only
    /// deletable once it has actually detached, so that is waited for rather
    /// than assumed.
    async fn destroy(&mut self, machine: &Machine) -> Result<(), ProviderError> {
        let region = machine.region.clone();
        let described = self.describe_instance(&region, &machine.native_id).await?;
        let volume = described.root_volume().map(ToOwned::to_owned);
        let address = self.address_of(&region, machine.id).await?;

        self.ec2_call(
            &region,
            "TerminateInstances",
            &ec2::InstanceAction::on(&machine.native_id),
        )
        .await?;
        self.await_state(&region, &machine.native_id, &InstanceState::Terminated)
            .await?;

        if let Some(address) = address {
            self.ec2_call(
                &region,
                "ReleaseAddress",
                &ec2::ReleaseAddress {
                    allocation_id: address.allocation_id,
                },
            )
            .await?;
        }

        if let Some(volume) = volume {
            self.await_detached(&region, &volume).await?;
            self.ec2_call(
                &region,
                "DeleteVolume",
                &ec2::DeleteVolume { volume_id: volume },
            )
            .await?;
        }

        tracing::info!(machine = %machine.id, "destroyed an AWS machine and its resources");
        Ok(())
    }
}

impl<T: HttpTransport, C: MonotonicClock, K: Timer, W: WallClock> AwsProvider<T, C, K, W> {
    /// Releases an address a failed launch would otherwise leave behind.
    ///
    /// A failure here is logged rather than raised: the caller is already
    /// returning the error that matters, and replacing it with a cleanup
    /// failure would hide why the provision failed in the first place.
    async fn release_address(&self, region: &str, allocation: &str) {
        let released = self
            .ec2_call(
                region,
                "ReleaseAddress",
                &ec2::ReleaseAddress {
                    allocation_id: allocation.to_owned(),
                },
            )
            .await;
        if let Err(error) = released {
            tracing::warn!(
                %error,
                %allocation,
                "could not release the elastic IP of a launch that failed"
            );
        }
    }
}
