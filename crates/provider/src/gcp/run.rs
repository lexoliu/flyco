//! Cloud Run jobs: the same GCP account, selling a container instead of a
//! virtual machine.
//!
//! One axis on the machine rather than a second driver (issue #235), so
//! everything a Cloud Run session needs that Compute Engine already gave it
//! — the service-account assertion, the 401 retry, the `flycod`
//! configuration, the resource naming — comes from [`super::GcpProvider`]
//! unchanged. What is here is the part that is genuinely a different
//! service: its URLs, its bodies, its published rates, and the five
//! lifecycle operations, which are not the VM ones with different nouns.
//!
//! # A job is a definition; an execution is the machine
//!
//! `jobs.create` writes a definition and starts nothing. `jobs.run` starts
//! an **execution**, and that execution is what a session actually runs on:
//! it holds the vCPU and the memory, it is what is cancelled to stop the
//! session, and its filesystem is what disappears when it ends. So a
//! machine's provider-native identity is both halves — see [`Handle`] — and
//! a `start` is a *new* execution rather than the old one resuming.
//!
//! That is why nothing here waits for the `jobs.run` operation. A Compute
//! Engine `instances.insert` operation finishes when the machine exists; a
//! Cloud Run run operation finishes when the **task** does, which for a
//! session is up to a week away. Waiting on it would be waiting for the
//! session to end. Every other operation — create, patch, cancel, delete —
//! is administrative and is followed to `done` exactly as the compute driver
//! follows its own.
//!
//! # Nothing survives a stop but the patch
//!
//! A job's filesystem is in memory and ends with the execution, so a
//! container session's working tree travels as the `workdir-patch` `flycod`
//! writes on `SIGTERM` and replays onto a fresh clone at the next start —
//! the mechanism [`flyco_core::Runtime`] exists to name. Nothing in this
//! module keeps a disk, because there is none to keep, and
//! [`MachineSpec::disk_gib`](flyco_core::machine::MachineSpec::disk_gib) is
//! ignored here rather than turned into an ephemeral-disk request flyco
//! would then be billed for.
//!
//! # What the user is charged
//!
//! Cloud Run publishes a rate per vCPU-second and per GiB-second in two
//! regional tiers, and **a job is billed at the instance-based rate for the
//! whole lifetime of the instance it starts** — not at the request-based
//! rate a service pays, which is dearer on both meters and counts only the
//! time a request is in flight. The two tables are [`Tier`], copied from
//! [cloud.google.com/run/pricing](https://cloud.google.com/run/pricing) with
//! the region lists that page publishes beside them. A region in neither
//! list is a region flyco cannot price, so it offers no container there
//! rather than quoting the wrong tier.

use core::fmt;
use std::collections::BTreeMap;

use base64::Engine as _;
use flyco_core::MachineId;
use flyco_core::machine::{
    CloudProviderKind, CpuArchitecture, MachineCapacity, MachineCatalogEntry, MachineLineage,
    MachinePricing, MachineState, OsFamily, Runtime, StoragePricing,
};
use flyco_core::money::Usd;
use serde::{Deserialize, Serialize};

use super::{
    CLOUD_RUN_FREE_GRANT, GcpProvider, MACHINE_LABEL, OWNER_LABEL, PROVIDER, SESSION_LABEL, names,
};
use crate::clock::{MonotonicClock, Timer, WallClock};
use crate::host::CONFIG_ENV;
use crate::http::{HttpRequest, HttpResponse, HttpTransport, Method};
use crate::polling::{MAX_POLL_ATTEMPTS, poll_delay};
use crate::{CapacityMode, Machine, ProviderError, ProvisionRequest, flycod};

/// The Cloud Run Admin API's base URL.
///
/// v2 rather than v1: v1 speaks Knative resources and has no jobs at all.
pub const RUN_BASE: &str = "https://run.googleapis.com/v2";

/// How long one task may run before Cloud Run kills it, as the API spells a
/// duration.
///
/// The documented ceiling, 168 hours. A session that is still working after
/// a week is one flyco would rather see stopped by its own archive policy
/// than by a platform limit, so the limit is set as far out of the way as
/// the service allows and never used as a schedule.
pub const TASK_TIMEOUT: &str = "604800s";

/// Retries Cloud Run may make of a failed task: none.
///
/// Not a durability preference. A retry would start a **second** `flycod`
/// against the same session — the same daemon token, the same repository,
/// the same relay — while the control plane still believed there was one.
/// A task that dies is a machine that died, and the control plane's own
/// recovery is what answers for it.
pub const MAX_RETRIES: u32 = 0;

/// Number of tasks one execution runs: one, which is the machine.
pub const TASK_COUNT: u32 = 1;

/// The execution environment a session runs in.
///
/// Second generation, stated rather than left to whatever the service
/// defaults to this year: a session compiles, links and runs test suites,
/// and the first-generation sandbox emulates the Linux system-call surface
/// rather than providing it. A machine whose builds fail on a syscall is
/// not a cheaper machine.
pub const EXECUTION_ENVIRONMENT: &str = "EXECUTION_ENVIRONMENT_GEN2";

/// Prefix every Cloud Run machine type carries.
///
/// Names the service rather than a Google machine family, because there is
/// no family to name: Cloud Run sells vCPU and memory directly, and the
/// "machine type" is flyco's own word for one point on that grid.
pub const MACHINE_TYPE_PREFIX: &str = "cloudrun-";

/// vCPU counts flyco offers a session, from the grid Cloud Run allows.
///
/// Cloud Run accepts `1`, `2`, `4`, `6` or `8` whole vCPUs, and fractions
/// below one. The fractions are left off the menu — a fraction of a core is
/// a machine that cannot build anything, and the catalog's own floor
/// ([`AUTO_MIN_VCPUS`](flyco_core::machine::AUTO_MIN_VCPUS)) is four — and
/// so is `6`, which is `8` at three quarters of the cores for
/// three quarters of the price and nothing else to recommend it.
pub const VCPU_SIZES: [u32; 4] = [1, 2, 4, 8];

/// GiB of memory per vCPU.
///
/// Four, which is both the ratio the rest of flyco's catalog is curated
/// against and exactly the **maximum** Cloud Run allows at every size on
/// this menu: 1 vCPU may hold up to 4 GiB, 2 up to 8, 4 up to 16, 8 up to 32
/// (docs.cloud.google.com/run/docs/configuring/services/memory-limits). A
/// coding agent's failure mode is a linker that runs out of memory, so the
/// menu takes all of it.
pub const MEMORY_GIB_PER_VCPU: u32 = 4;

