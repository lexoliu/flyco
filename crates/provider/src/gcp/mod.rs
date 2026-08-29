//! The GCP driver.
//!
//! Plain HTTPS against Compute Engine and the Cloud Billing catalog, so the
//! whole of it runs inside the Cloudflare Worker. Of the three cloud
//! drivers this is the one closest in shape to Azure's: every mutating call
//! answers with an **operation resource** that is polled to a terminal
//! state, and the operation is what is trusted rather than a read-back of
//! the resource.
//!
//! # Where it differs from the other two
//!
//! * **Authentication is a signature, not a secret.** A service account
//!   proves itself by signing a JWT with its RSA key and exchanging the
//!   assertion for a token; see [`auth`]. That needs civil time — the claims
//!   are checked against Google's clock — which is why the driver holds a
//!   [`WallClock`] as well as a monotonic one.
//! * **A machine type has no price.** Google publishes a rate per vCPU-hour
//!   and per GiB-hour, per family, per region, and a machine type's price is
//!   its own shape multiplied by those; see [`pricing`].
//! * **Quota states both halves.** A region's own answer carries `limit` and
//!   `usage` together, so — unlike AWS — there is nothing to count.
//! * **There is no `DONE` that means success.** A finished operation carries
//!   its failure inside it, so the status alone is never the answer.
//!
//! # Two gates, not three
//!
//! Azure has a subscription policy and AWS a region opt-in, both of which a
//! provisioning credential can read. Google's equivalent — the
//! `gcp.resourceLocations` organization policy — is an Org Policy API read
//! that a project-scoped service account is not granted and cannot be, so
//! flyco does not pretend to check it: a project under such a policy is
//! refused by Compute Engine at the insert, with a code that names it. What
//! *is* checked is what a compute credential can actually see:
//!
//! 1. **Availability** — the zone offers the machine type, and the type is
//!    not `OBSOLETE`. `machineTypes.list` is per zone, and a type offered in
//!    one zone of a region is routinely absent from another.
//! 2. **Quota** — and **spot draws on a different pool**, exactly as
//!    elsewhere: `PREEMPTIBLE_CPUS` is a separate limit from `CPUS`, so a
//!    machine the on-demand pool cannot fund is often perfectly runnable as
//!    spot. See [`quotas`].
//!
//! # Spot, and what survives a preemption
//!
//! Spot is requested by default: `provisioningModel: SPOT`,
//! `onHostMaintenance: TERMINATE` — which the API requires rather than
//! infers, because a spot instance may not be live-migrated — and
//! `instanceTerminationAction: STOP`, which is what keeps a preempted
//! machine's boot disk rather than deleting it with the instance. On a
//! refusal about spot capacity the identical body is re-sent as `STANDARD`,
//! after re-checking the on-demand pool.
//!
//! # Nothing is orphaned
//!
//! The boot disk carries `autoDelete: false`, which is this driver's
//! `Detach`, so it outlives the instance and [`GcpProvider::destroy`]
//! deletes it explicitly after the instance is gone. Deleting them in the
//! other order fails on a disk that is still attached.
//!
pub mod auth;
pub mod compute;
pub mod pricing;
pub mod quotas;

#[cfg(test)]
mod tests;

use flyco_core::MachineId;
use flyco_core::machine::{
    CloudProviderKind, MachineCapacity, MachineCatalogEntry, MachinePricing, MachineSpec,
    MachineState, OsFamily,
};

use crate::clock::{MonotonicClock, SystemClock, SystemTimer, SystemWallClock, Timer, WallClock};
use crate::http::{HttpRequest, HttpResponse, HttpTransport, Method};
use crate::polling::{MAX_POLL_ATTEMPTS, poll_delay};
use crate::{
    CapacityMode, CloudProvider, Machine, ProviderError, ProvisionRequest, ZenwaveTransport,
    cloud_init, flycod,
};

use auth::{ServiceAccountKey, TokenCache};
use compute::Operation;
use pricing::{Market, PriceCatalog};
use quotas::Quotas;

/// Driver name, as it appears in [`ProviderError::Unsupported`].
pub const PROVIDER: &str = "gcp";

/// Zones a catalog covers when the caller names none.
///
/// A short, overridable starting point rather than every zone the project
/// can reach: a catalog is three reads per zone plus a catalog-wide price
/// walk, and Compute Engine has over a hundred zones. One per continent is
/// enough for a first machine, and a caller who wants somewhere else names
/// it.
pub const DEFAULT_CANDIDATE_ZONES: [&str; 3] =
    ["us-central1-a", "europe-west4-a", "asia-southeast1-a"];

