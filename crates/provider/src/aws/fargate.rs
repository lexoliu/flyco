//! ECS on Fargate: the same AWS account, selling a container instead of a
//! virtual machine.
//!
//! One axis on the machine rather than a second driver (issue #235), so
//! everything a Fargate session needs that EC2 already gave it — the `SigV4`
//! signature, the JSON-RPC envelope, the region-access gate, the workspace
//! network, the `flycod` configuration, the `flyco-<machine>` naming — comes
//! from [`super::AwsProvider`] unchanged. What is here is the part that is
//! genuinely a different service: its `X-Amz-Target`s, its bodies, its
//! published rates and the five lifecycle operations, which are not the EC2
//! ones with different nouns.
//!
//! # A task definition is the machine's shape; a task is the machine
//!
//! `RegisterTaskDefinition` writes a **revision** and starts nothing;
//! `RunTask` starts a **task**, and that task is what the session runs on. It
//! holds the vCPU and the memory, it is what is stopped to stop the session,
//! and its filesystem is what disappears when it ends — so a `start` after a
//! `deallocate` is a *new* task rather than the old one resuming, and
//! [`Machine::native_id`] is the task's ARN.
//!
//! The definition is not carried in the id beside it, unlike the Cloud Run
//! driver's `<job>/<execution>` pair, because on ECS it does not have to be:
//! the family is [`names::machine`], derived from the machine id, and the
//! revision to run is whichever one is `ACTIVE` — which is exactly what a
//! resize changes. So the pair is recovered from the id rather than stored
//! in it, and a row written before a resize still names the right shape.
//!
//! # Nothing survives a stop but the patch
//!
//! A task's filesystem ends with the task, so a container session's working
//! tree travels as the `workdir-patch` `flycod` writes on `SIGTERM` and
//! replays onto a fresh clone at the next start — the mechanism
//! [`flyco_core::Runtime`] exists to name. The container therefore declares a
//! [`STOP_TIMEOUT_SECONDS`] of two minutes: Fargate's default is thirty
//! seconds, flyco's own shutdown is bounded at twenty-five, and the margin
//! costs nothing while a `SIGKILL` mid-patch costs the session its last turn.
//!
//! # Spot first, on demand second, and the driver has to do it itself
//!
//! `RunTask` names one capacity provider, `FARGATE_SPOT` when the spec asked
//! for interruptible capacity. AWS is explicit that Fargate does **not**
//! replace unavailable Spot capacity with on-demand
//! ([docs.aws.amazon.com/AmazonECS/latest/developerguide/fargate-capacity-providers.html](https://docs.aws.amazon.com/AmazonECS/latest/developerguide/fargate-capacity-providers.html)),
//! so the identical body is re-sent naming `FARGATE` — the same rule the EC2
//! path follows for a refused spot request. What makes this one different is
//! *where the refusal is*: `RunTask` answers **HTTP 200** with an empty
//! `tasks` and a `failures` entry whose reason is
//! [`CAPACITY_UNAVAILABLE`], so a driver that only inspected status codes
//! would report a machine it never started.
//!
//! # Nothing is orphaned
//!
//! There is nothing to orphan: a task owns no disk and no address, and the
//! two things that outlive it — the region's cluster and its task-definition
//! revisions — are flyco's own. [`AwsProvider::destroy`] deregisters and
//! then deletes every revision of the machine's family, because deregistering
//! alone leaves an `INACTIVE` revision behind for ever. The cluster is
//! shared by every container in the region, exactly as the security group is,
//! and so outlives any one machine.
//!
//! # No execution role, no task role
//!
//! Both are omitted deliberately. An execution role is what lets the Fargate
//! agent pull from a private registry and write to `CloudWatch` Logs; the
//! session image is public and the session's logs go to the control plane
//! over the relay, so a role would be an IAM resource to create, grant and
//! garbage-collect for no capability flyco uses. A task role would be an AWS
//! identity *inside* the session, which is the one thing a coding agent must
//! not be handed by default.

use flyco_core::MachineId;
use flyco_core::machine::{
    CloudProviderKind, CpuArchitecture, MachineCapacity, MachineCatalogEntry, MachineLineage,
    MachinePricing, MachineSpec, MachineState, OsFamily, Runtime, StoragePricing,
};
use flyco_core::money::Usd;
use flyco_core::release;
use serde::{Deserialize, Serialize};

use super::pricing::{FargatePrices, FargateRates};
use super::sigv4::Scope;
use super::{
    AwsProvider, MACHINE_TAG, OWNER_TAG, PROVIDER, RegionNetwork, SESSION_TAG, ec2, names,
};
use crate::clock::{MonotonicClock, Timer, WallClock};
use crate::host::{CONFIG_ENV, encoded_config};
use crate::http::HttpTransport;
use crate::polling::{MAX_POLL_ATTEMPTS, poll_delay};
use crate::{CapacityMode, Machine, ProviderError, ProvisionRequest};

// ── The service ──

/// Signing name and endpoint prefix of the ECS API.
pub const SERVICE: &str = "ecs";

/// Prefix of every ECS `X-Amz-Target`.
///
/// The date is the API's own version, `2014-11-13`, spelled the way the
/// service spells it: the target is what selects the operation, and there is
/// no other version of it.
pub const TARGET_PREFIX: &str = "AmazonEC2ContainerServiceV20141113.";

/// The one cluster a region's flyco containers run in.
///
/// A cluster is a namespace rather than capacity — on Fargate it owns no
/// instances and costs nothing — so one per region, created lazily, is the
/// counterpart of the workspace security group rather than of a machine.
pub const CLUSTER: &str = "flyco";

/// The interruptible capacity provider.
pub const SPOT_PROVIDER: &str = "FARGATE_SPOT";

/// The ordinary capacity provider.
pub const ON_DEMAND_PROVIDER: &str = "FARGATE";

/// The launch types this cluster is created with.
///
/// Both, because a cluster refuses a capacity provider it was not associated
/// with, and the spot fallback needs the other one to be there already.
pub const CAPACITY_PROVIDERS: [&str; 2] = [ON_DEMAND_PROVIDER, SPOT_PROVIDER];

/// The Fargate platform version a task asks for.
///
/// `LATEST`, which resolves to the newest generally available version —
/// currently 1.4.0 or later, and 1.4.0 is the floor for Graviton and for
/// Graviton on Spot, so pinning an older one would silently narrow the menu.
pub const PLATFORM_VERSION: &str = "LATEST";

/// The only network mode Fargate offers, stated because the API requires it.
pub const NETWORK_MODE: &str = "awsvpc";

/// The launch type the definition declares compatibility with.
pub const FARGATE_COMPATIBILITY: &str = "FARGATE";

/// The operating-system family every flyco container runs.
pub const OPERATING_SYSTEM_FAMILY: &str = "LINUX";

/// Name of the one container in the task.
pub const CONTAINER_NAME: &str = "session";

/// Tasks per `RunTask`: one, which is the machine.
pub const TASK_COUNT: u32 = 1;

/// Weight of the single capacity provider in a strategy.
///
/// One provider at a positive weight, rather than both at once: a strategy
/// naming Spot *and* on-demand would let ECS place the task on either, and
/// flyco would not know which market it is paying for until it read the task
/// back. The fallback is a second call, so the answer is never ambiguous.
pub const PROVIDER_WEIGHT: u32 = 1;

/// Whether the task's interface gets a public address.
///
/// `ENABLED`, and nothing listens on it. A task in a subnet with no NAT
/// gateway has no route to the internet without one, and every session needs
/// one: to fetch its repository, to reach its harness's API and to open the
/// relay to the control plane. The security group admits nothing inbound
/// that the container is listening on, so the address is an exit rather than
/// an entrance.
pub const ASSIGN_PUBLIC_IP: &str = "ENABLED";

/// Where the definition's tags go when a task is run from it.
pub const PROPAGATE_FROM_DEFINITION: &str = "TASK_DEFINITION";

/// How long the platform waits between `SIGTERM` and `SIGKILL`.
///
/// Two minutes, which is the documented maximum a container definition may
/// state and four times what flyco's own container shutdown is bounded at
/// (25 s: interrupt, flush, write the `workdir-patch`, report). Fargate's
/// default is thirty seconds, which would leave five, and a `SIGKILL` that
/// lands mid-patch costs the session the turn it was in.
pub const STOP_TIMEOUT_SECONDS: u32 = 120;

/// Reason flyco gives ECS for stopping a task, which the console shows.
pub const STOP_REASON: &str = "flyco stopped this session's machine";

/// The `lastStatus` of a task that has finished.
pub const STOPPED: &str = "STOPPED";