/// Seconds in an hour, for turning a published per-second rate into the
/// per-hour price the catalog quotes.
const SECONDS_PER_HOUR: u64 = 3_600;

/// Nanodollars in a microdollar.
///
/// The published rates are finer than [`Usd`]'s microdollar: Cloud Run's
/// Tier 2 vCPU-second is $0.0000216, which is 21.6 microdollars. Holding
/// them as nanodollars and dividing only after multiplying up to an hour
/// keeps every price in this module exact rather than rounded twice.
const NANOS_PER_MICRO: u64 = 1_000;

/// Which of Cloud Run's two published price tiers a region is billed at.
///
/// Both tables are from
/// [cloud.google.com/run/pricing](https://cloud.google.com/run/pricing),
/// read 2026-09-09, under *Jobs* — which the page states are billed at the
/// instance-based rate for the entire lifetime of any instance started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// $0.000018 per vCPU-second, $0.000002 per GiB-second.
    One,
    /// $0.0000216 per vCPU-second, $0.0000024 per GiB-second.
    Two,
}

/// Regions Cloud Run bills at [`Tier::One`].
pub const TIER_ONE_REGIONS: [&str; 23] = [
    "africa-south1",
    "asia-east1",
    "asia-northeast1",
    "asia-northeast2",
    "asia-south1",
    "asia-southeast3",
    "asia-southeast4",
    "europe-north1",
    "europe-north2",
    "europe-southwest1",
    "europe-west1",
    "europe-west4",
    "europe-west8",
    "europe-west9",
    "me-west1",
    "northamerica-south1",
    "us-central1",
    "us-east1",
    "us-east4",
    "us-east5",
    "us-south1",
    "us-west1",
    "us-west8",
];

/// Regions Cloud Run bills at [`Tier::Two`].
pub const TIER_TWO_REGIONS: [&str; 22] = [
    "asia-east2",
    "asia-northeast3",
    "asia-south2",
    "asia-southeast1",
    "asia-southeast2",
    "australia-southeast1",
    "australia-southeast2",
    "europe-central2",
    "europe-west10",
    "europe-west12",
    "europe-west2",
    "europe-west3",
    "europe-west6",
    "me-central1",
    "me-central2",
    "northamerica-northeast1",
    "northamerica-northeast2",
    "southamerica-east1",
    "southamerica-west1",
    "us-west2",
    "us-west3",
    "us-west4",
];

impl Tier {
    /// The tier a region is billed at, or `None` for a region Cloud Run
    /// publishes no rate for.
    ///
    /// `None` is an answer rather than a gap: the two tables above are the
    /// whole of Cloud Run's price list, so a region in neither is one the
    /// service does not offer — or one it has started offering since this
    /// table was read, which flyco must not price by assuming a tier.
    #[must_use]
    pub fn of(region: &str) -> Option<Self> {
        if TIER_ONE_REGIONS.contains(&region) {
            Some(Self::One)
        } else if TIER_TWO_REGIONS.contains(&region) {
            Some(Self::Two)
        } else {
            None
        }
    }

    /// Nanodollars per vCPU-second, on instance-based billing.
    #[must_use]
    pub const fn vcpu_nanos_per_second(self) -> u64 {
        match self {
            Self::One => 18_000,
            Self::Two => 21_600,
        }
    }

    /// Nanodollars per GiB-second, on instance-based billing.
    #[must_use]
    pub const fn memory_gib_nanos_per_second(self) -> u64 {
        match self {
            Self::One => 2_000,
            Self::Two => 2_400,
        }
    }

    /// What one hour of a size costs in this tier.
    ///
    /// Exact in integer arithmetic: every published rate is a whole number
    /// of nanodollars, and an hour of any of them is a whole number of
    /// microdollars.
    #[must_use]
    pub fn hourly(self, size: Size) -> Usd {
        let per_second = u64::from(size.vcpus) * self.vcpu_nanos_per_second()
            + u64::from(size.memory_gib) * self.memory_gib_nanos_per_second();
        Usd::from_micros(per_second * SECONDS_PER_HOUR / NANOS_PER_MICRO)
    }
}

/// One point on Cloud Run's vCPU × memory grid, as flyco offers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    /// Virtual CPUs.
    pub vcpus: u32,
    /// Memory in GiB, which is the unit Cloud Run's own limit is stated in.
    pub memory_gib: u32,
}

impl Size {
    /// The size flyco offers at a vCPU count.
    #[must_use]
    pub const fn of(vcpus: u32) -> Self {
        Self {
            vcpus,
            memory_gib: vcpus * MEMORY_GIB_PER_VCPU,
        }
    }

    /// Every size on the menu.
    #[must_use]
    pub fn offered() -> [Self; VCPU_SIZES.len()] {
        VCPU_SIZES.map(Self::of)
    }

    /// The size a machine type name describes, if it is one flyco offers.
    ///
    /// Both halves are checked rather than the prefix alone, so a request
    /// naming `cloudrun-8x8` — a shape Cloud Run would refuse and flyco
    /// never published — is refused here with a name in it instead of by
    /// the API with a quota message in it.
    #[must_use]
    pub fn parse(machine_type: &str) -> Option<Self> {
        let (vcpus, memory_gib) = machine_type
            .strip_prefix(MACHINE_TYPE_PREFIX)?
            .split_once('x')?;
        let size = Self::of(vcpus.parse().ok()?);
        (VCPU_SIZES.contains(&size.vcpus) && memory_gib.parse() == Ok(size.memory_gib))
            .then_some(size)
    }

    /// The machine type name this size is published under.
    #[must_use]
    pub fn machine_type(self) -> String {
        format!("{MACHINE_TYPE_PREFIX}{}x{}", self.vcpus, self.memory_gib)
    }

    /// The `cpu` limit, as Cloud Run spells a whole core count.
    #[must_use]
    pub fn cpu_limit(self) -> String {
        self.vcpus.to_string()
    }

    /// The `memory` limit, as Cloud Run spells gibibytes.
    #[must_use]
    pub fn memory_limit(self) -> String {
        format!("{}Gi", self.memory_gib)
    }
}