/// The VPC network a machine joins.
///
/// The project's own default network, which is the GCP counterpart of
/// Azure's out-of-band resource group: a network with its subnets, routes
/// and firewall rules is project-shaped infrastructure, and a project that
/// has deleted its default network has made a deliberate networking decision
/// flyco must not silently undo.
pub const DEFAULT_NETWORK: &str = "global/networks/default";

/// The image family every flyco machine boots.
///
/// A *family* rather than an image name, which is what makes it track
/// Canonical's republications instead of pinning one build — the same
/// property the AWS driver gets from an SSM parameter, and here it costs no
/// extra call at all.
pub const IMAGE_FAMILY: &str =
    "projects/ubuntu-os-cloud/global/images/family/ubuntu-2404-lts-amd64";

/// Storage tier of the boot disk.
pub const BOOT_DISK_TYPE: &str = "pd-balanced";

/// Metadata key the guest's cloud-init reads its configuration from.
pub const USER_DATA_KEY: &str = "user-data";

/// Label every flyco resource carries, so a project shared with other work
/// stays legible.
pub const OWNER_LABEL: &str = "owner";

/// Label naming the session a resource serves.
pub const SESSION_LABEL: &str = "flyco-session";

/// Label naming the machine a resource belongs to.
pub const MACHINE_LABEL: &str = "flyco-machine";

/// The refusals that mean "ask again as ordinary capacity".
///
/// Each is Compute Engine saying something about *interruptible capacity*
/// rather than about the machine: the zone has no spot capacity for this
/// shape right now, or the project's preemptible quota binds. Everything
/// else — a bad image, a missing permission, an unusable network — is a
/// genuine failure, and retrying it on-demand would turn one clear error
/// into two.
pub const SPOT_UNSUPPORTED_CODES: [&str; 3] = [
    "ZONE_RESOURCE_POOL_EXHAUSTED",
    "ZONE_RESOURCE_POOL_EXHAUSTED_WITH_DETAILS",
    "QUOTA_EXCEEDED",
];

/// The names every flyco resource in one project answers to.
///
/// Derived from the machine id rather than allocated, so a name is
/// recoverable without a column to store it in and two callers cannot
/// disagree. Compute Engine names must start with a letter and hold only
/// lowercase letters, digits and hyphens, which a UUID with a prefix
/// satisfies.
pub mod names {
    use flyco_core::MachineId;

    /// A machine's instance, which is also its hostname.
    #[must_use]
    pub fn machine(id: MachineId) -> String {
        format!("flyco-{id}")
    }

    /// A machine's boot disk.
    #[must_use]
    pub fn boot_disk(id: MachineId) -> String {
        format!("flyco-{id}-boot")
    }
}

/// Why a machine type is not on offer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExclusionReason {
    /// The region the zone is in cannot be deployed into at all.
    #[error("{0}")]
    ZoneUnavailable(String),
    /// Compute Engine has withdrawn this machine type.
    #[error("this machine type is no longer offered: {0}")]
    NotOffered(String),
    /// Neither the on-demand pool nor the preemptible pool has room for it.
    #[error("{0}")]
    NoQuota(String),
    /// The billing catalog publishes no rate for its family, so flyco
    /// cannot tell the user what an hour would cost.
    #[error("no published price")]
    Unpriced,
    /// Compute Engine described a machine type without a usable size.
    #[error("Compute Engine reported this machine type without a size")]
    Unreadable,
}

/// What one zone offers, and why everything else was left out.
#[derive(Debug, Clone)]
pub struct ZoneReport {
    /// The zone this covers.
    pub zone: String,
    /// The machine types a session can actually be started on.
    pub offered: Vec<MachineCatalogEntry>,
    /// Everything else, paired with the reason. When the zone's region is
    /// unusable this holds one entry naming the zone rather than one per
    /// machine type: the answer is the same for all of them.
    pub excluded: Vec<(String, ExclusionReason)>,
}

/// The parts of a GCP project that are configuration rather than
/// credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcpWorkspace {
    /// Where a machine fetches `flycod` from on first boot.
    pub flycod_installer_url: String,
    /// Zones [`GcpProvider::catalog`] reports on.
    ///
    /// Empty — the default — means [`DEFAULT_CANDIDATE_ZONES`].
    pub zones: Vec<String>,
}