/// The failure reason that means "no capacity in this market right now".
///
/// Matched as a prefix because the rest of it names an availability zone:
/// `Capacity is unavailable at this time. Please try again later or in a
/// different availability zone`. On a Spot request this is the instruction to
/// ask again on demand; on an on-demand one it is
/// [`ProviderError::NoCapacity`] and the end of the attempt.
pub const CAPACITY_UNAVAILABLE: &str = "Capacity is unavailable";

/// The task states a `ListTasks` adoption looks for.
pub const DESIRED_RUNNING: &str = "RUNNING";

/// Definition revisions that can still be run.
pub const ACTIVE: &str = "ACTIVE";

/// Newest revision first.
pub const NEWEST_FIRST: &str = "DESC";

/// Revisions one `DeleteTaskDefinitions` may name.
pub const DELETE_BATCH: usize = 10;

/// What a describe has to ask for to be told a definition's tags.
pub const INCLUDE_TAGS: &str = "TAGS";

// ── Sizes ──

/// Prefix every Fargate machine type carries.
pub const MACHINE_TYPE_PREFIX: &str = "fargate-";

/// What a Graviton machine type carries after the prefix.
///
/// The architecture is part of the *name* because it is part of the price and
/// part of the image: `fargate-4x8` and `fargate-arm64-4x8` are two entries
/// in the same region at two rates, and a resize between them would swap the
/// instruction set under a running session.
pub const ARM_INFIX: &str = "arm64-";

/// CPU units Fargate charges for one vCPU.
pub const CPU_UNITS_PER_VCPU: u32 = 1_024;

/// Mebibytes in a gibibyte, which is the unit Fargate states memory in.
pub const MIB_PER_GIB: u64 = 1_024;

/// vCPU counts flyco offers a session.
///
/// Fargate's whole grid starts at a quarter of a core; the fractions are left
/// off for the reason Cloud Run's are — a fraction of a core builds nothing —
/// and every size Fargate sells above them is on the menu.
pub const VCPU_SIZES: [u32; 5] = [1, 2, 4, 8, 16];

/// GiB of memory per vCPU on the smaller sizes.
pub const MEMORY_GIB_PER_VCPU: u32 = 2;

/// GiB of memory per vCPU from [`LARGE_VCPUS`] upwards.
pub const LARGE_MEMORY_GIB_PER_VCPU: u32 = 4;

/// The vCPU count at which the memory ratio doubles.
///
/// Two GiB a core is what a session needs to run; four is what it needs to
/// *link*, and the sizes an agent reaches for when a build runs out of memory
/// are the large ones. Doubling the ratio at the bottom of the menu instead
/// would double the price of every small session for memory it never touches.
pub const LARGE_VCPUS: u32 = 8;

/// Fargate's own vCPU-to-memory table: `(vCPUs, min GiB, max GiB)`.
///
/// From
/// [docs.aws.amazon.com/AmazonECS/latest/developerguide/task-cpu-memory-error.html](https://docs.aws.amazon.com/AmazonECS/latest/developerguide/task-cpu-memory-error.html),
/// read 2026-09-09. Held here rather than in a comment so
/// [`Size::is_offerable`] can prove every size on the menu is one the API
/// would accept — the alternative is discovering an illegal pair as an
/// `InvalidParameterException` when a session tries to start.
pub const CPU_MEMORY_RANGES: [(u32, u32, u32); 5] = [
    (1, 2, 8),
    (2, 4, 16),
    (4, 8, 30),
    (8, 16, 60),
    (16, 32, 120),
];

/// Ephemeral disk every Fargate task gets without asking, and without being
/// billed for it.
pub const FREE_EPHEMERAL_GIB: u32 = 20;

/// The most ephemeral disk a task may ask for.
pub const MAX_EPHEMERAL_GIB: u32 = 200;

/// One point on Fargate's vCPU × memory grid, as flyco offers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    /// Virtual CPUs.
    pub vcpus: u32,
    /// Memory in GiB.
    pub memory_gib: u32,
}

impl Size {
    /// The size flyco offers at a vCPU count.
    #[must_use]
    pub const fn of(vcpus: u32) -> Self {
        let ratio = if vcpus >= LARGE_VCPUS {
            LARGE_MEMORY_GIB_PER_VCPU
        } else {
            MEMORY_GIB_PER_VCPU
        };
        Self {
            vcpus,
            memory_gib: vcpus * ratio,
        }
    }

    /// Every size on the menu.
    #[must_use]
    pub fn offered() -> [Self; VCPU_SIZES.len()] {
        VCPU_SIZES.map(Self::of)
    }

    /// Whether Fargate would accept this pair at all.
    ///
    /// Both halves of the question: a vCPU count the service does not sell,
    /// and a memory size outside the range it pairs with that count, are the
    /// same `InvalidParameterException` — and flyco publishes neither.
    #[must_use]
    pub fn is_offerable(self) -> bool {
        CPU_MEMORY_RANGES.iter().any(|&(vcpus, min_gib, max_gib)| {
            vcpus == self.vcpus && self.memory_gib >= min_gib && self.memory_gib <= max_gib
        })
    }

    /// The `cpu` field, in the CPU units Fargate bills.
    #[must_use]
    pub fn cpu_units(self) -> String {
        (self.vcpus * CPU_UNITS_PER_VCPU).to_string()
    }

    /// The `memory` field, in the mebibytes Fargate states.
    #[must_use]
    pub fn memory_mib(self) -> u64 {
        u64::from(self.memory_gib) * MIB_PER_GIB
    }

    /// What one hour of this size costs at a region's rates.
    ///
    /// The two meters multiplied out and added: Fargate sells vCPU-hours and
    /// GiB-hours, and a size is a point on both.
    #[must_use]
    pub fn hourly(self, rates: FargateRates) -> Usd {
        Usd::from_micros(
            rates
                .vcpu_hourly
                .micros()
                .saturating_mul(u64::from(self.vcpus))
                .saturating_add(
                    rates
                        .memory_gib_hourly
                        .micros()
                        .saturating_mul(u64::from(self.memory_gib)),
                ),
        )
    }
}

/// A size and the instruction set it runs, which together are one machine
/// type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    /// The size.
    pub size: Size,
    /// The instruction set, which is a different price and a different image.
    pub architecture: CpuArchitecture,
}

impl Shape {
    /// Every shape flyco offers: each size on both architectures.
    #[must_use]
    pub fn offered() -> Vec<Self> {
        [CpuArchitecture::X8664, CpuArchitecture::Arm64]
            .into_iter()
            .flat_map(|architecture| {
                Size::offered()
                    .into_iter()
                    .map(move |size| Self { size, architecture })
            })
            .collect()
    }

    /// The machine type name this shape is published under.
    #[must_use]
    pub fn machine_type(self) -> String {
        let infix = match self.architecture {
            CpuArchitecture::X8664 => "",
            CpuArchitecture::Arm64 => ARM_INFIX,
        };
        format!(
            "{MACHINE_TYPE_PREFIX}{infix}{}x{}",
            self.size.vcpus, self.size.memory_gib
        )
    }

    /// The shape a machine type name describes, if it is one flyco offers.
    ///
    /// Every part is checked rather than the prefix alone, so a name flyco
    /// never published — `fargate-4x16`, a legal Fargate pair that is not on
    /// this menu, or `fargate-32x128`, which is not a legal one — is refused
    /// here with the name in it rather than by the API with a parameter
    /// message in it.
    #[must_use]
    pub fn parse(machine_type: &str) -> Option<Self> {
        let rest = machine_type.strip_prefix(MACHINE_TYPE_PREFIX)?;
        let (architecture, rest) = rest
            .strip_prefix(ARM_INFIX)
            .map_or((CpuArchitecture::X8664, rest), |graviton| {
                (CpuArchitecture::Arm64, graviton)
            });
        let (vcpus, memory_gib) = rest.split_once('x')?;
        let size = Size::of(vcpus.parse().ok()?);
        (VCPU_SIZES.contains(&size.vcpus) && memory_gib.parse() == Ok(size.memory_gib))
            .then_some(Self { size, architecture })
    }

    /// The architecture as `runtimePlatform` spells it.
    #[must_use]
    pub const fn cpu_architecture(self) -> &'static str {
        match self.architecture {
            CpuArchitecture::X8664 => "X86_64",
            CpuArchitecture::Arm64 => "ARM64",
        }
    }

    /// The architecture ECS named in a definition it answered with.
    ///
    /// `None` for anything else, which for flyco's own definitions cannot
    /// happen and for a hand-edited one is a refusal rather than a guess.
    #[must_use]
    pub fn architecture_named(spelled: &str) -> Option<CpuArchitecture> {
        match spelled {
            "X86_64" => Some(CpuArchitecture::X8664),
            "ARM64" => Some(CpuArchitecture::Arm64),
            _ => None,
        }
    }
}