/// What Cloud Run offers in one region.
///
/// Computed rather than fetched, unlike the Compute Engine catalog: Cloud
/// Run publishes two rates and a region list, not a per-SKU price feed, so
/// there is nothing to read and a catalog call spends no API quota on it.
///
/// The entries carry [`CLOUD_RUN_FREE_GRANT`], which is what makes a
/// container worth preferring over hardware at all — see
/// [`auto_linux_choice`](flyco_core::machine::auto_linux_choice) — and no
/// spot price, because Cloud Run sells no interruptible capacity.
#[must_use]
pub fn catalog(region: &str, tier: Tier) -> Vec<MachineCatalogEntry> {
    Size::offered()
        .into_iter()
        .map(|size| MachineCatalogEntry {
            // Stamped by the control plane, which knows the row.
            account: None,
            provider: CloudProviderKind::Gcp,
            // A region, not a zone: Cloud Run places the execution itself
            // and a job has no zone to name.
            region: region.to_owned(),
            machine_type: size.machine_type(),
            runtime: Runtime::Container,
            free_grant: Some(CLOUD_RUN_FREE_GRANT),
            os: OsFamily::Linux,
            capacity: Some(MachineCapacity {
                vcpus: size.vcpus,
                memory_mib: u64::from(size.memory_gib) * 1_024,
            }),
            lineage: Some(MachineLineage {
                // Cloud Run runs `linux/amd64` images and nothing else.
                architecture: CpuArchitecture::X8664,
                // One family with no generations: these are four sizes of
                // one machine, which is exactly what a shared family key
                // means to curation.
                family: "cloudrun".to_owned(),
                generation: None,
            }),
            pricing: MachinePricing::Metered {
                on_demand_hourly: tier.hourly(size),
                spot_hourly: None,
                // Cloud Run bills a minimum of one minute per instance,
                // which is not a floor a session can notice and not one
                // [`BillingMinimum`](flyco_core::machine::BillingMinimum)
                // can state — it counts whole hours, for the day an EC2 Mac
                // commits to.
                minimum: None,
                // A job's filesystem is the in-memory one already paid for
                // by the memory rate: flyco requests no ephemeral disk, so
                // there is no second meter running.
                storage: StoragePricing::PerGibHourly { rate: Usd::ZERO },
            },
        })
        .collect()
}

/// A container machine's provider-native identity: the job that defines it
/// and the execution that is currently running it.
///
/// Both halves, because neither is enough. The job alone cannot be
/// cancelled and the execution alone cannot be re-run, and a `start` after a
/// stop produces a *different* execution of the same job — so the pair is
/// what [`Machine::native_id`] carries, spelled `<job>/<execution>`.
///
/// It is also how this driver tells its own two runtimes apart without a
/// field on [`Machine`]: a Compute Engine machine's native id is an absolute
/// `https://` URL, and [`Handle::parse`] accepts only a name flyco itself
/// derived from a [`MachineId`] followed by a single path segment. The two
/// shapes cannot be confused for one another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handle {
    /// The Cloud Run job, named from the machine id.
    pub job: String,
    /// The execution Cloud Run created for the current run, short name only.
    pub execution: String,
}

impl Handle {
    /// Reads a handle out of a machine's native id, or `None` when the id
    /// describes something that is not a Cloud Run job.
    #[must_use]
    pub fn parse(native_id: &str) -> Option<Self> {
        let (job, execution) = native_id.split_once('/')?;
        if execution.is_empty() || execution.contains('/') {
            return None;
        }
        // The job half has to be a name flyco derived, which is what keeps
        // this disjoint from every other id shape the driver produces.
        names::machine_named(job)?;
        Some(Self {
            job: job.to_owned(),
            execution: execution.to_owned(),
        })
    }
}

impl fmt::Display for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.job, self.execution)
    }
}

// ── URLs ──

/// The collection one project's jobs live in, in one region.
#[must_use]
pub fn jobs_url(project: &str, region: &str) -> String {
    format!("{RUN_BASE}/projects/{project}/locations/{region}/jobs")
}

/// One job's resource URL.
#[must_use]
pub fn job_url(project: &str, region: &str, job: &str) -> String {
    format!("{}/{job}", jobs_url(project, region))
}

/// One execution's resource URL.
#[must_use]
pub fn execution_url(project: &str, region: &str, job: &str, execution: &str) -> String {
    format!("{}/executions/{execution}", job_url(project, region, job))
}

/// Where to enable the Cloud Run API on a project.
///
/// Named in the refusal rather than described, because the way out of a
/// `SERVICE_DISABLED` runs through this exact page and nothing flyco can do
/// on the user's behalf gets there.
#[must_use]
pub fn enable_api_url(project: &str) -> String {
    format!("https://console.cloud.google.com/apis/library/run.googleapis.com?project={project}")
}

// ── Bodies ──

/// The ownership labels a job carries.
///
/// The same three keys the compute driver puts on an instance, so a project
/// shared with other work reads the same whichever runtime a session is on.
#[must_use]
pub fn labels(machine: MachineId, session: &str) -> BTreeMap<String, String> {
    [
        (OWNER_LABEL.to_owned(), PROVIDER.to_owned()),
        (SESSION_LABEL.to_owned(), session.to_owned()),
        (MACHINE_LABEL.to_owned(), machine.to_string()),
    ]
    .into_iter()
    .collect()
}

/// Body of `jobs.create` and `jobs.patch`.
#[derive(Debug, Clone, Serialize)]
pub struct Job {
    /// What one execution of it runs.
    pub template: ExecutionTemplate,
    /// Ownership labels, so a project shared with other work stays legible.
    /// The same three keys the compute driver puts on an instance.
    pub labels: BTreeMap<String, String>,
}

/// What one execution of a job runs.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionTemplate {
    /// Tasks per execution: one, which is the machine.
    pub task_count: u32,
    /// The task itself.
    pub template: TaskTemplate,
}

/// One task of an execution.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskTemplate {
    /// Retries of a failed task — see [`MAX_RETRIES`].
    pub max_retries: u32,
    /// How long the task may run, as a duration string.
    pub timeout: &'static str,
    /// Which sandbox it runs in — see [`EXECUTION_ENVIRONMENT`].
    pub execution_environment: &'static str,
    /// The one container it runs.
    pub containers: Vec<Container>,
}

/// The session container.
#[derive(Debug, Clone, Serialize)]
pub struct Container {
    /// The image, at the tag that matches this control plane's wire
    /// protocol.
    pub image: String,
    /// The size, which on Cloud Run is a property of the container.
    pub resources: Resources,
    /// The `flycod` configuration, base64, exactly as the host path passes
    /// it to Podman.
    pub env: Vec<EnvVar>,
}