impl GcpWorkspace {
    /// A workspace covering [`DEFAULT_CANDIDATE_ZONES`], installing `flycod`
    /// from [`cloud_init::DEFAULT_FLYCOD_INSTALLER_URL`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            flycod_installer_url: cloud_init::DEFAULT_FLYCOD_INSTALLER_URL.to_owned(),
            zones: Vec::new(),
        }
    }

    /// Reports a catalog for these zones instead.
    #[must_use]
    pub fn with_zones(mut self, zones: Vec<String>) -> Self {
        self.zones = zones;
        self
    }

    /// Installs `flycod` from somewhere else.
    #[must_use]
    pub fn with_installer(mut self, url: impl Into<String>) -> Self {
        self.flycod_installer_url = url.into();
        self
    }
}

impl Default for GcpWorkspace {
    fn default() -> Self {
        Self::new()
    }
}

/// The GCP driver.
///
/// Generic over its transport, clocks and timer so the whole of it is
/// testable against recorded exchanges — the signed assertion included,
/// which is only reproducible because the signing instant comes from a
/// [`WallClock`] rather than from the host.
#[derive(Debug)]
pub struct GcpProvider<T = ZenwaveTransport, C = SystemClock, K = SystemTimer, W = SystemWallClock>
{
    transport: T,
    clock: C,
    timer: K,
    wall_clock: W,
    tokens: TokenCache,
    workspace: GcpWorkspace,
    prices: PriceCatalog,
}