/// What Fargate offers in one region, at the rates it publishes there.
///
/// Computed rather than fetched, like Cloud Run's: the sizes are flyco's own
/// curation of a grid, and the only reads behind this are the three price
/// meters. An architecture the region publishes no rate pair for is left out
/// entirely — Graviton Fargate reached the regions at its own pace, and a
/// machine flyco cannot price is one it must not offer — and so is the whole
/// region when it publishes no ephemeral-storage rate, because a session's
/// disk is sized from its spec and would otherwise be billed at a rate flyco
/// invented.
///
/// **The spot price quoted is the on-demand rate**, which is not a mistake
/// and not a discount flyco made up. AWS publishes no Fargate Spot rate
/// anywhere a program can read it: there is no Spot meter under `AmazonECS`,
/// no Fargate offer code beside it, and
/// [aws.amazon.com/fargate/pricing](https://aws.amazon.com/fargate/pricing/)
/// says only that Spot prices are "set by AWS Fargate and adjust gradually",
/// at up to 70% off. A catalog with no spot price at all would be a catalog
/// flyco never asks Spot for — `POST /v1/sessions` drops a spot request an
/// entry does not offer — so the on-demand rate is quoted as what it is: the
/// published **ceiling** on what an hour of Spot can cost. The session is run
/// on Fargate Spot and billed by AWS at less than flyco quoted, never more.
#[must_use]
pub fn catalog(region: &str, prices: &FargatePrices) -> Vec<MachineCatalogEntry> {
    let Some(ephemeral_gib_hourly) = prices.ephemeral_gib_hourly else {
        return Vec::new();
    };

    Shape::offered()
        .into_iter()
        .filter_map(|shape| {
            let rates = prices.rates(shape.architecture)?;
            let hourly = shape.size.hourly(rates);
            Some(MachineCatalogEntry {
                // Stamped by the control plane, which knows the row.
                account: None,
                provider: CloudProviderKind::Aws,
                // A region, not an availability zone: ECS places the task
                // itself, across every subnet the request names.
                region: region.to_owned(),
                location: super::locations::of(region),
                machine_type: shape.machine_type(),
                runtime: Runtime::Container,
                // Fargate gives nothing away: the free tier covers ECS's own
                // control plane, which is free on every account, and not a
                // second of task capacity.
                free_grant: None,
                os: OsFamily::Linux,
                capacity: Some(MachineCapacity {
                    vcpus: shape.size.vcpus,
                    memory_mib: shape.size.memory_mib(),
                }),
                lineage: Some(MachineLineage {
                    architecture: shape.architecture,
                    // One family with no generations: these are five sizes of
                    // one machine, which is what a shared family key means to
                    // curation. The architecture is the axis it is ranked
                    // across, and it is a field of its own.
                    family: "fargate".to_owned(),
                    generation: None,
                }),
                pricing: MachinePricing::Metered {
                    on_demand_hourly: hourly,
                    spot_hourly: Some(hourly),
                    // Per second past a one-minute floor, which is not a
                    // floor a session can notice and not one
                    // `BillingMinimum` can state — it counts whole hours,
                    // for the day an EC2 Mac commits to.
                    minimum: None,
                    // The first `FREE_EPHEMERAL_GIB` of a task's disk are
                    // free and the rest is metered, which `StoragePricing`
                    // has no shape for. The metered rate is quoted for every
                    // GiB rather than none, so the quote is at most
                    // `FREE_EPHEMERAL_GIB` × this rate — a fifth of a cent an
                    // hour — *above* what AWS bills, never below it.
                    storage: StoragePricing::PerGibHourly {
                        rate: ephemeral_gib_hourly,
                    },
                },
            })
        })
        .collect()
}

// ── Request bodies ──

/// One ownership tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tag {
    /// Its key.
    pub key: String,
    /// Its value.
    pub value: String,
}

/// The tags a machine's definition and task carry.
///
/// The same three keys the EC2 path puts on an instance, so an account shared
/// with other work reads the same whichever runtime a session is on. `Name`
/// is left off: it is what the EC2 console labels an instance with, and a
/// task is labelled by the family it was run from, which is this machine's
/// name already.
#[must_use]
pub fn tags(machine: MachineId, session: &str) -> Vec<Tag> {
    vec![
        Tag {
            key: OWNER_TAG.to_owned(),
            value: PROVIDER.to_owned(),
        },
        Tag {
            key: SESSION_TAG.to_owned(),
            value: session.to_owned(),
        },
        Tag {
            key: MACHINE_TAG.to_owned(),
            value: machine.to_string(),
        },
    ]
}

/// Body of `CreateCluster`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateCluster {
    /// The cluster's name, which is also how it is addressed later.
    pub cluster_name: &'static str,
    /// The capacity providers it may place tasks on.
    pub capacity_providers: [&'static str; 2],
}

/// Body of `DescribeClusters`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DescribeClusters {
    /// Names or ARNs to describe.
    pub clusters: [&'static str; 1],
}

/// Body of `RegisterTaskDefinition`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterTaskDefinition {
    /// The family, which is the machine's name: a resize adds a revision to
    /// it rather than creating a second definition.
    pub family: String,
    /// `FARGATE`.
    pub requires_compatibilities: [&'static str; 1],
    /// `awsvpc`, the only mode Fargate has.
    pub network_mode: &'static str,
    /// CPU units for the whole task.
    pub cpu: String,
    /// Mebibytes for the whole task, as a string, which is how the API takes
    /// it.
    pub memory: String,
    /// Which kernel and instruction set the platform runs it on.
    pub runtime_platform: RuntimePlatform,
    /// The ephemeral disk, when one larger than the free allowance was asked
    /// for. Absent means the free [`FREE_EPHEMERAL_GIB`], which is also the
    /// only size the API will not accept explicitly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ephemeral_storage: Option<EphemeralStorage>,
    /// The one container.
    pub container_definitions: Vec<ContainerDefinition>,
    /// Ownership tags, propagated to every task run from this revision.
    pub tags: Vec<Tag>,
}

/// The platform a task definition runs on.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimePlatform {
    /// `LINUX`.
    pub operating_system_family: &'static str,
    /// `X86_64` or `ARM64`.
    pub cpu_architecture: &'static str,
}

/// A task's ephemeral disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EphemeralStorage {
    /// Gibibytes, between [`FREE_EPHEMERAL_GIB`] + 1 and
    /// [`MAX_EPHEMERAL_GIB`].
    ///
    /// Spelled out rather than left to `rename_all`, because AWS capitalises
    /// the `B` of `sizeInGiB` and a name that merely looks right is a field
    /// the service silently ignores on the way in and this driver fails to
    /// read on the way back.
    #[serde(rename = "sizeInGiB")]
    pub size_in_gib: u32,
}

/// The session container.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerDefinition {
    /// `session`.
    pub name: &'static str,
    /// The image, at the tag matching this control plane's wire protocol.
    pub image: String,
    /// Whether the task stops when this container does. It is the only
    /// container, so its exit is the machine's.
    pub essential: bool,
    /// Seconds between `SIGTERM` and `SIGKILL` — see
    /// [`STOP_TIMEOUT_SECONDS`].
    pub stop_timeout: u32,
    /// The `flycod` configuration, base64, in the same variable the host path
    /// passes to Podman.
    pub environment: Vec<EnvVar>,
}

/// One environment variable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvVar {
    /// Its name.
    pub name: String,
    /// Its value.
    ///
    /// For [`CONFIG_ENV`] this is the session's whole `flycod`
    /// configuration — daemon token, harness credential and GitHub token
    /// inside it — and it is stored **in the task definition**, readable by
    /// anyone the account grants `ecs:DescribeTaskDefinition`. That is the
    /// same exposure a VM's `user-data` already has, and the alternative is a
    /// Secrets Manager secret plus an execution role per session: two more
    /// resources to create, grant and garbage-collect for no change in who
    /// can read it. Deleting the definitions with the machine is what bounds
    /// it.
    pub value: String,
}

/// Body of `RunTask`.
///
/// No `clientToken`. ECS's idempotency token pins the **first** task ARN it
/// saw — a second `RunTask` with the same token and the same parameters
/// answers with that task, and with different ones is a `ConflictException`
/// naming it — so the obvious token, the machine id, would make every later
/// `start` answer with the task it is supposed to be replacing. A redelivered
/// call is answered instead by adopting the family's live task, which is
/// right for every run rather than only the first.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunTask {
    /// The cluster to place it in.
    pub cluster: &'static str,
    /// `family:revision` or the revision's ARN.
    pub task_definition: String,
    /// One.
    pub count: u32,
    /// The single capacity provider this attempt asks for.
    pub capacity_provider_strategy: Vec<CapacityProviderItem>,
    /// The subnets and security group the task's interface joins.
    pub network_configuration: NetworkConfiguration,
    /// `LATEST`.
    pub platform_version: &'static str,
    /// Copy the definition's tags onto the task.
    pub propagate_tags: &'static str,
}