/// A container's resource limits.
#[derive(Debug, Clone, Serialize)]
pub struct Resources {
    /// `cpu` and `memory`.
    pub limits: ResourceLimits,
}

/// The two limits Cloud Run accepts.
#[derive(Debug, Clone, Serialize)]
pub struct ResourceLimits {
    /// Whole vCPUs, as a string.
    pub cpu: String,
    /// Memory, e.g. `16Gi`.
    pub memory: String,
}

/// One environment variable.
#[derive(Debug, Clone, Serialize)]
pub struct EnvVar {
    /// Its name.
    pub name: &'static str,
    /// Its value.
    ///
    /// This is the session's whole `flycod` configuration — daemon token,
    /// harness credential and GitHub token inside it — and on Cloud Run it
    /// is stored **in the job definition**, readable by anyone the project
    /// grants `run.jobs.get`. That is the same exposure a VM's `user-data`
    /// already has, and the alternative is a Secret Manager secret per
    /// session, which is a second resource to create, grant and garbage
    /// collect for no change in who can read it. The job is deleted with
    /// the machine, which is what bounds it.
    pub value: String,
}

/// Body of `jobs.run`.
///
/// Empty: every override Cloud Run accepts here — task count, timeout,
/// container arguments — is already what the job says, and the one thing a
/// resize would want to override, the size, is not overridable at all.
/// Sent as `{}` rather than as no body, because that is what the method's
/// request message is.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct RunJobRequest {}

/// Body of `executions.cancel`, for the same reason.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct CancelExecutionRequest {}

// ── Responses ──

/// A long-running operation, as every mutating Cloud Run call answers.
///
/// Unlike Compute Engine's, this one carries `done` as a boolean rather than
/// a status string, and its in-flight name is a resource path rather than a
/// URL to poll — see [`Operation::poll_url`].
#[derive(Debug, Clone, Deserialize)]
pub struct Operation {
    /// Its resource name, `projects/…/locations/…/operations/…`.
    #[serde(default)]
    pub name: String,
    /// Whether it has finished, one way or another.
    #[serde(default)]
    pub done: bool,
    /// Present only on an operation that finished unsuccessfully.
    #[serde(default)]
    pub error: Option<Status>,
    /// The resource being operated on. On `jobs.run` this is the
    /// [`Execution`] the call created, which is the only place its name
    /// appears.
    #[serde(default)]
    pub metadata: Option<Execution>,
}

impl Operation {
    /// Where to poll this operation.
    #[must_use]
    pub fn poll_url(&self) -> String {
        format!("{RUN_BASE}/{}", self.name)
    }

    /// The failure it finished with, if it failed.
    #[must_use]
    pub fn failure(&self) -> Option<ProviderError> {
        self.error
            .as_ref()
            .map(|status| ProviderError::OperationFailed {
                status: "DONE".to_owned(),
                code: status.reason(),
                message: status.message.clone(),
            })
    }

    /// The short name of the execution this operation created.
    #[must_use]
    pub fn execution_name(&self) -> Option<String> {
        self.metadata.as_ref().and_then(Execution::short_name)
    }
}

/// One execution of a job, in the parts flyco reads.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Execution {
    /// Its full resource name,
    /// `projects/…/locations/…/jobs/…/executions/…`.
    #[serde(default)]
    pub name: String,
    /// When it finished. Absent while it is still running, which is the
    /// whole question this type is read for.
    #[serde(rename = "completionTime", default)]
    pub completion_time: Option<String>,
}

impl Execution {
    /// The last segment of the resource name, which is what a [`Handle`]
    /// carries.
    #[must_use]
    pub fn short_name(&self) -> Option<String> {
        self.name
            .rsplit('/')
            .next()
            .filter(|segment| !segment.is_empty())
            .map(ToOwned::to_owned)
    }

    /// Whether this execution is still running.
    #[must_use]
    pub const fn is_live(&self) -> bool {
        self.completion_time.is_none()
    }
}

/// One page of `executions.list`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ExecutionPage {
    /// The executions on this page.
    #[serde(default)]
    pub executions: Vec<Execution>,
    /// The next page, when the list is longer than one.
    #[serde(rename = "nextPageToken", default)]
    pub next_page_token: Option<String>,
}

/// A job as `jobs.get` reports it, in the one part a resize has to carry
/// over.
///
/// Flyco creates these jobs and nothing else writes to them, so the body
/// this module authors is a complete description of one — except for the
/// container's environment, which holds the session's `flycod`
/// configuration and cannot be re-derived without the bootstrap that a
/// resize no longer has. So a resize reads that back and re-sends
/// everything else.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct JobView {
    /// Its execution template.
    #[serde(default)]
    pub template: ExecutionTemplateView,
    /// Its ownership labels, carried over rather than rebuilt: they name the
    /// session, which a resize is not told and must not drop.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

/// The template half of a [`JobView`].
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ExecutionTemplateView {
    /// The task template.
    #[serde(default)]
    pub template: TaskTemplateView,
}

/// The task half of a [`JobView`].
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TaskTemplateView {
    /// Its containers.
    #[serde(default)]
    pub containers: Vec<ContainerView>,
}

/// The container half of a [`JobView`].
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ContainerView {
    /// The image it runs.
    #[serde(default)]
    pub image: String,
    /// Its environment, which is where the `flycod` configuration is.
    #[serde(default)]
    pub env: Vec<EnvVarView>,
}

/// One environment entry of a [`JobView`].
#[derive(Debug, Clone, Default, Deserialize)]
pub struct EnvVarView {
    /// Its name.
    #[serde(default)]
    pub name: String,
    /// Its value.
    #[serde(default)]
    pub value: String,
}

impl JobView {
    /// The value of one environment variable of the job's single container.
    #[must_use]
    pub fn env(&self, name: &str) -> Option<&str> {
        self.template
            .template
            .containers
            .first()?
            .env
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| entry.value.as_str())
    }

    /// The image the job's single container runs.
    #[must_use]
    pub fn image(&self) -> Option<&str> {
        self.template
            .template
            .containers
            .first()
            .map(|container| container.image.as_str())
            .filter(|image| !image.is_empty())
    }
}

// ── Errors ──