impl GcpProvider {
    /// The driver as it is deployed: zenwave, the host clocks, a real timer.
    #[must_use]
    pub fn new(key: ServiceAccountKey, workspace: GcpWorkspace) -> Self {
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

impl<T: HttpTransport, C: MonotonicClock, K: Timer, W: WallClock> GcpProvider<T, C, K, W> {
    /// The driver over an explicit transport, clocks and timer.
    pub const fn with_parts(
        transport: T,
        clock: C,
        timer: K,
        wall_clock: W,
        key: ServiceAccountKey,
        workspace: GcpWorkspace,
    ) -> Self {
        Self {
            transport,
            clock,
            timer,
            wall_clock,
            tokens: TokenCache::new(key),
            workspace,
            prices: PriceCatalog::new(),
        }
    }

    /// The transport this driver sends through.
    ///
    /// Exposed so a test can read back exactly what was put on the wire.
    pub const fn transport(&self) -> &T {
        &self.transport
    }

    /// The timer this driver waits on, for the same reason.
    pub const fn timer(&self) -> &K {
        &self.timer
    }

    /// The project every URL is built under.
    fn project(&self) -> &str {
        &self.tokens.key().project_id
    }

    /// Sends one authenticated request, re-minting the token on a 401.
    ///
    /// A 401 is retried exactly once: the service is the authority on
    /// whether a token still works, and a second refusal after a fresh one
    /// is about the service account rather than the credential's age.
    async fn send(&mut self, request: HttpRequest) -> Result<HttpResponse, ProviderError> {
        let token = self
            .tokens
            .access_token(&self.transport, &self.clock, &self.wall_clock)
            .await?;
        let response = self.transport.send(request.clone().bearer(&token)).await?;
        if response.status != 401 {
            return Ok(response);
        }

        tracing::debug!("Google rejected an access token; minting a fresh one");
        self.tokens.invalidate();
        let token = self
            .tokens
            .access_token(&self.transport, &self.clock, &self.wall_clock)
            .await?;
        Ok(self.transport.send(request.bearer(&token)).await?)
    }

    /// Sends one authenticated request and refuses anything but a success.
    async fn get<R: serde::de::DeserializeOwned>(
        &mut self,
        url: String,
    ) -> Result<R, ProviderError> {
        let response = self.send(HttpRequest::new(Method::Get, url)).await?;
        if !response.is_success() {
            return Err(compute::refusal(&response));
        }
        Ok(response.json()?)
    }

    /// Sends a mutating request and waits for the operation it started.
    async fn send_and_await(&mut self, request: HttpRequest) -> Result<(), ProviderError> {
        let response = self.send(request).await?;
        if !response.is_success() {
            return Err(compute::refusal(&response));
        }
        let operation: Operation = response.json()?;
        self.await_operation(operation).await
    }

    /// A `POST` with no body, which is what the lifecycle actions are.
    async fn post_action(&mut self, url: String) -> Result<(), ProviderError> {
        self.send_and_await(HttpRequest::new(Method::Post, url))
            .await
    }

    /// Follows an asynchronous operation to a terminal state.
    ///
    /// The terminal set is exactly `{DONE}`; anything else means keep
    /// polling, including in-flight values the service invents. And a `DONE`
    /// operation is **not** a successful one — the failure is reported
    /// inside the finished operation, so the status alone is never the
    /// answer.
    async fn await_operation(&mut self, started: Operation) -> Result<(), ProviderError> {
        let mut operation = started;

        for attempt in 0..MAX_POLL_ATTEMPTS {
            if operation.state().is_terminal() {
                return operation.failure().map_or(Ok(()), Err);
            }

            let url = operation.self_link.clone().ok_or(ProviderError::Malformed(
                "Compute Engine started an operation without naming a URL to follow it on",
            ))?;
            // Compute Engine states no `Retry-After`, so the shared backoff
            // is what paces this.
            self.timer.sleep(poll_delay(None, attempt)).await;
            operation = self.get(url).await?;
        }

        Err(ProviderError::Rejected(format!(
            "a Compute Engine operation was still running after {MAX_POLL_ATTEMPTS} polls"
        )))
    }

    /// Every machine type one zone offers.
    async fn list_machine_types(
        &mut self,
        zone: &str,
    ) -> Result<Vec<compute::MachineType>, ProviderError> {
        let base = compute::zone_url(self.project(), zone, "machineTypes");
        let mut next: Option<String> = None;
        let mut collected = Vec::new();

        loop {
            let url = next.as_ref().map_or_else(
                || base.clone(),
                |token| {
                    let query = url::form_urlencoded::Serializer::new(String::new())
                        .append_pair("pageToken", token)
                        .finish();
                    format!("{base}?{query}")
                },
            );

            let page: compute::MachineTypePage = self.get(url).await?;
            collected.extend(page.items);
            next = page.next_page_token.filter(|token| !token.is_empty());
            if next.is_none() {
                return Ok(collected);
            }
        }
    }

    /// One region's own answer: whether it is up, and what its quotas are.
    async fn read_region(&mut self, region: &str) -> Result<compute::RegionInfo, ProviderError> {
        self.get(compute::project_url(
            self.project(),
            &format!("regions/{region}"),
        ))
        .await
    }

    /// Finds a machine type and refuses it unless both gates pass.
    ///
    /// The quotas come back with the type because the spot fallback needs to
    /// re-check them against the other pool without a second read.
    async fn deployable_type(
        &mut self,
        zone: &str,
        machine_type: &str,
        mode: CapacityMode,
    ) -> Result<(compute::MachineType, Quotas), ProviderError> {
        let region = compute::region_of(zone)?;
        let info = self.read_region(&region).await?;
        if !quotas::is_up(&info) {
            return Err(ProviderError::Unavailable {
                machine_type: machine_type.to_owned(),
                region: zone.to_owned(),
                reason: format!("Compute Engine reports {region} as {}", info.status),
            });
        }

        let found = self
            .list_machine_types(zone)
            .await?
            .into_iter()
            .find(|candidate| candidate.name == machine_type)
            .ok_or_else(|| ProviderError::Unavailable {
                machine_type: machine_type.to_owned(),
                region: zone.to_owned(),
                reason: "this zone offers no such machine type".to_owned(),
            })?;

        if !found.is_offerable() {
            return Err(ProviderError::Unavailable {
                machine_type: machine_type.to_owned(),
                region: zone.to_owned(),
                reason: "Compute Engine has withdrawn this machine type".to_owned(),
            });
        }

        let quotas = Quotas::from_region(&info);
        quotas.require_capacity_for(found.guest_cpus, &region, mode)?;
        Ok((found, quotas))
    }

    /// The `instances.insert` body for one request, asking for spot when the
    /// spec does.
    fn instance_body(
        &self,
        request: &ProvisionRequest,
    ) -> Result<compute::Instance, ProviderError> {
        let id = request.machine;
        let zone = &request.spec.region;
        let config = flycod::render(&request.bootstrap)
            .map_err(|_| ProviderError::Malformed("the flycod configuration did not render"))?;

        Ok(compute::Instance {
            name: names::machine(id),
            machine_type: compute::zone_url(
                self.project(),
                zone,
                &format!("machineTypes/{}", request.spec.machine_type),
            ),
            scheduling: if request.spec.spot {
                compute::Scheduling::spot()
            } else {
                compute::Scheduling::on_demand()
            },
            disks: vec![compute::AttachedDisk {
                boot: true,
                // This driver's `Detach`: the disk outlives the machine, and
                // `destroy` is what deletes it.
                auto_delete: false,
                initialize_params: compute::DiskParams {
                    disk_name: names::boot_disk(id),
                    disk_size_gb: request.spec.disk_gib,
                    source_image: IMAGE_FAMILY.to_owned(),
                    disk_type: compute::zone_url(
                        self.project(),
                        zone,
                        &format!("diskTypes/{BOOT_DISK_TYPE}"),
                    ),
                },
            }],
            network_interfaces: vec![compute::NetworkInterface {
                network: compute::project_url(self.project(), DEFAULT_NETWORK),
                // Without this the machine has no external address at all
                // and answers nothing.
                access_configs: vec![compute::AccessConfig {
                    kind: "ONE_TO_ONE_NAT",
                    name: "External NAT",
                    network_tier: "PREMIUM",
                }],
            }],
            metadata: compute::Metadata {
                items: vec![compute::MetadataItem {
                    key: USER_DATA_KEY,
                    value: cloud_init::render(&config, &self.workspace.flycod_installer_url)?,
                }],
            },
            labels: [
                (OWNER_LABEL.to_owned(), PROVIDER.to_owned()),
                (
                    SESSION_LABEL.to_owned(),
                    request.bootstrap.session.to_string(),
                ),
                (MACHINE_LABEL.to_owned(), id.to_string()),
            ]
            .into_iter()
            .collect(),
        })
    }

    /// The URL of one machine's instance resource.
    fn instance_url(&self, id: MachineId, zone: &str) -> String {
        compute::zone_url(
            self.project(),
            zone,
            &format!("instances/{}", names::machine(id)),
        )
    }

    /// Creates the instance, falling back to on-demand when spot is refused.
    ///
    /// The fallback re-checks quota, because the two markets are funded from
    /// different pools: a machine the preemptible pool could afford may have
    /// no `CPUS` headroom at all, and re-sending it would trade a clear
    /// refusal for an opaque one.
    async fn create_instance(
        &mut self,
        zone: &str,
        body: compute::Instance,
        vcpus: u32,
        quotas: &Quotas,
    ) -> Result<CapacityMode, ProviderError> {
        let url = compute::zone_url(self.project(), zone, "instances");
        let asked_for_spot = body.is_spot();

        let attempt = self
            .send_and_await(HttpRequest::new(Method::Post, url.clone()).json_body(&body)?)
            .await;

        match attempt {
            Ok(()) if asked_for_spot => Ok(CapacityMode::Spot),
            Ok(()) => Ok(CapacityMode::OnDemand),
            Err(error) if asked_for_spot && spot_unsupported(&error) => {
                let region = compute::region_of(zone)?;
                quotas.require_capacity_for(vcpus, &region, CapacityMode::OnDemand)?;
                tracing::info!(
                    code = error.code().unwrap_or_default(),
                    "Compute Engine refused spot capacity; retrying the same machine on-demand"
                );
                let on_demand = body.without_spot();
                self.send_and_await(HttpRequest::new(Method::Post, url).json_body(&on_demand)?)
                    .await?;
                Ok(CapacityMode::OnDemand)
            }
            Err(error) => Err(error),
        }
    }

    /// One instance, as Compute Engine currently sees it.
    async fn describe_instance(
        &mut self,
        id: MachineId,
        zone: &str,
    ) -> Result<compute::InstanceStatus, ProviderError> {
        self.get(self.instance_url(id, zone)).await
    }

    /// What one zone offers, and why everything else was left out.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] if Google refuses any of the reads.
    pub async fn zone_report(&mut self, zone: &str) -> Result<ZoneReport, ProviderError> {
        let region = compute::region_of(zone)?;
        let info = self.read_region(&region).await?;
        if !quotas::is_up(&info) {
            return Ok(ZoneReport {
                zone: zone.to_owned(),
                offered: Vec::new(),
                excluded: vec![(
                    zone.to_owned(),
                    ExclusionReason::ZoneUnavailable(format!(
                        "Compute Engine reports {region} as {}",
                        info.status
                    )),
                )],
            });
        }

        let machine_types = self.list_machine_types(zone).await?;
        let quotas = Quotas::from_region(&info);
        let token = self
            .tokens
            .access_token(&self.transport, &self.clock, &self.wall_clock)
            .await?;
        let rates = self
            .prices
            .region_rates(&self.transport, &self.clock, &token, &region)
            .await?
            .clone();

        let mut report = ZoneReport {
            zone: zone.to_owned(),
            offered: Vec::new(),
            excluded: Vec::new(),
        };

        for machine_type in machine_types {
            match Self::entry_for(&machine_type, zone, &region, &quotas, &rates) {
                Ok(entry) => report.offered.push(entry),
                Err(reason) => report.excluded.push((machine_type.name, reason)),
            }
        }
        Ok(report)
    }

    /// One machine type's catalog entry, or the reason it has none.
    fn entry_for(
        machine_type: &compute::MachineType,
        zone: &str,
        region: &str,
        quotas: &Quotas,
        rates: &pricing::RegionRates,
    ) -> Result<MachineCatalogEntry, ExclusionReason> {
        if !machine_type.is_offerable() {
            return Err(ExclusionReason::NotOffered(zone.to_owned()));
        }

        // Either market is enough to put a machine on the menu, but a spot
        // price is only quoted when the preemptible pool could fund it.
        let on_demand =
            quotas.require_capacity_for(machine_type.guest_cpus, region, CapacityMode::OnDemand);
        let spot = quotas.require_capacity_for(machine_type.guest_cpus, region, CapacityMode::Spot);
        if let (Err(refused), Err(_)) = (&on_demand, &spot) {
            return Err(ExclusionReason::NoQuota(refused.to_string()));
        }

        if machine_type.guest_cpus == 0 || machine_type.memory_mb == 0 {
            return Err(ExclusionReason::Unreadable);
        }
        let family = machine_type.family().ok_or(ExclusionReason::Unreadable)?;

        let hourly = |market| {
            rates
                .family(family, market)
                .and_then(|rates| rates.hourly(machine_type.guest_cpus, machine_type.memory_mb))
        };
        let on_demand_hourly = hourly(Market::OnDemand).ok_or(ExclusionReason::Unpriced)?;

        Ok(MachineCatalogEntry {
            provider: CloudProviderKind::Gcp,
            // The *zone*, not the region: a machine type offered in one zone
            // of a region is routinely absent from another, so a catalog
            // entry that named only the region would not say where the
            // machine it describes can be created.
            region: zone.to_owned(),
            machine_type: machine_type.name.clone(),
            os: OsFamily::Linux,
            capacity: Some(MachineCapacity {
                vcpus: machine_type.guest_cpus,
                memory_mib: machine_type.memory_mb,
            }),
            pricing: MachinePricing::Metered {
                on_demand_hourly,
                spot_hourly: spot.is_ok().then(|| hourly(Market::Spot)).flatten(),
                // Compute Engine bills by the second past a one-minute
                // floor, with no per-type minimum of any kind.
                minimum_billing_hours: None,
            },
        })
    }

    /// Proves the service-account key works, and answers with the token it
    /// mints.
    ///
    /// The cheapest call that exercises the whole credential: it signs an
    /// assertion with the private key and has Google check it, which is the
    /// entire question, and it creates nothing. It does not prove the
    /// account may *provision* — what an IAM role grants is only knowable by
    /// trying, and a link-time simulation would be a second, weaker opinion
    /// about a question the first provision answers exactly.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] if Google refuses the assertion.
    pub async fn mint_token(&mut self) -> Result<String, ProviderError> {
        self.tokens
            .access_token(&self.transport, &self.clock, &self.wall_clock)
            .await
    }
}

/// Whether a refusal means "the same machine, as ordinary capacity".
#[must_use]
pub fn spot_unsupported(error: &ProviderError) -> bool {
    error
        .code()
        .is_some_and(|code| SPOT_UNSUPPORTED_CODES.contains(&code))
}

impl<T: HttpTransport, C: MonotonicClock, K: Timer, W: WallClock> CloudProvider
    for GcpProvider<T, C, K, W>
{
    async fn catalog(&mut self) -> Result<Vec<MachineCatalogEntry>, ProviderError> {
        let zones = if self.workspace.zones.is_empty() {
            DEFAULT_CANDIDATE_ZONES
                .iter()
                .map(|zone| (*zone).to_owned())
                .collect()
        } else {
            self.workspace.zones.clone()
        };

        let mut entries = Vec::new();
        for zone in zones {
            let report = self.zone_report(&zone).await?;
            tracing::debug!(
                %zone,
                offered = report.offered.len(),
                excluded = report.excluded.len(),
                "read a GCP zone's catalog"
            );
            entries.extend(report.offered);
        }
        Ok(entries)
    }

    async fn provision(&mut self, request: &ProvisionRequest) -> Result<Machine, ProviderError> {
        let MachineSpec {
            region: zone,
            machine_type,
            ..
        } = &request.spec;
        let id = request.machine;

        let requested = if request.spec.spot {
            CapacityMode::Spot
        } else {
            CapacityMode::OnDemand
        };
        let (found, quotas) = self.deployable_type(zone, machine_type, requested).await?;

        let body = self.instance_body(request)?;
        let capacity_mode = self
            .create_instance(zone, body, found.guest_cpus, &quotas)
            .await?;

        // Read back only for the address, which the insert cannot state: an
        // ephemeral external address is assigned as the instance comes up.
        // Everything about *whether it worked* came from the operation.
        let described = self.describe_instance(id, zone).await?;

        tracing::info!(
            machine = %id,
            %machine_type,
            %zone,
            capacity = ?capacity_mode,
            "provisioned a GCP machine"
        );
        Ok(Machine {
            id,
            native_id: self.instance_url(id, zone),
            region: zone.clone(),
            state: MachineState::Running,
            capacity_mode,
            address: described.address(),
        })
    }

    /// Stop, `setMachineType`, start.
    ///
    /// Always through a stop, because the machine type is settable only on a
    /// stopped instance, and the disk survives because it is a separate
    /// resource that was never being deleted. The result is read from the
    /// operations, never from the instance.
    async fn resize(
        &mut self,
        machine: &Machine,
        new_machine_type: &str,
    ) -> Result<Machine, ProviderError> {
        let zone = machine.region.clone();
        self.deployable_type(&zone, new_machine_type, machine.capacity_mode)
            .await?;

        self.post_action(format!("{}/stop", self.instance_url(machine.id, &zone)))
            .await?;

        let body = compute::SetMachineType {
            machine_type: compute::zone_url(
                self.project(),
                &zone,
                &format!("machineTypes/{new_machine_type}"),
            ),
        };
        self.send_and_await(
            HttpRequest::new(
                Method::Post,
                format!("{}/setMachineType", self.instance_url(machine.id, &zone)),
            )
            .json_body(&body)?,
        )
        .await?;

        self.post_action(format!("{}/start", self.instance_url(machine.id, &zone)))
            .await?;

        tracing::info!(machine = %machine.id, %new_machine_type, "resized a GCP machine");
        Ok(Machine {
            state: MachineState::Running,
            ..machine.clone()
        })
    }

    async fn deallocate(&mut self, machine: &Machine) -> Result<(), ProviderError> {
        let zone = machine.region.clone();
        self.post_action(format!("{}/stop", self.instance_url(machine.id, &zone)))
            .await
    }

    async fn start(&mut self, machine: &Machine) -> Result<Machine, ProviderError> {
        let zone = machine.region.clone();
        self.post_action(format!("{}/start", self.instance_url(machine.id, &zone)))
            .await?;

        // The external address is ephemeral and a stopped instance releases
        // it, so a started machine answers somewhere new and the caller has
        // to be told where.
        let described = self.describe_instance(machine.id, &zone).await?;
        Ok(Machine {
            state: MachineState::Running,
            address: described.address(),
            ..machine.clone()
        })
    }

    /// Deletes the instance and then the disk that outlived it.
    ///
    /// In that order: a disk that is still attached cannot be deleted, and
    /// the instance's deletion is what detaches it.
    async fn destroy(&mut self, machine: &Machine) -> Result<(), ProviderError> {
        let zone = machine.region.clone();
        self.send_and_await(HttpRequest::new(
            Method::Delete,
            self.instance_url(machine.id, &zone),
        ))
        .await?;

        self.send_and_await(HttpRequest::new(
            Method::Delete,
            compute::zone_url(
                self.project(),
                &zone,
                &format!("disks/{}", names::boot_disk(machine.id)),
            ),
        ))
        .await?;

        tracing::info!(machine = %machine.id, "destroyed a GCP machine and its disk");
        Ok(())
    }
}