impl RunTask {
    /// The same task on the ordinary market.
    ///
    /// The identical body with one field replaced, which is the whole point:
    /// a fallback that rebuilt the request could differ from it in some other
    /// way and nobody would notice.
    #[must_use]
    pub fn on_demand(mut self) -> Self {
        self.capacity_provider_strategy = vec![CapacityProviderItem::of(ON_DEMAND_PROVIDER)];
        self
    }

    /// Whether this attempt asked for interruptible capacity.
    #[must_use]
    pub fn is_spot(&self) -> bool {
        self.capacity_provider_strategy
            .iter()
            .any(|item| item.capacity_provider == SPOT_PROVIDER)
    }
}

/// One entry of a capacity provider strategy.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapacityProviderItem {
    /// `FARGATE` or `FARGATE_SPOT`.
    pub capacity_provider: &'static str,
    /// Its weight in the strategy.
    pub weight: u32,
}

impl CapacityProviderItem {
    /// The strategy naming one provider.
    #[must_use]
    pub const fn of(capacity_provider: &'static str) -> Self {
        Self {
            capacity_provider,
            weight: PROVIDER_WEIGHT,
        }
    }
}

/// A task's networking.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkConfiguration {
    /// The `awsvpc` half, which is the only one.
    pub awsvpc_configuration: AwsVpcConfiguration,
}

/// Where a task's elastic network interface is attached.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AwsVpcConfiguration {
    /// Every subnet of the region's default VPC.
    ///
    /// All of them rather than the one the EC2 path launches into: ECS
    /// manages Fargate capacity per availability zone, and AWS's own advice
    /// for `Capacity is unavailable` is to name subnets in several
    /// ([repost.aws/knowledge-center/ecs-fargate-runtask-capacity](https://repost.aws/knowledge-center/ecs-fargate-runtask-capacity)).
    /// A task has no disk to keep in one zone, so nothing is lost by letting
    /// the platform choose.
    pub subnets: Vec<String>,
    /// The workspace security group.
    pub security_groups: Vec<String>,
    /// `ENABLED` — see [`ASSIGN_PUBLIC_IP`].
    pub assign_public_ip: &'static str,
}

/// Body of `DescribeTasks`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DescribeTasks {
    /// The cluster they are in.
    pub cluster: &'static str,
    /// Their ARNs.
    pub tasks: Vec<String>,
}

/// Body of `StopTask`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StopTask {
    /// The cluster the task is in.
    pub cluster: &'static str,
    /// The task's ARN.
    pub task: String,
    /// Why, which the console and the task's own `stoppedReason` show.
    pub reason: &'static str,
}

/// Body of `ListTasks`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListTasks {
    /// The cluster to look in.
    pub cluster: &'static str,
    /// The definition family, which is one machine.
    pub family: String,
    /// `RUNNING`: a task on its way out is not one to adopt.
    pub desired_status: &'static str,
    /// Continuation token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_token: Option<String>,
}

/// Body of `ListTaskDefinitions`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListTaskDefinitions {
    /// The family, in full: a machine id is unique, so a prefix filter on it
    /// matches one family.
    pub family_prefix: String,
    /// `ACTIVE`.
    pub status: &'static str,
    /// `DESC`, so the first result is the revision a run would use.
    pub sort: &'static str,
    /// Page size.
    pub max_results: u32,
    /// Continuation token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_token: Option<String>,
}

/// Body of `DescribeTaskDefinition` and `DeregisterTaskDefinition`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OneTaskDefinition {
    /// `family:revision` or the revision's ARN.
    pub task_definition: String,
    /// What else to answer with. `TAGS` on a describe, because a definition's
    /// tags are a sibling of the definition rather than part of it and are
    /// omitted unless they are asked for — and a resize has to carry them
    /// over: they name the session, which a resize is not told and must not
    /// drop. Absent on a deregister, which takes no such field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<[&'static str; 1]>,
}

impl OneTaskDefinition {
    /// One revision, tags and all.
    #[must_use]
    pub const fn described(task_definition: String) -> Self {
        Self {
            task_definition,
            include: Some([INCLUDE_TAGS]),
        }
    }

    /// One revision, named for a call that takes nothing else.
    #[must_use]
    pub const fn named(task_definition: String) -> Self {
        Self {
            task_definition,
            include: None,
        }
    }
}

/// Body of `DeleteTaskDefinitions`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteTaskDefinitions {
    /// Up to [`DELETE_BATCH`] revisions, all of which must already be
    /// deregistered.
    pub task_definitions: Vec<String>,
}

// ── Responses ──

/// A cluster, in the part flyco reads.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cluster {
    /// Its name.
    #[serde(default)]
    pub cluster_name: String,
    /// `ACTIVE`, or a state a task cannot be placed in.
    #[serde(default)]
    pub status: String,
}

impl Cluster {
    /// Whether tasks can be placed in it.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.status == ACTIVE
    }
}

/// Answer to `CreateCluster` and `DescribeClusters`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterList {
    /// The clusters that exist. `DescribeClusters` answers with an empty list
    /// and a `MISSING` failure for one that does not.
    #[serde(default)]
    pub clusters: Vec<Cluster>,
    /// The one `CreateCluster` made.
    #[serde(default)]
    pub cluster: Option<Cluster>,
}

impl ClusterList {
    /// Whether the flyco cluster is there and usable.
    #[must_use]
    pub fn has_active(&self) -> bool {
        self.clusters
            .iter()
            .chain(self.cluster.as_ref())
            .any(|cluster| cluster.cluster_name == CLUSTER && cluster.is_active())
    }
}

/// One failure of a call that answers HTTP 200 with failures in it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Failure {
    /// The resource it is about.
    #[serde(default)]
    pub arn: String,
    /// The reason, which for a capacity refusal is
    /// [`CAPACITY_UNAVAILABLE`] and a zone hint.
    #[serde(default)]
    pub reason: String,
    /// Further explanation, when ECS gives one.
    #[serde(default)]
    pub detail: String,
}

impl Failure {
    /// The reason and detail as one sentence for a user to read.
    #[must_use]
    pub fn message(&self) -> String {
        if self.detail.is_empty() {
            self.reason.clone()
        } else {
            format!("{}: {}", self.reason, self.detail)
        }
    }
}

/// One task, in the parts flyco reads.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    /// Its ARN, which is the machine's `native_id`.
    #[serde(default)]
    pub task_arn: String,
    /// Where it is in its lifecycle.
    #[serde(default)]
    pub last_status: String,
    /// Which capacity provider actually placed it — the market flyco is
    /// billed at, read rather than assumed.
    #[serde(default)]
    pub capacity_provider_name: String,
    /// Why it stopped, when it has, which is what a log line says about a
    /// machine that was evicted rather than asked to stop.
    #[serde(default)]
    pub stopped_reason: String,
}

impl Task {
    /// Whether it has finished.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.last_status == STOPPED
    }

    /// Which market it holds, when ECS named the provider that placed it.
    #[must_use]
    pub fn capacity_mode(&self) -> Option<CapacityMode> {
        match self.capacity_provider_name.as_str() {
            SPOT_PROVIDER => Some(CapacityMode::Spot),
            ON_DEMAND_PROVIDER => Some(CapacityMode::OnDemand),
            _ => None,
        }
    }
}

/// Answer to `RunTask`, `DescribeTasks` and `StopTask`.
///
/// One type for all three because they answer the same way, and because the
/// half that matters is the same half: a call that placed or found nothing
/// says so in `failures` with HTTP 200.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskList {
    /// The tasks it placed or found.
    #[serde(default)]
    pub tasks: Vec<Task>,
    /// The one `StopTask` stopped.
    #[serde(default)]
    pub task: Option<Task>,
    /// Everything that did not work.
    #[serde(default)]
    pub failures: Vec<Failure>,
}

impl TaskList {
    /// The one task this call was about, or the failure that explains why
    /// there is none.
    ///
    /// A capacity refusal becomes [`ProviderError::NoCapacity`], because that
    /// is the one failure a caller acts on rather than reports: on a Spot
    /// attempt it means "ask again on demand".
    ///
    /// # Errors
    ///
    /// [`ProviderError::NoCapacity`] for a capacity refusal,
    /// [`ProviderError::Rejected`] for any other failure, and
    /// [`ProviderError::Malformed`] when ECS answered with neither a task nor
    /// a reason.
    pub fn placed(self) -> Result<Task, ProviderError> {
        if let Some(task) = self.tasks.into_iter().next().or(self.task) {
            return Ok(task);
        }
        match self.failures.first() {
            Some(failure) if failure.reason.starts_with(CAPACITY_UNAVAILABLE) => {
                Err(ProviderError::NoCapacity(failure.message()))
            }
            Some(failure) => Err(ProviderError::Rejected(format!(
                "ECS placed no task: {}",
                failure.message()
            ))),
            None => Err(ProviderError::Malformed(
                "ECS answered a task call with neither a task nor a reason",
            )),
        }
    }
}