/// The document a refused Cloud Run call answers with.
#[derive(Debug, Clone, Deserialize)]
struct ErrorEnvelope {
    error: Status,
}

/// A `google.rpc.Status`, which is both the body of an HTTP refusal and the
/// failure inside a finished operation.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Status {
    /// Symbolic status, e.g. `ALREADY_EXISTS`. Present on an HTTP refusal;
    /// an operation's failure carries only the numeric code.
    #[serde(default)]
    pub status: String,
    /// Numeric `google.rpc.Code`.
    #[serde(default)]
    pub code: i32,
    /// Human-readable explanation.
    #[serde(default)]
    pub message: String,
    /// Typed details, whose `ErrorInfo` carries the reason a driver can act
    /// on — `SERVICE_DISABLED` is only ever found here.
    #[serde(default)]
    pub details: Vec<ErrorDetail>,
}

/// One entry of a status's `details`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ErrorDetail {
    /// The `ErrorInfo` reason, e.g. `SERVICE_DISABLED`.
    #[serde(default)]
    pub reason: String,
}

impl Status {
    /// The code a driver can act on.
    ///
    /// The `ErrorInfo` reason where there is one, the symbolic status
    /// otherwise, and the numeric code when the service gave neither — so
    /// the code a caller matches on is never an empty string that reads like
    /// a real one.
    #[must_use]
    pub fn reason(&self) -> String {
        self.details
            .iter()
            .map(|detail| detail.reason.clone())
            .find(|reason| !reason.is_empty())
            .or_else(|| Some(self.status.clone()).filter(|status| !status.is_empty()))
            .unwrap_or_else(|| self.code.to_string())
    }
}

/// The reason a project that has never used Cloud Run refuses everything.
pub const SERVICE_DISABLED: &str = "SERVICE_DISABLED";

/// The status a create gets when the job is already there.
pub const ALREADY_EXISTS: &str = "ALREADY_EXISTS";

/// Turns a refused response into an error that keeps its code.
///
/// One code is answered rather than reported: `SERVICE_DISABLED` means the
/// project has never turned Cloud Run on, which no retry and no other
/// machine type fixes, and the message says where to fix it.
#[must_use]
pub fn refusal(project: &str, response: &HttpResponse) -> ProviderError {
    let Ok(envelope) = response.json::<ErrorEnvelope>() else {
        return ProviderError::Rejected(format!(
            "Cloud Run answered HTTP {}: {}",
            response.status,
            response.body_text()
        ));
    };

    let code = envelope.error.reason();
    let message = if code == SERVICE_DISABLED {
        format!(
            "the Cloud Run API is not enabled on project {project}, so no container can be \
             started there; enable it at {}",
            enable_api_url(project)
        )
    } else {
        envelope.error.message
    };
    ProviderError::Refused { code, message }
}

/// The image a session runs in, at the tag that matches this control plane.
///
/// Pinned to the wire protocol rather than to `latest`, which the host path
/// may use because a host's daemon is upgraded with its image: a job flyco
/// starts must speak the protocol the control plane that started it speaks,
/// and `latest` would let a republished image break every running session's
/// relay at once.
#[must_use]
pub fn session_image() -> String {
    format!(
        "{}:wire-{}",
        flyco_core::release::SESSION_IMAGE,
        flyco_core::WIRE_PROTOCOL_VERSION
    )
}

/// What a create found when it asked Cloud Run for the job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobCreation {
    /// The job did not exist and now does.
    Created,
    /// A job of that name was already there, which on an at-least-once
    /// queue means this create is a redelivery.
    AlreadyExisted,
}

impl<T: HttpTransport, C: MonotonicClock, K: Timer, W: WallClock> GcpProvider<T, C, K, W> {
    /// Reads a Cloud Run resource, refusing anything but a success in Cloud
    /// Run's own error vocabulary.
    ///
    /// Separate from [`GcpProvider::get`](super::GcpProvider) only in that
    /// vocabulary: Compute Engine puts its actionable code in
    /// `error.errors[].reason` and Cloud Run in `error.details[].reason`,
    /// and a driver that read one document with the other's parser would
    /// report every refusal without a code.
    async fn read_run<R: serde::de::DeserializeOwned>(
        &mut self,
        url: String,
    ) -> Result<R, ProviderError> {
        let project = self.project().to_owned();
        self.fetch(HttpRequest::new(Method::Get, url), |response| {
            refusal(&project, response)
        })
        .await
    }

    /// Sends a mutating Cloud Run request and answers with the operation it
    /// started, without waiting for it.
    async fn start_run_operation(
        &mut self,
        request: HttpRequest,
    ) -> Result<Operation, ProviderError> {
        let project = self.project().to_owned();
        self.fetch(request, |response| refusal(&project, response))
            .await
    }

    /// Sends a mutating Cloud Run request and follows it to `done`.
    async fn send_run_and_await(&mut self, request: HttpRequest) -> Result<(), ProviderError> {
        let operation = self.start_run_operation(request).await?;
        self.await_run_operation(operation).await
    }

    /// Follows a Cloud Run operation to `done`.
    ///
    /// `done` alone is not success here either: the failure is inside the
    /// finished operation, exactly as it is on Compute Engine.
    async fn await_run_operation(&mut self, started: Operation) -> Result<(), ProviderError> {
        let mut operation = started;

        for attempt in 0..MAX_POLL_ATTEMPTS {
            if operation.done {
                return operation.failure().map_or(Ok(()), Err);
            }
            if operation.name.is_empty() {
                return Err(ProviderError::Malformed(
                    "Cloud Run started an operation without naming it, so it cannot be followed",
                ));
            }

            // Cloud Run states no `Retry-After`, so the shared backoff is
            // what paces this.
            self.timer.sleep(poll_delay(None, attempt)).await;
            let url = operation.poll_url();
            operation = self.read_run(url).await?;
        }

        Err(ProviderError::Rejected(format!(
            "a Cloud Run operation was still running after {MAX_POLL_ATTEMPTS} polls"
        )))
    }

    /// The job body for one provisioning request.
    fn job_body(request: &ProvisionRequest, size: Size) -> Result<Job, ProviderError> {
        let config = flycod::render(&request.bootstrap)
            .map_err(|_| ProviderError::Malformed("the flycod configuration did not render"))?;
        Ok(Self::job_body_with_config(
            size,
            session_image(),
            base64::engine::general_purpose::STANDARD.encode(config),
            labels(request.machine, &request.bootstrap.session.to_string()),
        ))
    }