/// One page of `ListTasks`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskArnPage {
    /// The ARNs on this page.
    #[serde(default)]
    pub task_arns: Vec<String>,
    /// The next page, when the list is longer than one.
    #[serde(default)]
    pub next_token: Option<String>,
}

/// One page of `ListTaskDefinitions`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DefinitionArnPage {
    /// The revision ARNs on this page, newest first.
    #[serde(default)]
    pub task_definition_arns: Vec<String>,
    /// The next page.
    #[serde(default)]
    pub next_token: Option<String>,
}

/// Answer to `RegisterTaskDefinition`, `DescribeTaskDefinition` and
/// `DeregisterTaskDefinition`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DefinitionAnswer {
    /// The revision.
    #[serde(default)]
    pub task_definition: TaskDefinitionView,
    /// Its tags, which ECS answers with only when they were asked for — see
    /// [`OneTaskDefinition::described`].
    #[serde(default)]
    pub tags: Vec<Tag>,
}

/// A registered revision, in the parts a resize has to carry over.
///
/// Flyco writes these definitions and nothing else does, so the body this
/// module authors describes one completely — except for the container's
/// environment, which holds the session's `flycod` configuration and cannot
/// be re-derived without the bootstrap a resize no longer has. So a resize
/// reads that back and re-sends everything else.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskDefinitionView {
    /// `family:revision`, which is what a `RunTask` names.
    #[serde(default)]
    pub task_definition_arn: String,
    /// Which instruction set it runs, which a resize may not change.
    #[serde(default)]
    pub runtime_platform: Option<RuntimePlatformView>,
    /// Its ephemeral disk, when it asked for more than the free allowance.
    #[serde(default)]
    pub ephemeral_storage: Option<EphemeralStorage>,
    /// Its containers, of which flyco writes exactly one.
    #[serde(default)]
    pub container_definitions: Vec<ContainerView>,
}

/// The platform half of a [`TaskDefinitionView`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimePlatformView {
    /// `X86_64` or `ARM64`.
    #[serde(default)]
    pub cpu_architecture: String,
}

/// The container half of a [`TaskDefinitionView`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerView {
    /// The image it runs.
    #[serde(default)]
    pub image: String,
    /// Its environment, which is where the `flycod` configuration is.
    #[serde(default)]
    pub environment: Vec<EnvVar>,
}

impl TaskDefinitionView {
    /// The instruction set it runs, when ECS named one this driver knows.
    #[must_use]
    pub fn architecture(&self) -> Option<CpuArchitecture> {
        Shape::architecture_named(&self.runtime_platform.as_ref()?.cpu_architecture)
    }

    /// The one container's image.
    #[must_use]
    pub fn image(&self) -> Option<&str> {
        self.container_definitions
            .first()
            .map(|container| container.image.as_str())
            .filter(|image| !image.is_empty())
    }

    /// One environment value of the one container.
    #[must_use]
    pub fn env(&self, name: &str) -> Option<&str> {
        self.container_definitions
            .first()?
            .environment
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| entry.value.as_str())
    }
}

/// Answer to `DeleteTaskDefinitions`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskDefinitionDeletions {
    /// The ones it would not.
    #[serde(default)]
    pub failures: Vec<Failure>,
}

/// Whether a refusal is "no capacity in this market".
#[must_use]
pub const fn no_capacity(error: &ProviderError) -> bool {
    matches!(error, ProviderError::NoCapacity(_))
}

// ── The driver ──