    /// The job body from its parts, which is what a resize rebuilds it from.
    fn job_body_with_config(
        size: Size,
        image: String,
        config_base64: String,
        labels: BTreeMap<String, String>,
    ) -> Job {
        Job {
            template: ExecutionTemplate {
                task_count: TASK_COUNT,
                template: TaskTemplate {
                    max_retries: MAX_RETRIES,
                    timeout: TASK_TIMEOUT,
                    execution_environment: EXECUTION_ENVIRONMENT,
                    containers: vec![Container {
                        image,
                        resources: Resources {
                            limits: ResourceLimits {
                                cpu: size.cpu_limit(),
                                memory: size.memory_limit(),
                            },
                        },
                        env: vec![EnvVar {
                            name: CONFIG_ENV,
                            value: config_base64,
                        }],
                    }],
                },
            },
            labels,
        }
    }

    /// Creates the job, and says whether it was already there.
    async fn create_job(
        &mut self,
        region: &str,
        job: &str,
        body: &Job,
    ) -> Result<JobCreation, ProviderError> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("jobId", job)
            .finish();
        let url = format!("{}?{query}", jobs_url(self.project(), region));
        let response = self
            .send(HttpRequest::new(Method::Post, url).json_body(body)?)
            .await?;

        if response.is_success() {
            let operation: Operation = response.json()?;
            self.await_run_operation(operation).await?;
            return Ok(JobCreation::Created);
        }

        let error = refusal(self.project(), &response);
        if error.code() == Some(ALREADY_EXISTS) {
            Ok(JobCreation::AlreadyExisted)
        } else {
            Err(error)
        }
    }

    /// The execution of this job that is still running, if any.
    ///
    /// Paged rather than read one page deep: a session that has been stopped
    /// and started many times has an execution per start, and the live one
    /// is not promised to be on the first page.
    async fn live_execution(
        &mut self,
        region: &str,
        job: &str,
    ) -> Result<Option<String>, ProviderError> {
        let base = format!("{}/executions", job_url(self.project(), region, job));
        let mut next: Option<String> = None;

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

            let page: ExecutionPage = self.read_run(url).await?;
            if let Some(live) = page
                .executions
                .iter()
                .find(|execution| execution.is_live())
                .and_then(Execution::short_name)
            {
                return Ok(Some(live));
            }
            next = page.next_page_token.filter(|token| !token.is_empty());
            if next.is_none() {
                return Ok(None);
            }
        }
    }

    /// Starts an execution of the job and answers with its short name.
    ///
    /// The operation is deliberately not followed: it finishes when the
    /// *task* does, which for a session is up to [`TASK_TIMEOUT`] away. The
    /// execution's name is in the operation's metadata the moment Cloud Run
    /// accepts the call, which is everything the caller needs.
    async fn run_job(&mut self, region: &str, job: &str) -> Result<String, ProviderError> {
        let url = format!("{}:run", job_url(self.project(), region, job));
        let operation = self
            .start_run_operation(HttpRequest::new(Method::Post, url).json_body(&RunJobRequest {})?)
            .await?;
        operation.execution_name().ok_or(ProviderError::Malformed(
            "Cloud Run started a job without naming the execution it created",
        ))
    }

    /// Cancels an execution and waits for it to actually stop.
    ///
    /// Waited on, unlike the run: the caller's next step is either another
    /// execution of the same job or the job's deletion, and both of those
    /// against a task that is still writing its `workdir-patch` would lose
    /// the session's work.
    ///
    /// An execution that has already finished is left alone. That is not a
    /// swallowed failure — it is the read that precedes the write: a task
    /// that hit its timeout or died on its own is already stopped, and
    /// asking Cloud Run to cancel it would refuse a `deallocate` that has
    /// nothing left to do.
    async fn cancel_execution(
        &mut self,
        region: &str,
        handle: &Handle,
    ) -> Result<(), ProviderError> {
        let url = execution_url(self.project(), region, &handle.job, &handle.execution);
        let execution: Execution = self.read_run(url.clone()).await?;
        if !execution.is_live() {
            tracing::debug!(
                execution = %handle.execution,
                "the execution had already finished, so there was nothing to cancel"
            );
            return Ok(());
        }

        self.send_run_and_await(
            HttpRequest::new(Method::Post, format!("{url}:cancel"))
                .json_body(&CancelExecutionRequest {})?,
        )
        .await
    }

    /// The machine a job and one of its executions add up to.
    fn container_machine(machine: MachineId, region: &str, handle: &Handle) -> Machine {
        Machine {
            id: machine,
            native_id: handle.to_string(),
            region: region.to_owned(),
            state: MachineState::Running,
            // Cloud Run sells no interruptible capacity, so this is what was
            // asked for and what was obtained whatever the spec said.
            capacity_mode: CapacityMode::OnDemand,
            // A job has no inbound address and flyco never dials one: the
            // daemon opens the connection, from inside the container.
            address: None,
        }
    }

    /// The size a machine type names, or a refusal that names it back.
    fn offered_size(machine_type: &str, region: &str) -> Result<Size, ProviderError> {
        Size::parse(machine_type).ok_or_else(|| ProviderError::Unavailable {
            machine_type: machine_type.to_owned(),
            region: region.to_owned(),
            reason: "Cloud Run offers no job of that size".to_owned(),
        })
    }

    /// Provisions a session as a Cloud Run job.
    ///
    /// Create then run, and the create is idempotent because the queue that
    /// asks for it is at-least-once: a job of this machine's name already
    /// existing means this is a redelivery, and a redelivery that ran the
    /// job again would put a *second* `flycod` on the same session token.
    /// So a redelivery adopts the execution that is already running, and
    /// starts one only when there is none.
    pub(super) async fn provision_job(
        &mut self,
        request: &ProvisionRequest,
    ) -> Result<Machine, ProviderError> {
        let region = &request.spec.region;
        let size = Self::offered_size(&request.spec.machine_type, region)?;
        if Tier::of(region).is_none() {
            return Err(ProviderError::Unavailable {
                machine_type: request.spec.machine_type.clone(),
                region: region.clone(),
                reason: "Cloud Run publishes no price for this region".to_owned(),
            });
        }

        let job = names::machine(request.machine);
        let body = Self::job_body(request, size)?;

        let execution = match self.create_job(region, &job, &body).await? {
            JobCreation::Created => self.run_job(region, &job).await?,
            JobCreation::AlreadyExisted => match self.live_execution(region, &job).await? {
                Some(live) => {
                    tracing::info!(
                        machine = %request.machine,
                        execution = %live,
                        "a redelivered provision found the job already running; adopted it"
                    );
                    live
                }
                None => self.run_job(region, &job).await?,
            },
        };

        let handle = Handle { job, execution };
        tracing::info!(
            machine = %request.machine,
            machine_type = %request.spec.machine_type,
            %region,
            "provisioned a Cloud Run job"
        );
        Ok(Self::container_machine(request.machine, region, &handle))
    }

    /// Stop, resize the definition, start — which is what a resize is on a
    /// runtime whose size belongs to the execution.
    ///
    /// The stop is what writes the session's `workdir-patch`, and the start
    /// is what replays it onto a fresh clone, so the ordering here is the
    /// same promise an ordinary container stop and start make.
    pub(super) async fn resize_job(
        &mut self,
        machine: &Machine,
        handle: &Handle,
        new_machine_type: &str,
    ) -> Result<Machine, ProviderError> {
        let region = machine.region.clone();
        let size = Self::offered_size(new_machine_type, &region)?;

        self.cancel_execution(&region, handle).await?;

        // Everything about this job is flyco's own body except the
        // container's environment, which carries the session's `flycod`
        // configuration and cannot be re-derived without the bootstrap a
        // resize no longer holds. So it is read back and re-sent.
        let url = job_url(self.project(), &region, &handle.job);
        let view: JobView = self.read_run(url.clone()).await?;
        let config = view.env(CONFIG_ENV).ok_or(ProviderError::Malformed(
            "the Cloud Run job carries no flycod configuration, so resizing it would \
             start a machine that cannot reach its session",
        ))?;
        // The image is carried over rather than re-derived from
        // [`session_image`], although it could be: a resize changes the size
        // and nothing else. A session that came up on `wire-11` keeps
        // speaking `wire-11` across it, and bundling an image upgrade into a
        // size change would make one operation two.
        let image = view.image().ok_or(ProviderError::Malformed(
            "the Cloud Run job names no image, so there is nothing to resize",
        ))?;
        let body = Self::job_body_with_config(
            size,
            image.to_owned(),
            config.to_owned(),
            view.labels.clone(),
        );

        self.send_run_and_await(HttpRequest::new(Method::Patch, url).json_body(&body)?)
            .await?;

        let execution = self.run_job(&region, &handle.job).await?;
        tracing::info!(machine = %machine.id, %new_machine_type, "resized a Cloud Run job");
        Ok(Self::container_machine(
            machine.id,
            &region,
            &Handle {
                job: handle.job.clone(),
                execution,
            },
        ))
    }

    /// Cancels the running execution, keeping the job.
    pub(super) async fn deallocate_job(
        &mut self,
        machine: &Machine,
        handle: &Handle,
    ) -> Result<(), ProviderError> {
        let region = machine.region.clone();
        self.cancel_execution(&region, handle).await
    }

    /// Starts a new execution of the job the machine already has.
    ///
    /// A different execution from the one that stopped, on a fresh
    /// filesystem: the machine's native id changes, and the caller is told
    /// so by the [`Machine`] it gets back. A redelivered start adopts a live
    /// execution for the reason a redelivered provision does.
    pub(super) async fn start_job(
        &mut self,
        machine: &Machine,
        handle: &Handle,
    ) -> Result<Machine, ProviderError> {
        let region = machine.region.clone();
        let execution = match self.live_execution(&region, &handle.job).await? {
            Some(live) => {
                tracing::info!(
                    machine = %machine.id,
                    execution = %live,
                    "a redelivered start found the job already running; adopted it"
                );
                live
            }
            None => self.run_job(&region, &handle.job).await?,
        };

        Ok(Self::container_machine(
            machine.id,
            &region,
            &Handle {
                job: handle.job.clone(),
                execution,
            },
        ))
    }

    /// Cancels the execution and deletes the job.
    ///
    /// In that order, and the cancel comes first for the reason it does in a
    /// resize: a task still flushing its transcript is a session losing its
    /// last turn.
    pub(super) async fn destroy_job(
        &mut self,
        machine: &Machine,
        handle: &Handle,
    ) -> Result<(), ProviderError> {
        let region = machine.region.clone();
        self.cancel_execution(&region, handle).await?;
        self.send_run_and_await(HttpRequest::new(
            Method::Delete,
            job_url(self.project(), &region, &handle.job),
        ))
        .await?;

        tracing::info!(machine = %machine.id, "destroyed a Cloud Run job");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Handle, MEMORY_GIB_PER_VCPU, Operation, SERVICE_DISABLED, Size, TIER_ONE_REGIONS,
        TIER_TWO_REGIONS, Tier, catalog, refusal, session_image,
    };
    use crate::ProviderError;
    use crate::http::HttpResponse;
    use flyco_core::MachineId;
    use flyco_core::machine::{MachinePricing, Runtime};
    use flyco_core::money::Usd;

    const PROJECT: &str = "flyco-sessions";

    #[test]
    fn a_size_round_trips_through_the_name_it_is_published_under() {
        for size in Size::offered() {
            let name = size.machine_type();
            assert_eq!(Size::parse(&name), Some(size), "`{name}` must parse back");
            assert_eq!(size.memory_gib, size.vcpus * MEMORY_GIB_PER_VCPU);
        }
        assert_eq!(Size::of(4).machine_type(), "cloudrun-4x16");
        assert_eq!(Size::of(8).memory_limit(), "32Gi");
        assert_eq!(Size::of(8).cpu_limit(), "8");
    }

    #[test]
    fn a_shape_flyco_never_published_is_not_a_size() {
        // The memory has to agree with the vCPU count, or the name describes
        // a machine Cloud Run would refuse.
        assert_eq!(Size::parse("cloudrun-4x8"), None);
        // And the vCPU count has to be one that is offered.
        assert_eq!(Size::parse("cloudrun-16x64"), None);
        assert_eq!(Size::parse("cloudrun-0x0"), None);
        // A Compute Engine type is not a Cloud Run one.
        assert_eq!(Size::parse("e2-standard-2"), None);
        assert_eq!(Size::parse("cloudrun-"), None);
    }

    #[test]
    fn an_hour_is_the_published_per_second_rate_times_three_thousand_six_hundred() {
        // $0.000018/vCPU-s and $0.000002/GiB-s in Tier 1
        // (cloud.google.com/run/pricing, Jobs): four cores and sixteen
        // gibibytes is $0.3744 an hour.
        assert_eq!(
            Tier::One.hourly(Size::of(4)),
            Usd::from_micros(4 * 64_800 + 16 * 7_200)
        );
        assert_eq!(Tier::One.hourly(Size::of(4)), Usd::from_micros(374_400));
        // Tier 2 is exactly a fifth dearer on both meters.
        assert_eq!(Tier::Two.hourly(Size::of(4)), Usd::from_micros(449_280));
        assert_eq!(Tier::One.hourly(Size::of(8)), Usd::from_micros(748_800));
    }

    #[test]
    fn a_region_is_billed_at_one_tier_and_an_unlisted_one_at_none() {
        assert_eq!(Tier::of("us-central1"), Some(Tier::One));
        assert_eq!(Tier::of("europe-west4"), Some(Tier::One));
        assert_eq!(Tier::of("asia-southeast1"), Some(Tier::Two));
        assert_eq!(Tier::of("europe-west3"), Some(Tier::Two));
        // Cloud Run publishes no rate for it, so flyco quotes none.
        assert_eq!(Tier::of("us-east7"), None);
        assert_eq!(Tier::of("us-central1-a"), None);

        // The two tables are disjoint: a region in both would have two
        // prices and the first match would silently win.
        for region in TIER_TWO_REGIONS {
            assert!(
                !TIER_ONE_REGIONS.contains(&region),
                "`{region}` is in both tier tables"
            );
        }
    }

    #[test]
    fn every_catalog_entry_is_a_granted_container_with_no_spot_price() {
        let entries = catalog("us-central1", Tier::One);
        assert_eq!(entries.len(), Size::offered().len());

        for entry in &entries {
            assert_eq!(entry.runtime, Runtime::Container);
            assert_eq!(entry.region, "us-central1");
            assert_eq!(entry.free_grant, Some(super::CLOUD_RUN_FREE_GRANT));
            // No interruptible market exists here, so quoting a spot price
            // would be quoting one nobody can buy.
            assert!(!entry.pricing.offers_spot());
            let MachinePricing::Metered { storage, .. } = &entry.pricing else {
                panic!("a Cloud Run job is metered");
            };
            // The filesystem is the in-memory one the memory rate already
            // pays for, so no disk size is billed twice.
            assert_eq!(storage.hourly(200), Some(Usd::ZERO));
        }

        let largest = entries.last().expect("the catalog is not empty");
        assert_eq!(largest.machine_type, "cloudrun-8x32");
        assert_eq!(
            largest.capacity.as_ref().expect("a size").memory_mib,
            32 * 1_024
        );
    }

    #[test]
    fn a_handle_is_only_ever_a_job_flyco_named_and_one_execution() {
        let machine = MachineId::generate();
        let native = format!("flyco-{machine}/flyco-{machine}-abcd");
        let handle = Handle::parse(&native).expect("a Cloud Run handle");
        assert_eq!(handle.job, format!("flyco-{machine}"));
        assert_eq!(handle.to_string(), native);

        // A Compute Engine machine's native id is an absolute URL, which is
        // what keeps the two runtimes' identities disjoint without a field
        // on `Machine`.
        assert_eq!(
            Handle::parse(
                "https://compute.googleapis.com/compute/v1/projects/flyco-sessions\
                 /zones/us-central1-a/instances/flyco-1"
            ),
            None
        );
        assert_eq!(Handle::parse(&format!("flyco-{machine}")), None);
        assert_eq!(Handle::parse(&format!("flyco-{machine}/")), None);
        assert_eq!(Handle::parse("not-a-machine/exec"), None);
    }

    #[test]
    fn a_disabled_api_is_refused_with_the_page_that_enables_it() {
        let response = HttpResponse::new(
            403,
            include_bytes!("../../fixtures/gcp/run_error_service_disabled.json").to_vec(),
        );
        let error = refusal(PROJECT, &response);
        assert_eq!(error.code(), Some(SERVICE_DISABLED));
        let message = error.to_string();
        assert!(
            message.contains(
                "https://console.cloud.google.com/apis/library/run.googleapis.com\
                 ?project=flyco-sessions"
            ),
            "the refusal names the page that fixes it: {message}"
        );
    }

    #[test]
    fn an_already_existing_job_is_refused_with_a_code_a_driver_can_act_on() {
        let response = HttpResponse::new(
            409,
            include_bytes!("../../fixtures/gcp/run_error_already_exists.json").to_vec(),
        );
        assert_eq!(refusal(PROJECT, &response).code(), Some("ALREADY_EXISTS"));
    }

    #[test]
    fn a_refusal_that_is_not_a_google_document_keeps_its_status_and_body() {
        let error = refusal(
            PROJECT,
            &HttpResponse::new(502, b"<html>gateway</html>".to_vec()),
        );
        assert!(matches!(error, ProviderError::Rejected(_)));
        assert!(error.to_string().contains("502"));
    }

    #[test]
    fn a_run_operation_names_the_execution_it_created_before_it_is_done() {
        let operation: Operation = serde_json::from_str(include_str!(
            "../../fixtures/gcp/run_operation_started.json"
        ))
        .expect("the operation fixture parses");
        assert!(!operation.done);
        assert_eq!(
            operation.execution_name().as_deref(),
            Some("flyco-session-x8k2p")
        );
        assert_eq!(
            operation.poll_url(),
            "https://run.googleapis.com/v2/projects/flyco-sessions/locations\
             /us-central1/operations/6f2a1c0e-run"
        );
    }

    #[test]
    fn a_done_operation_that_carries_an_error_is_a_failure() {
        let operation: Operation =
            serde_json::from_str(include_str!("../../fixtures/gcp/run_operation_failed.json"))
                .expect("the operation fixture parses");
        assert!(operation.done);
        let failure = operation
            .failure()
            .expect("a finished failure is a failure");
        assert_eq!(failure.code(), Some("QUOTA_EXCEEDED"));
    }

    #[test]
    fn the_image_is_pinned_to_this_control_planes_wire_protocol() {
        assert_eq!(
            session_image(),
            format!(
                "ghcr.io/lexoliu/flyco-session:wire-{}",
                flyco_core::WIRE_PROTOCOL_VERSION
            )
        );
    }
}