impl<T: HttpTransport, C: MonotonicClock, K: Timer, W: WallClock> AwsProvider<T, C, K, W> {
    /// Sends one signed ECS call and decodes the answer.
    async fn ecs<B: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        region: &str,
        action: &str,
        body: &B,
    ) -> Result<R, ProviderError> {
        self.json_rpc(
            &ec2::endpoint(SERVICE, region),
            Scope {
                region,
                service: SERVICE,
            },
            &format!("{TARGET_PREFIX}{action}"),
            body,
        )
        .await
    }

    /// What Fargate offers in one region.
    ///
    /// Three price reads and no availability read: the sizes are flyco's own
    /// curation of a grid every region sells, so what varies between regions
    /// is the rate and whether Graviton is sold there at all — and both of
    /// those are answers the price list gives.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] if the Price List refuses a read.
    pub async fn fargate_catalog(
        &mut self,
        region: &str,
    ) -> Result<Vec<MachineCatalogEntry>, ProviderError> {
        let now = self.wall_clock.unix_seconds();
        let prices = self
            .prices
            .fargate_prices(&self.transport, &self.clock, &self.key, region, now)
            .await?;
        Ok(catalog(region, &prices))
    }

    /// Creates or finds the one cluster a region's containers share.
    ///
    /// Described before it is created, for the reason the security group is:
    /// a container session is provisioned far more often than once, and a
    /// `CreateCluster` on every provision would be a write on the hot path.
    /// Cached for the driver's lifetime after that, which is one operation.
    async fn ensure_cluster(&mut self, region: &str) -> Result<(), ProviderError> {
        if self.cluster.as_deref() == Some(region) {
            return Ok(());
        }

        let described: ClusterList = self
            .ecs(
                region,
                "DescribeClusters",
                &DescribeClusters {
                    clusters: [CLUSTER],
                },
            )
            .await?;

        if !described.has_active() {
            // Both capacity providers, because a cluster refuses one it was
            // not associated with and the spot fallback needs the other to be
            // there before it is needed.
            let created: ClusterList = self
                .ecs(
                    region,
                    "CreateCluster",
                    &CreateCluster {
                        cluster_name: CLUSTER,
                        capacity_providers: CAPACITY_PROVIDERS,
                    },
                )
                .await?;
            if !created.has_active() {
                return Err(ProviderError::Malformed(
                    "ECS accepted a cluster creation without answering with an active cluster",
                ));
            }
            tracing::info!(%region, cluster = CLUSTER, "created the flyco ECS cluster");
        }

        self.cluster = Some(region.to_owned());
        Ok(())
    }

    /// The shape a machine type names, or a refusal that names it back.
    fn offered_shape(machine_type: &str, region: &str) -> Result<Shape, ProviderError> {
        Shape::parse(machine_type).ok_or_else(|| ProviderError::Unavailable {
            machine_type: machine_type.to_owned(),
            region: region.to_owned(),
            reason: "Fargate offers no container of that size".to_owned(),
        })
    }

    /// The ephemeral disk a spec asks for, in the form the API takes it.
    ///
    /// `None` below the free allowance, because Fargate's floor for an
    /// explicit request is one gibibyte above it: a task that asked for
    /// exactly twenty would be refused for naming the default.
    fn ephemeral_storage(spec: &MachineSpec) -> Result<Option<EphemeralStorage>, ProviderError> {
        if spec.disk_gib > MAX_EPHEMERAL_GIB {
            return Err(ProviderError::Unavailable {
                machine_type: spec.machine_type.clone(),
                region: spec.region.clone(),
                reason: format!(
                    "a Fargate task's disk cannot exceed {MAX_EPHEMERAL_GIB} GiB and this \
                     session asked for {}",
                    spec.disk_gib
                ),
            });
        }
        Ok(
            (spec.disk_gib > FREE_EPHEMERAL_GIB).then_some(EphemeralStorage {
                size_in_gib: spec.disk_gib,
            }),
        )
    }

    /// The definition body for one provisioning request.
    fn definition_body(
        request: &ProvisionRequest,
        shape: Shape,
    ) -> Result<RegisterTaskDefinition, ProviderError> {
        Ok(Self::definition_body_with_config(
            names::machine(request.machine),
            shape,
            release::session_image_for_wire_protocol(),
            encoded_config(&request.bootstrap)?,
            Self::ephemeral_storage(&request.spec)?,
            tags(request.machine, &request.bootstrap.session.to_string()),
        ))
    }

    /// The definition body from its parts, which is what a resize rebuilds it
    /// from.
    fn definition_body_with_config(
        family: String,
        shape: Shape,
        image: String,
        config_base64: String,
        ephemeral_storage: Option<EphemeralStorage>,
        tags: Vec<Tag>,
    ) -> RegisterTaskDefinition {
        RegisterTaskDefinition {
            family,
            requires_compatibilities: [FARGATE_COMPATIBILITY],
            network_mode: NETWORK_MODE,
            cpu: shape.size.cpu_units(),
            memory: shape.size.memory_mib().to_string(),
            runtime_platform: RuntimePlatform {
                operating_system_family: OPERATING_SYSTEM_FAMILY,
                cpu_architecture: shape.cpu_architecture(),
            },
            ephemeral_storage,
            container_definitions: vec![ContainerDefinition {
                name: CONTAINER_NAME,
                image,
                essential: true,
                stop_timeout: STOP_TIMEOUT_SECONDS,
                environment: vec![EnvVar {
                    name: CONFIG_ENV.to_owned(),
                    value: config_base64,
                }],
            }],
            tags,
        }
    }

    /// Registers a revision and answers with the ARN a `RunTask` names.
    async fn register_definition(
        &self,
        region: &str,
        body: &RegisterTaskDefinition,
    ) -> Result<String, ProviderError> {
        let registered: DefinitionAnswer = self.ecs(region, "RegisterTaskDefinition", body).await?;
        let arn = registered.task_definition.task_definition_arn;
        if arn.is_empty() {
            return Err(ProviderError::Malformed(
                "ECS registered a task definition without naming the revision it created",
            ));
        }
        Ok(arn)
    }

    /// The newest runnable revision of one family, if the family exists.
    ///
    /// This is what stands in for the Cloud Run driver's `<job>/<execution>`
    /// pair: the definition half of a container machine is recovered from the
    /// machine id rather than carried beside the task's own ARN, so a row
    /// written before a resize still names the shape the machine has now.
    async fn latest_definition(
        &self,
        region: &str,
        family: &str,
    ) -> Result<Option<String>, ProviderError> {
        let page: DefinitionArnPage = self
            .ecs(
                region,
                "ListTaskDefinitions",
                &ListTaskDefinitions {
                    family_prefix: family.to_owned(),
                    status: ACTIVE,
                    sort: NEWEST_FIRST,
                    max_results: 1,
                    next_token: None,
                },
            )
            .await?;
        Ok(page.task_definition_arns.into_iter().next())
    }

    /// Every revision of one family, newest first.
    ///
    /// Paged, because a session that has been resized many times has a
    /// revision per resize and teardown has to reach all of them.
    async fn all_definitions(
        &self,
        region: &str,
        family: &str,
    ) -> Result<Vec<String>, ProviderError> {
        let mut collected = Vec::new();
        let mut next = None;
        loop {
            let page: DefinitionArnPage = self
                .ecs(
                    region,
                    "ListTaskDefinitions",
                    &ListTaskDefinitions {
                        family_prefix: family.to_owned(),
                        status: ACTIVE,
                        sort: NEWEST_FIRST,
                        max_results: 100,
                        next_token: next,
                    },
                )
                .await?;
            collected.extend(page.task_definition_arns);
            next = page.next_token.filter(|token| !token.is_empty());
            if next.is_none() {
                return Ok(collected);
            }
        }
    }

    /// The task of this family that is still meant to be running, if any.
    ///
    /// Only ever asked once the family is known to exist: `ListTasks`
    /// filtered by a family ECS has never registered is a refusal rather than
    /// an empty list.
    async fn live_task(&self, region: &str, family: &str) -> Result<Option<String>, ProviderError> {
        let mut next = None;
        loop {
            let page: TaskArnPage = self
                .ecs(
                    region,
                    "ListTasks",
                    &ListTasks {
                        cluster: CLUSTER,
                        family: family.to_owned(),
                        desired_status: DESIRED_RUNNING,
                        next_token: next,
                    },
                )
                .await?;
            if let Some(task) = page.task_arns.into_iter().next() {
                return Ok(Some(task));
            }
            next = page.next_token.filter(|token| !token.is_empty());
            if next.is_none() {
                return Ok(None);
            }
        }
    }

    /// One task, as ECS currently sees it, or `None` once ECS has forgotten
    /// it.
    ///
    /// A stopped task is described for about an hour and then purged, so
    /// absence is an ordinary answer for a machine that has been deallocated
    /// for a while — and it is the answer that says there is nothing left to
    /// stop.
    async fn describe_task(&self, region: &str, task: &str) -> Result<Option<Task>, ProviderError> {
        let described: TaskList = self
            .ecs(
                region,
                "DescribeTasks",
                &DescribeTasks {
                    cluster: CLUSTER,
                    tasks: vec![task.to_owned()],
                },
            )
            .await?;
        Ok(described.tasks.into_iter().next())
    }

    /// Runs the definition, falling back to on-demand when Spot has no
    /// capacity.
    ///
    /// The refusal is in the body rather than the status: `RunTask` answers
    /// HTTP 200 with an empty `tasks` and a `failures` entry, so a driver
    /// reading only status codes would report a machine that was never
    /// placed. Fargate does not fall back on its own — AWS says so — which is
    /// why the identical body is re-sent naming the other provider.
    async fn run_task(
        &self,
        region: &str,
        definition: String,
        network: &RegionNetwork,
        spot: bool,
    ) -> Result<Task, ProviderError> {
        let body = RunTask {
            cluster: CLUSTER,
            task_definition: definition,
            count: TASK_COUNT,
            capacity_provider_strategy: vec![CapacityProviderItem::of(if spot {
                SPOT_PROVIDER
            } else {
                ON_DEMAND_PROVIDER
            })],
            network_configuration: NetworkConfiguration {
                awsvpc_configuration: AwsVpcConfiguration {
                    subnets: network.subnet_ids.clone(),
                    security_groups: vec![network.security_group_id.clone()],
                    assign_public_ip: ASSIGN_PUBLIC_IP,
                },
            },
            platform_version: PLATFORM_VERSION,
            propagate_tags: PROPAGATE_FROM_DEFINITION,
        };

        let asked_for_spot = body.is_spot();
        let placed: TaskList = self.ecs(region, "RunTask", &body).await?;
        match placed.placed() {
            Ok(task) => Ok(task),
            Err(error) if asked_for_spot && no_capacity(&error) => {
                tracing::info!(
                    %error,
                    "Fargate Spot had no capacity; running the same task on demand"
                );
                let retried: TaskList = self.ecs(region, "RunTask", &body.on_demand()).await?;
                retried.placed()
            }
            Err(error) => Err(error),
        }
    }

    /// Stops the task and waits for it to actually be stopped, unless ECS has
    /// already stopped or forgotten it.
    ///
    /// Read before written, which is not a swallowed failure: a task that hit
    /// its own end or was evicted is already stopped, and `StopTask` against
    /// one ECS no longer knows is a refusal for a `deallocate` that has
    /// nothing left to do.
    ///
    /// Waited on, because the caller's next step is either another task of
    /// the same definition or the definition's deletion, and both against a
    /// container still writing its `workdir-patch` would lose the session's
    /// work.
    async fn stop_task(&self, region: &str, task: &str) -> Result<(), ProviderError> {
        match self.describe_task(region, task).await? {
            None => {
                tracing::debug!(
                    %task,
                    "ECS no longer describes this task, so there was nothing to stop"
                );
                return Ok(());
            }
            Some(described) if described.is_stopped() => {
                tracing::debug!(
                    %task,
                    reason = %described.stopped_reason,
                    "the task had already stopped, so there was nothing to stop"
                );
                return Ok(());
            }
            Some(_) => {}
        }

        let stopped: TaskList = self
            .ecs(
                region,
                "StopTask",
                &StopTask {
                    cluster: CLUSTER,
                    task: task.to_owned(),
                    reason: STOP_REASON,
                },
            )
            .await?;
        stopped.placed()?;
        self.await_stopped(region, task).await
    }

    /// Polls one task until it has stopped.
    ///
    /// A task that vanishes mid-poll counts as stopped: ECS only forgets a
    /// task it has finished with.
    async fn await_stopped(&self, region: &str, task: &str) -> Result<(), ProviderError> {
        for attempt in 0..MAX_POLL_ATTEMPTS {
            // ECS states no `Retry-After`, so the shared backoff paces this.
            self.timer.sleep(poll_delay(None, attempt)).await;
            match self.describe_task(region, task).await? {
                None => return Ok(()),
                Some(described) if described.is_stopped() => return Ok(()),
                Some(_) => {}
            }
        }

        Err(ProviderError::Rejected(format!(
            "an ECS task had not stopped after {MAX_POLL_ATTEMPTS} polls"
        )))
    }

    /// The machine one task adds up to.
    ///
    /// `Running` the moment ECS has accepted and placed the task, rather than
    /// once the image has been pulled: the pull is the session's boot, which
    /// the daemon announces over the relay when it comes up, and the two
    /// sibling container drivers report a machine the same way. There is also
    /// nothing left for this driver to do to it — a task has no address to
    /// attach and no volume to size — so a wait here would be a wait for
    /// nobody.
    fn container_machine(
        machine: MachineId,
        region: &str,
        task: &Task,
        requested: CapacityMode,
    ) -> Machine {
        Machine {
            id: machine,
            native_id: task.task_arn.clone(),
            runtime: Runtime::Container,
            region: region.to_owned(),
            state: MachineState::Running,
            // What ECS says placed it, because that is what the bill
            // follows. The request named exactly one capacity provider, so a
            // response that echoes none has still placed the task on the one
            // that was asked for.
            capacity_mode: task.capacity_mode().unwrap_or(requested),
            // A task has no inbound address and flyco never dials one: the
            // daemon opens the connection, from inside the container.
            address: None,
        }
    }

    /// Provisions a session as a standalone Fargate task.
    ///
    /// Register then run, and the register is skipped when the family is
    /// already there, because the queue that asks for this is at-least-once:
    /// a definition of this machine's name already existing means this is a
    /// redelivery, and a redelivery that ran it again would put a *second*
    /// `flycod` on the same session token. So a redelivery adopts the task
    /// that is already running, and runs one only when there is none.
    ///
    /// The definition body is built before any of those reads even though a
    /// redelivery will not send it: building it is what refuses a disk
    /// Fargate cannot give and a configuration that will not render, and both
    /// of those are better answered before a cluster exists than after.
    pub(super) async fn provision_task(
        &mut self,
        request: &ProvisionRequest,
    ) -> Result<Machine, ProviderError> {
        let region = &request.spec.region;
        let shape = Self::offered_shape(&request.spec.machine_type, region)?;

        // The first of the EC2 path's three gates applies unchanged: a region
        // the account has never enabled answers `AuthFailure` naming neither
        // the region nor the reason, whichever service is asked.
        let access = self.region_access().await?;
        if !access.allows(region) {
            return Err(ProviderError::Unavailable {
                machine_type: request.spec.machine_type.clone(),
                region: region.clone(),
                reason: access.refusal(region),
            });
        }

        // The other two do not: Fargate publishes no instance types to be
        // offered or withheld, and its quota is a count of tasks and vCPUs
        // that a refused `RunTask` names in the answer rather than a limit
        // this driver could subtract from beforehand.
        let requested = if request.spec.spot {
            CapacityMode::Spot
        } else {
            CapacityMode::OnDemand
        };
        let body = Self::definition_body(request, shape)?;
        let family = names::machine(request.machine);

        self.ensure_cluster(region).await?;
        let existing = self.latest_definition(region, &family).await?;
        if existing.is_some()
            && let Some(live) = self.live_task(region, &family).await?
        {
            tracing::info!(
                machine = %request.machine,
                task = %live,
                "a redelivered provision found the task already running; adopted it"
            );
            let adopted =
                self.describe_task(region, &live)
                    .await?
                    .ok_or(ProviderError::Malformed(
                        "ECS listed a running task it then did not describe",
                    ))?;
            return Ok(Self::container_machine(
                request.machine,
                region,
                &adopted,
                requested,
            ));
        }

        let definition = match existing {
            Some(revision) => revision,
            None => self.register_definition(region, &body).await?,
        };
        let network = self.ensure_network(region).await?;
        let task = self
            .run_task(region, definition, &network, request.spec.spot)
            .await?;

        let machine = Self::container_machine(request.machine, region, &task, requested);
        tracing::info!(
            machine = %request.machine,
            machine_type = %request.spec.machine_type,
            %region,
            capacity = ?machine.capacity_mode,
            "provisioned a Fargate task"
        );
        Ok(machine)
    }

    /// Stop, register a revision at the new size, run — which is what a
    /// resize is on a runtime whose size belongs to the task.
    ///
    /// Validated before anything is mutated, for the reason the EC2 path
    /// checks its three gates before it writes: a resize that refused after
    /// the stop would leave the session on a machine that is neither its old
    /// one nor its new one. What is validated is the one thing a name could
    /// silently change — the instruction set — because `fargate-4x8` and
    /// `fargate-arm64-4x8` are two entries at two prices, and swapping them
    /// under a running session would swap the architecture its working tree
    /// was built for.
    ///
    /// The stop is what writes the session's `workdir-patch` and the run is
    /// what replays it onto a fresh clone, so the ordering here is the same
    /// promise an ordinary container stop and start make.
    pub(super) async fn resize_task(
        &mut self,
        machine: &Machine,
        new_machine_type: &str,
    ) -> Result<Machine, ProviderError> {
        let region = machine.region.clone();
        let shape = Self::offered_shape(new_machine_type, &region)?;
        let family = names::machine(machine.id);

        let current =
            self.latest_definition(&region, &family)
                .await?
                .ok_or(ProviderError::Malformed(
                    "this machine has no ECS task definition, so there is nothing to resize",
                ))?;
        let described: DefinitionAnswer = self
            .ecs(
                &region,
                "DescribeTaskDefinition",
                &OneTaskDefinition::described(current),
            )
            .await?;
        let view = described.task_definition;

        if view.architecture() != Some(shape.architecture) {
            return Err(ProviderError::Unsupported {
                provider: PROVIDER,
                operation: "resize",
                reason: "a container cannot be resized across instruction sets, because its \
                         working tree was built for the one it is on",
            });
        }

        // Everything about this definition is flyco's own body except the
        // container's environment, which carries the session's `flycod`
        // configuration and cannot be re-derived without the bootstrap a
        // resize no longer holds — and the image, which is carried over
        // rather than re-derived for the same reason the Cloud Run driver
        // carries it: a resize changes the size and nothing else, and folding
        // an image upgrade into it would make one operation two.
        let config = view.env(CONFIG_ENV).ok_or(ProviderError::Malformed(
            "the ECS task definition carries no flycod configuration, so resizing it would \
             start a machine that cannot reach its session",
        ))?;
        let image = view.image().ok_or(ProviderError::Malformed(
            "the ECS task definition names no image, so there is nothing to resize",
        ))?;
        let body = Self::definition_body_with_config(
            family.clone(),
            shape,
            image.to_owned(),
            config.to_owned(),
            // The disk is carried over for the same reason: it is the
            // session's, and a resize was asked about the size of the
            // machine rather than the size of its filesystem.
            view.ephemeral_storage,
            // The tags name the session, which a resize is not told.
            described.tags.clone(),
        );

        self.stop_task(&region, &machine.native_id).await?;
        let definition = self.register_definition(&region, &body).await?;
        let network = self.ensure_network(&region).await?;
        let task = self
            .run_task(
                &region,
                definition,
                &network,
                machine.capacity_mode.is_spot(),
            )
            .await?;

        tracing::info!(machine = %machine.id, %new_machine_type, "resized a Fargate task");
        Ok(Self::container_machine(
            machine.id,
            &region,
            &task,
            machine.capacity_mode,
        ))
    }

    /// Stops the task, keeping its definition.
    ///
    /// No cluster check on this path, or on any other that is handed a
    /// [`Machine`]: the machine could not have been provisioned without the
    /// region's cluster, and creating one in order to stop a task in it would
    /// be a write to make a read succeed.
    pub(super) async fn deallocate_task(&self, machine: &Machine) -> Result<(), ProviderError> {
        let region = machine.region.clone();
        self.stop_task(&region, &machine.native_id).await
    }

    /// Runs the machine's newest definition again.
    ///
    /// A different task from the one that stopped, on a fresh filesystem: the
    /// machine's native id changes, and the caller is told so by the
    /// [`Machine`] it gets back. A redelivered start adopts a live task for
    /// the reason a redelivered provision does. The revision run is the
    /// newest, which is the one a resize left behind.
    pub(super) async fn start_task(&mut self, machine: &Machine) -> Result<Machine, ProviderError> {
        let region = machine.region.clone();
        let family = names::machine(machine.id);

        if let Some(live) = self.live_task(&region, &family).await? {
            tracing::info!(
                machine = %machine.id,
                task = %live,
                "a redelivered start found the task already running; adopted it"
            );
            let adopted =
                self.describe_task(&region, &live)
                    .await?
                    .ok_or(ProviderError::Malformed(
                        "ECS listed a running task it then did not describe",
                    ))?;
            return Ok(Self::container_machine(
                machine.id,
                &region,
                &adopted,
                machine.capacity_mode,
            ));
        }

        let definition =
            self.latest_definition(&region, &family)
                .await?
                .ok_or(ProviderError::Malformed(
                    "this machine has no ECS task definition, so there is nothing to start",
                ))?;
        let network = self.ensure_network(&region).await?;
        let task = self
            .run_task(
                &region,
                definition,
                &network,
                machine.capacity_mode.is_spot(),
            )
            .await?;

        Ok(Self::container_machine(
            machine.id,
            &region,
            &task,
            machine.capacity_mode,
        ))
    }

    /// Stops the task and removes every revision of its definition.
    ///
    /// In that order, and the stop comes first for the reason it does in a
    /// resize: a container still flushing its transcript is a session losing
    /// its last turn. Then deregister, then delete — deregistering alone
    /// leaves an `INACTIVE` revision in the account for ever, and a revision
    /// cannot be deleted until it has been deregistered.
    pub(super) async fn destroy_task(&self, machine: &Machine) -> Result<(), ProviderError> {
        let region = machine.region.clone();
        let family = names::machine(machine.id);
        self.stop_task(&region, &machine.native_id).await?;

        let revisions = self.all_definitions(&region, &family).await?;
        for revision in &revisions {
            let _: DefinitionAnswer = self
                .ecs(
                    &region,
                    "DeregisterTaskDefinition",
                    &OneTaskDefinition::named(revision.clone()),
                )
                .await?;
        }
        for batch in revisions.chunks(DELETE_BATCH) {
            let deleted: TaskDefinitionDeletions = self
                .ecs(
                    &region,
                    "DeleteTaskDefinitions",
                    &DeleteTaskDefinitions {
                        task_definitions: batch.to_vec(),
                    },
                )
                .await?;
            for failure in &deleted.failures {
                // Reported rather than raised: the machine is gone either
                // way, and a revision AWS declined to delete is an
                // `INACTIVE` row in the account rather than a resource
                // anybody is billed for.
                tracing::warn!(
                    revision = %failure.arn,
                    reason = %failure.message(),
                    "ECS would not delete a task definition revision"
                );
            }
        }

        tracing::info!(
            machine = %machine.id,
            revisions = revisions.len(),
            "destroyed a Fargate task and its definitions"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CPU_MEMORY_RANGES, CapacityMode, FREE_EPHEMERAL_GIB, Shape, Size, TaskList, catalog,
    };
    use crate::ProviderError;
    use crate::aws::pricing::{FargatePrices, FargateRates};
    use flyco_core::machine::{CpuArchitecture, MachinePricing, Runtime};
    use flyco_core::money::Usd;

    const REGION: &str = "us-west-2";

    /// The us-west-2 rates AWS's own price list publishes: $0.04048 per
    /// vCPU-hour and $0.004445 per GiB-hour on x86-64, $0.03238 and $0.00356
    /// on Graviton, $0.000111 per GiB-hour of ephemeral disk.
    fn prices() -> FargatePrices {
        FargatePrices {
            x86_64: Some(FargateRates {
                vcpu_hourly: Usd::from_micros(40_480),
                memory_gib_hourly: Usd::from_micros(4_445),
            }),
            arm64: Some(FargateRates {
                vcpu_hourly: Usd::from_micros(32_380),
                memory_gib_hourly: Usd::from_micros(3_560),
            }),
            ephemeral_gib_hourly: Some(Usd::from_micros(111)),
        }
    }

    #[test]
    fn every_offered_size_is_a_pair_fargate_would_accept() {
        for size in Size::offered() {
            assert!(
                size.is_offerable(),
                "{}x{} is outside Fargate's own table",
                size.vcpus,
                size.memory_gib
            );
        }
        // The table is what makes that assertion mean anything, so it has to
        // cover exactly the sizes the menu names.
        assert_eq!(CPU_MEMORY_RANGES.len(), Size::offered().len());
    }

    #[test]
    fn memory_doubles_per_core_at_the_top_of_the_menu() {
        assert_eq!(Size::of(1).memory_gib, 2);
        assert_eq!(Size::of(4).memory_gib, 8);
        assert_eq!(Size::of(8).memory_gib, 32);
        assert_eq!(Size::of(16).memory_gib, 64);
        assert_eq!(Size::of(4).cpu_units(), "4096");
        assert_eq!(Size::of(4).memory_mib(), 8_192);
    }

    #[test]
    fn a_shape_round_trips_through_the_name_it_is_published_under() {
        for shape in Shape::offered() {
            let name = shape.machine_type();
            assert_eq!(Shape::parse(&name), Some(shape), "`{name}` must parse back");
        }
        assert_eq!(
            Shape {
                size: Size::of(4),
                architecture: CpuArchitecture::X8664,
            }
            .machine_type(),
            "fargate-4x8"
        );
        assert_eq!(
            Shape {
                size: Size::of(8),
                architecture: CpuArchitecture::Arm64,
            }
            .machine_type(),
            "fargate-arm64-8x32"
        );
    }

    #[test]
    fn a_shape_flyco_never_published_is_not_a_shape() {
        // A legal Fargate pair that is not on this menu, an illegal one, an
        // EC2 instance type and a truncation are all the same answer.
        for name in [
            "fargate-4x16",
            "fargate-32x128",
            "fargate-arm64-4x16",
            "t3.small",
            "fargate-",
            "fargate-4x",
            "",
        ] {
            assert_eq!(Shape::parse(name), None, "`{name}` is not an offered shape");
        }
    }

    #[test]
    fn an_hour_is_the_published_rates_multiplied_out() {
        // Four cores and eight gibibytes on x86-64: 4 × $0.04048 +
        // 8 × $0.004445 = $0.19748.
        assert_eq!(
            Size::of(4).hourly(prices().x86_64.expect("x86-64 rates")),
            Usd::from_micros(197_480)
        );
        // The same size on Graviton is cheaper on both meters: 4 × $0.03238
        // + 8 × $0.00356.
        assert_eq!(
            Size::of(4).hourly(prices().arm64.expect("Graviton rates")),
            Usd::from_micros(158_000)
        );
    }

    #[test]
    fn the_catalog_publishes_every_size_on_both_architectures() {
        let entries = catalog(REGION, &prices());
        assert_eq!(entries.len(), Shape::offered().len());

        for entry in &entries {
            assert_eq!(entry.runtime, Runtime::Container);
            assert_eq!(entry.region, REGION);
            // Fargate gives nothing away by the month, unlike the two other
            // managed container services.
            assert_eq!(entry.free_grant, None);
            // Spot exists here, and the price quoted for it is the published
            // ceiling: AWS publishes no Fargate Spot rate at all.
            assert!(entry.pricing.offers_spot());
            let MachinePricing::Metered {
                on_demand_hourly,
                spot_hourly,
                storage,
                ..
            } = &entry.pricing
            else {
                panic!("a Fargate task is metered");
            };
            assert_eq!(spot_hourly.as_ref(), Some(on_demand_hourly));
            assert_eq!(storage.hourly(30), Some(Usd::from_micros(30 * 111)));
        }

        let arm: Vec<&str> = entries
            .iter()
            .filter(|entry| {
                entry.lineage.as_ref().expect("a lineage").architecture == CpuArchitecture::Arm64
            })
            .map(|entry| entry.machine_type.as_str())
            .collect();
        assert_eq!(
            arm,
            [
                "fargate-arm64-1x2",
                "fargate-arm64-2x4",
                "fargate-arm64-4x8",
                "fargate-arm64-8x32",
                "fargate-arm64-16x64"
            ]
        );
    }

    #[test]
    fn an_architecture_the_region_does_not_price_is_not_offered() {
        let mut prices = prices();
        prices.arm64 = None;
        let entries = catalog(REGION, &prices);
        assert_eq!(entries.len(), Size::offered().len());
        assert!(
            entries
                .iter()
                .all(|entry| !entry.machine_type.contains("arm64")),
            "a container flyco cannot price is a container it must not offer"
        );

        // And a region with no disk meter offers nothing: a session's disk is
        // sized from its spec, so an unpriced GiB is an unpriced machine.
        prices.ephemeral_gib_hourly = None;
        assert!(catalog(REGION, &prices).is_empty());
    }

    #[test]
    fn a_disk_inside_the_free_allowance_is_not_asked_for_at_all() {
        // Fargate's floor for an explicit request is one gibibyte above the
        // free allowance, so naming the default would be refused.
        assert_eq!(FREE_EPHEMERAL_GIB, 20);
    }

    #[test]
    fn a_capacity_refusal_arrives_as_a_two_hundred_with_no_task_in_it() {
        let answer: TaskList = serde_json::from_str(include_str!(
            "../../fixtures/aws/ecs_run_task_no_spot_capacity.json"
        ))
        .expect("the fixture parses");
        let error = answer.placed().expect_err("no task was placed");
        assert!(
            matches!(error, ProviderError::NoCapacity(_)),
            "a capacity refusal is what tells the driver to ask again on demand: {error}"
        );
    }

    #[test]
    fn any_other_failure_is_reported_rather_than_retried() {
        let answer: TaskList =
            serde_json::from_str(include_str!("../../fixtures/aws/ecs_run_task_failure.json"))
                .expect("the fixture parses");
        let error = answer.placed().expect_err("no task was placed");
        assert!(matches!(error, ProviderError::Rejected(_)));
        assert!(error.to_string().contains("RESOURCE:ENI"));
    }

    #[test]
    fn the_market_a_task_holds_is_read_from_the_provider_that_placed_it() {
        let answer: TaskList =
            serde_json::from_str(include_str!("../../fixtures/aws/ecs_run_task_spot.json"))
                .expect("the fixture parses");
        let task = answer.placed().expect("a task was placed");
        assert_eq!(task.capacity_mode(), Some(CapacityMode::Spot));
        assert!(!task.is_stopped());
        assert!(task.task_arn.starts_with("arn:aws:ecs:us-west-2:"));
    }
}
