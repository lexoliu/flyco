//! Machines: cloud providers, the priced catalog agents choose from, and
//! machine lifecycle state.

use serde::{Deserialize, Serialize};

use crate::id::{MachineId, ProviderAccountId, SessionId};
use crate::money::Usd;

/// A supported compute provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "sql", derive(skyzen::Column))]
pub enum CloudProviderKind {
    /// Microsoft Azure.
    Azure,
    /// Amazon Web Services.
    Aws,
    /// Google Cloud Platform.
    Gcp,
    /// GitHub Codespaces: a per-user dev container the account's own
    /// `codespace`-scoped token creates and drives.
    ///
    /// Counted as a cloud rather than a host: flyco opens the account's
    /// Codespaces API and bills against its free core-hour grant and its
    /// paid rate, exactly as it does for a subscription.
    Codespaces,
    /// A Linux machine the user owns, enrolled with the control plane and
    /// running sessions as Podman containers.
    ///
    /// Not a cloud at all: flyco opens no connection to it. The machine's
    /// own `flycod host` holds an outbound attachment and executes the
    /// container work the control plane sends down it — see [`crate::host`].
    Host,
}

/// What kind of thing a machine actually is: a virtual machine, or a
/// container the provider runs for the length of one execution.
///
/// One axis on the machine rather than a second set of providers, because
/// the same subscription sells both and the driver that reaches them is the
/// same driver. What it decides is the only thing that differs everywhere
/// else in flyco: **whether the filesystem survives a stop.** A VM
/// deallocates onto a disk that is still there when it starts again; a
/// managed container's filesystem ends with its execution, so the working
/// tree has to travel as the `workdir-patch` flyco already keeps and be
/// replayed onto a fresh clone at the next start.
///
/// [`Vm`](Self::Vm) is the default, and that is a statement about history
/// rather than a preference: every machine flyco provisioned before this
/// axis existed was one, and a row or a request that names no runtime is
/// one of those.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "sql", derive(skyzen::Column))]
pub enum Runtime {
    /// A virtual machine with a persistent disk that outlives a stop.
    #[default]
    Vm,
    /// A container the provider runs for one execution, with no disk.
    Container,
}

impl Runtime {
    /// Whether the working tree is still there after the machine stops.
    ///
    /// The question every caller is actually asking, asked once: a spot
    /// reclamation on a VM syncs the page cache and trusts the disk, while
    /// the same event on a container has to write the patch out before the
    /// filesystem goes with the execution.
    #[must_use]
    pub const fn keeps_disk(self) -> bool {
        matches!(self, Self::Vm)
    }
}

/// What a provider gives away every month before it bills a container at
/// all.
///
/// Stated in the provider's own two meters rather than as a number of hours
/// or a sum of money, because that is how the grant is actually spent: Azure
/// Container Apps gives 180,000 vCPU-seconds and 360,000 GiB-seconds per
/// subscription per month, and a 4 vCPU / 8 GiB job exhausts the vCPU half
/// first. Converting either meter into hours here would need a machine size
/// this type does not have, and converting it into dollars would be a
/// discount rather than an allowance.
///
/// Per subscription and per calendar month, on every provider that offers
/// one, which is why nothing here names a machine: two entries of one
/// account share one grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct FreeGrant {
    /// vCPU-seconds the provider does not bill for, per month.
    pub vcpu_seconds_per_month: u64,
    /// GiB-seconds of memory the provider does not bill for, per month.
    pub gib_seconds_per_month: u64,
}

/// Operating system family of a machine type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OsFamily {
    /// Linux (the default, and the only family flyco ever picks itself).
    Linux,
    /// macOS (dedicated-host constraints apply).
    MacOs,
    /// Windows.
    Windows,
}

/// The instruction set a machine type runs.
///
/// An independent dimension of the catalog rather than a property to rank:
/// an arm64 type is not a cheaper or dearer version of an x86-64 one, it is
/// a different machine, and a build that needs one is not served by the
/// other. Curation therefore never compares across it (see
/// [`crate::catalog`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
pub enum CpuArchitecture {
    /// 64-bit x86. Spelled as every provider spells it, which is not what
    /// `rename_all` would produce from a camel-case variant.
    #[serde(rename = "x86_64")]
    X8664,
    /// 64-bit Arm.
    #[serde(rename = "arm64")]
    Arm64,
}

/// Where a machine type sits in its provider's own line-up.
///
/// The three facts curation needs, and none of which a frontend should ever
/// recover by parsing a type name: which instruction set it runs, which
/// family of the provider's line-up it belongs to, and which generation of
/// that family it is. Every driver derives them from the metadata its own
/// API publishes — Azure's quota family, EC2's `DescribeInstanceTypes`,
/// Compute Engine's `architecture` field and series name — so a provider
/// that renames its types breaks one driver rather than the whole product.
///
/// Absent from an entry describing hardware the user owns: flyco has not
/// inspected that machine, and inventing a family for it would be a claim
/// the catalog cannot support.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MachineLineage {
    /// The instruction set it runs.
    pub architecture: CpuArchitecture,
    /// Provider-native family key, with the generation removed.
    ///
    /// Two types share a family when the provider sells them as the same
    /// machine in different generations: `Standard_D4s_v5` and
    /// `Standard_D4s_v6`, `m6g.xlarge` and `m7g.xlarge`. The string is a
    /// key rather than a label — its shape is the driver's business, and
    /// nothing outside the driver that produced it may parse it.
    pub family: String,
    /// Which generation of that family it is, when the provider numbers
    /// them.
    ///
    /// Absent where a family carries no version in the provider's own
    /// metadata, which makes it a family of one rather than an old
    /// generation to hide.
    pub generation: Option<u32>,
}

/// How much compute a catalog entry offers.
///
/// Absent from an entry the provider does not publish a size for — see
/// [`MachineCatalogEntry::capacity`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MachineCapacity {
    /// Virtual CPU count.
    pub vcpus: u32,
    /// Memory in MiB.
    pub memory_mib: u64,
}

/// How a provider prices the persistent disk attached to a machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StoragePricing {
    /// A linear price for every provisioned GiB.
    PerGibHourly {
        /// Price of one GiB for one hour.
        rate: Usd,
    },
    /// Fixed-price capacity tiers. The first tier whose capacity contains
    /// the requested disk is the one the provider bills.
    CapacityTiers {
        /// Tiers in ascending capacity order.
        tiers: Vec<StoragePriceTier>,
    },
}

/// One fixed-price disk tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct StoragePriceTier {
    /// Maximum provisioned size covered by the tier.
    pub capacity_gib: u32,
    /// Price of the tier for one hour.
    pub hourly: Usd,
}

impl StoragePricing {
    /// Prices a requested persistent disk.
    #[must_use]
    pub fn hourly(&self, disk_gib: u32) -> Option<Usd> {
        match self {
            Self::PerGibHourly { rate } => Some(Usd::from_micros(
                rate.micros().saturating_mul(u64::from(disk_gib)),
            )),
            Self::CapacityTiers { tiers } => tiers
                .iter()
                .find(|tier| disk_gib <= tier.capacity_gib)
                .map(|tier| tier.hourly),
        }
    }
}

/// A provider's floor on what starting a machine costs at all.
///
/// Both halves travel together because only one of them answers the question
/// a user is actually asking. `24` hours is a fact about a licence;
/// `$14.98` is what pressing the button costs, and it is the number that has
/// to be on screen before send is enabled. Deriving the second from the
/// first at every call site would be one multiplication written four times,
/// and a catalog whose price and minimum could disagree would be worse than
/// one that quoted neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct BillingMinimum {
    /// Hours the provider bills however briefly the machine runs.
    pub hours: u32,
    /// What those hours cost at the on-demand rate.
    pub charge: Usd,
}

impl BillingMinimum {
    /// The floor `hours` of `on_demand_hourly` adds up to.
    #[must_use]
    pub fn new(hours: u32, on_demand_hourly: Usd) -> Self {
        Self {
            hours,
            charge: Usd::from_micros(on_demand_hourly.micros().saturating_mul(u64::from(hours))),
        }
    }
}

/// What an hour on a machine costs.
///
/// A sum rather than an amount with a nullable field, because "the provider
/// bills nothing for this" and "the price is zero" are different claims and
/// only one of them is ever true. A machine the user already owns has no
/// rate flyco could quote, and quoting `$0.00` would tell a budget it can
/// run forever.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MachinePricing {
    /// The provider meters this machine and bills for it by the hour.
    Metered {
        /// On-demand price per hour.
        on_demand_hourly: Usd,
        /// Spot price per hour, when the type is available as spot.
        spot_hourly: Option<Usd>,
        /// The floor the provider bills the moment the machine boots, when
        /// it imposes one (e.g. EC2 Mac dedicated hosts bill 24 hours under
        /// the Apple licence). The agent and the user both see this before
        /// choosing.
        minimum: Option<BillingMinimum>,
        /// Persistent-disk pricing published by the provider.
        storage: StoragePricing,
    },
    /// Hardware the user already owns and already pays for. Flyco meters
    /// nothing on it and a session running here spends no budget.
    UserOwned,
}

impl MachinePricing {
    /// Whether the provider quoted this type as spot at all.
    ///
    /// A type without a spot price is one the account cannot hold as spot —
    /// on Azure, one whose cores exceed the region's `lowPriorityCores`
    /// quota — so a session that asked for spot has to hold it on demand,
    /// and say so, rather than ask the provider for capacity it will refuse.
    #[must_use]
    pub const fn offers_spot(&self) -> bool {
        matches!(
            self,
            Self::Metered {
                spot_hourly: Some(_),
                ..
            }
        )
    }

    /// What one hour costs at the given capacity mode, when flyco bills for
    /// it at all.
    #[must_use]
    pub const fn hourly(&self, spot: bool) -> Option<Usd> {
        match self {
            Self::UserOwned => None,
            Self::Metered {
                on_demand_hourly,
                spot_hourly,
                ..
            } => match (spot, spot_hourly) {
                (true, Some(price)) => Some(*price),
                _ => Some(*on_demand_hourly),
            },
        }
    }

    /// What one hour of the requested persistent disk costs.
    #[must_use]
    pub fn storage_hourly(&self, disk_gib: u32) -> Option<Usd> {
        match self {
            Self::UserOwned => None,
            Self::Metered { storage, .. } => storage.hourly(disk_gib),
        }
    }
}

/// One entry in the machine catalog the agent sees when deciding whether
/// to keep, upgrade, or downgrade its machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MachineCatalogEntry {
    /// Which provider offers it.
    pub provider: CloudProviderKind,
    /// Which linked account offers it.
    ///
    /// A driver cannot know this — it holds credentials, not the row they
    /// were unsealed from — so it is stamped by the control plane as it
    /// merges each account's catalog. Without it a caller could not turn a
    /// choice from the merged list back into a request, which names the
    /// account it provisions through.
    pub account: Option<ProviderAccountId>,
    /// Provider-native region this entry is offered in.
    ///
    /// A catalog spans every region an account may deploy into, so an entry
    /// without one would not say where the machine it describes can be
    /// created. A registered SSH host names itself here: it is its own
    /// region, and there is nowhere else to put it.
    pub region: String,
    /// Provider-native machine type name (e.g. `Standard_B2ats_v2`).
    pub machine_type: String,
    /// Whether this entry is a virtual machine or a managed container.
    ///
    /// Defaulted rather than required, because a catalog is cached as JSON
    /// and a document written before this axis existed describes machines
    /// that were all [`Runtime::Vm`].
    #[serde(default)]
    pub runtime: Runtime,
    /// The monthly allowance the provider does not bill this entry for,
    /// where it publishes one.
    ///
    /// Absent is the ordinary case and the honest one: a VM never carries a
    /// grant, and neither does a container service that does not offer one
    /// (ACI and Fargate). Present, it belongs to the *subscription* rather
    /// than to this machine — every entry of that account draws on the same
    /// pool — which is why it says what the pool is rather than what is
    /// left of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_grant: Option<FreeGrant>,
    /// Operating system family.
    pub os: OsFamily,
    /// How big it is, when the provider publishes a size.
    ///
    /// A cloud SKU always does. A host the user registered over SSH has
    /// whatever hardware it has, and flyco does not learn that until a
    /// daemon runs on it and says so — inventing a size here would be a
    /// number the agent could plan against and be wrong about.
    pub capacity: Option<MachineCapacity>,
    /// Where it sits in the provider's line-up, when the provider says.
    ///
    /// Absent for the same reason [`Self::capacity`] is: a host the user
    /// registered is whatever hardware it is, and flyco has not looked.
    pub lineage: Option<MachineLineage>,
    /// What it costs to run for an hour.
    pub pricing: MachinePricing,
}

/// Smallest vCPU count flyco will pick for a session on its own.
///
/// A coding agent compiles, runs test suites and holds a language server
/// open. Below this the machine is cheap and the session is slow, which is
/// the wrong trade for compute billed by the hour — the fast machine
/// finishes first and costs less in total.
pub const AUTO_MIN_VCPUS: u32 = 4;

/// Smallest memory, in MiB, flyco will pick for a session on its own.
///
/// The same argument as [`AUTO_MIN_VCPUS`], and the sharper of the two
/// constraints: a linker or a test run that exhausts memory does not run
/// slowly, it fails.
pub const AUTO_MIN_MEMORY_MIB: u64 = 16 * 1024;

impl MachineCatalogEntry {
    /// The floor the provider bills the moment a machine of this type boots,
    /// when it imposes one.
    ///
    /// Reads through the pricing sum so no caller has to match on it to ask
    /// the one question that decides whether a resize needs the user: an EC2
    /// Mac bills a full day under the Apple licence, and hardware the user
    /// owns bills nothing at all.
    #[must_use]
    pub const fn billing_minimum(&self) -> Option<BillingMinimum> {
        match self.pricing {
            MachinePricing::UserOwned => None,
            MachinePricing::Metered { minimum, .. } => minimum,
        }
    }

    /// Whether starting this type commits the user to a minimum charge.
    ///
    /// The gate on the agent's own resize: moving to a license-bound type
    /// spends the user's money before anything runs, so it is raised as an
    /// approval rather than performed (docs/ux.md §9.5).
    #[must_use]
    pub const fn is_license_bound(&self) -> bool {
        self.billing_minimum().is_some()
    }

    /// Whether the provider covers this machine out of a monthly allowance
    /// rather than billing it.
    ///
    /// A fact about the entry, not about the account's remaining balance:
    /// how much of the grant this subscription has already spent is a
    /// number only the provider's own meter has, and nothing here pretends
    /// to it.
    #[must_use]
    pub const fn has_free_grant(&self) -> bool {
        self.free_grant.is_some()
    }

    /// Whether flyco may pick this entry without being asked to.
    ///
    /// Linux, provisionable through a linked account, and at least
    /// [`AUTO_MIN_VCPUS`] × [`AUTO_MIN_MEMORY_MIB`]. An entry that publishes
    /// no capacity qualifies: that is hardware the user already owns, whose
    /// size flyco does not learn until a daemon runs on it, and refusing it
    /// for a size nobody stated would rule out the one machine the user
    /// explicitly registered.
    ///
    /// A container the provider covers out of a monthly grant clears the
    /// memory floor on the cores alone. Azure's Consumption jobs top out at
    /// 4 vCPU and 8 GiB, so holding them to 16 GiB would mean `Auto` never
    /// picks the one size that costs nothing — which is the reason the
    /// container path exists (issue #235). The core floor still holds: a
    /// two-core container is too small for a coding agent whoever pays.
    #[must_use]
    pub fn is_auto_eligible(&self) -> bool {
        self.os == OsFamily::Linux
            && self.account.is_some()
            && self.capacity.as_ref().is_none_or(|capacity| {
                capacity.vcpus >= AUTO_MIN_VCPUS
                    && (capacity.memory_mib >= AUTO_MIN_MEMORY_MIB || self.has_free_grant())
            })
    }
}

/// The machine flyco picks when the caller names none.
///
/// Two rules, in this order:
///
/// 1. **A container the provider does not bill for wins.** Where the
///    account's catalog offers an entry with a
///    [`free_grant`](MachineCatalogEntry::free_grant), that grant is
///    compute the user is already entitled to and which expires unspent at
///    the end of the month — so spending it is strictly better than
///    spending money, and better than spending nothing on hardware the user
///    owns, which is still there in November.
/// 2. **Otherwise, the cheapest
///    [auto-eligible](MachineCatalogEntry::is_auto_eligible) entry**:
///    user-owned hardware wins (flyco meters nothing on it), and metered
///    entries are ordered by the hourly rate the session actually asked for
///    — spot when the caller wants it and the type has a spot price,
///    otherwise on-demand.
///
/// The second rule also orders *within* the first, because two free-granted
/// containers draw on the same subscription pool and the cheaper one leaves
/// more of it.
///
/// There is deliberately no fallback to something smaller. A machine below
/// the floor is not a cheaper version of the same session, it is a session
/// that thrashes; a catalog offering nothing big enough is a fact the user
/// has to act on, not one to paper over.
#[must_use]
pub fn auto_linux_choice(
    entries: &[MachineCatalogEntry],
    spot: bool,
) -> Option<&MachineCatalogEntry> {
    entries
        .iter()
        .filter(|entry| entry.is_auto_eligible())
        .min_by_key(|entry| {
            (
                u8::from(!entry.has_free_grant()),
                entry
                    .pricing
                    .hourly(spot)
                    .map_or((0_u8, Usd::ZERO), |hourly| (1_u8, hourly)),
            )
        })
}

/// The machine a session is on, as the agent driving it is told about it.
///
/// Not the whole [`MachineCatalogEntry`]: the agent is deciding whether to
/// keep working here, and five facts answer that — what the machine is
/// called, what an hour of it costs, whether the capacity is interruptible,
/// how big it is, and whether starting it already committed the user to a
/// licence minimum. The account, the region, the family and the storage
/// tiers answer a different question (where flyco would provision *another*
/// machine) and stay in the catalog.
///
/// The same value travels three ways and means one thing in all of them: in
/// the bootstrap a machine boots with, in the `[machine]` table of the
/// daemon's configuration, and in what the agent's `machine_status` tool
/// reads back. Absent facts are absent from the serialized document rather
/// than spelled `null`, because one of those three is TOML and TOML has no
/// null to spell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SessionMachine {
    /// Provider-native machine type name.
    pub machine_type: String,
    /// What one hour of it costs, when flyco meters it at all.
    ///
    /// The rate for the capacity actually held — spot when
    /// [`spot`](Self::spot) is true and the type has a spot price. Absent on
    /// hardware the user owns, where quoting `$0.00` would tell a budget the
    /// session can run forever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hourly: Option<Usd>,
    /// Whether the machine holds interruptible capacity.
    pub spot: bool,
    /// How big it is, when the provider publishes a size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<MachineCapacity>,
    /// The floor the provider billed the moment it booted, when the type
    /// carries one.
    ///
    /// Present on exactly the license-bound types. An agent reading it knows
    /// the machine has already cost [`BillingMinimum::charge`] whatever it
    /// does next — and that moving *to* such a type is the user's decision
    /// rather than its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum: Option<BillingMinimum>,
}

impl SessionMachine {
    /// Reads the five facts off a curated catalog entry.
    ///
    /// `spot` is what the machine actually holds rather than what was asked
    /// for, because that is what the price follows.
    #[must_use]
    pub fn of(entry: &MachineCatalogEntry, spot: bool) -> Self {
        Self {
            machine_type: entry.machine_type.clone(),
            hourly: entry.pricing.hourly(spot),
            spot,
            capacity: entry.capacity.clone(),
            minimum: entry.billing_minimum(),
        }
    }

    /// Whether being on this machine already committed the user to a
    /// minimum charge.
    #[must_use]
    pub const fn is_license_bound(&self) -> bool {
        self.minimum.is_some()
    }
}

/// What `GET /v1/sessions/{id}/agent/machine` tells a session's daemon.
///
/// The machine and *who chose it*, because the second changes what the agent
/// may do with the first: a machine the user picked is a decision flyco must
/// not quietly undo (docs/ux.md §9.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AgentMachineView {
    /// Whether flyco or the user chose the machine this session runs on.
    pub origin: crate::session::MachineOrigin,
    /// The machine, as the agent is told about it.
    pub machine: SessionMachine,
    /// Where it is in its lifecycle.
    pub state: MachineState,
    /// Provider-native region it runs in.
    pub region: String,
}

/// Everything needed to provision a machine for a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MachineSpec {
    /// Which provider to provision on.
    pub provider: CloudProviderKind,
    /// Provider-native machine type name.
    pub machine_type: String,
    /// Whether the type names a virtual machine or a managed container.
    ///
    /// Carried on the spec rather than looked up from the catalog every
    /// time, because it decides what a *stop* means to this machine and a
    /// stop happens long after the catalog entry it came from was read.
    /// Defaulted for the rows written before this axis existed, every one
    /// of which is a [`Runtime::Vm`].
    #[serde(default)]
    pub runtime: Runtime,
    /// Provider-native region name.
    pub region: String,
    /// Whether to request spot capacity (the default).
    pub spot: bool,
    /// Disk size in GiB.
    ///
    /// Ignored on [`Runtime::Container`], which has no persistent disk to
    /// size — the request still carries the session's default so a session
    /// moved between runtimes asks for the same disk it always did.
    pub disk_gib: u32,
}

/// Lifecycle state of a provisioned machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "sql", derive(skyzen::Column))]
pub enum MachineState {
    /// Being created.
    Provisioning,
    /// Running and connected.
    Running,
    /// Compute reclaimed (spot eviction or resize); disk retained.
    Deallocated,
    /// Compute and disk released.
    Destroyed,
}

/// The machine a session is running on, as `GET
/// /v1/sessions/{id}/machine` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MachineView {
    /// Identifier of the provisioned machine.
    pub id: MachineId,
    /// Session it belongs to. A machine serves exactly one.
    pub session: SessionId,
    /// What was asked for.
    pub spec: MachineSpec,
    /// Where it is in its lifecycle.
    pub state: MachineState,
    /// Whether the machine actually holds interruptible capacity.
    ///
    /// [`spec.spot`](MachineSpec::spot) is what was asked for; this is what
    /// the provider gave. Azure refuses spot on subscriptions and SKUs that
    /// do not support it, and flyco falls back to on-demand rather than
    /// failing the session, so the two can disagree — and the price being
    /// billed follows this field, not the request.
    pub spot: bool,
    /// Price actually being billed per hour — the spot price when the
    /// machine holds spot capacity, the on-demand one otherwise, and
    /// nothing at all on hardware the user owns.
    pub hourly: Option<Usd>,
    /// Persistent-disk price actually being billed per hour.
    pub storage_hourly: Option<Usd>,
    /// Provider-native region it landed in.
    pub region: String,
    /// When it was created, seconds since the Unix epoch.
    pub created_at_unix: u64,
}

/// Answer of `GET /v1/machines/catalog`.
///
/// Not a bare list, because "no machines" and "no machines *yet*" are
/// different answers and a list can only say the first. A cloud account's
/// catalog is read in the background — it is thousands of SKUs, quotas and
/// price pages per region, which is minutes of work and far more than a
/// request may spend — so an account linked moments ago has nothing to
/// contribute and is named in [`pending_accounts`](Self::pending_accounts)
/// instead. A reader that ignored the difference would tell a user who has
/// just linked a subscription that it can deploy nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MachineCatalog {
    /// The curated entries every account that has been read can offer.
    pub entries: Vec<MachineCatalogEntry>,
    /// The linked accounts whose catalog has not been read yet.
    ///
    /// Empty is the steady state. While it is not, the answer is incomplete
    /// rather than final, and a reader should say so and ask again.
    pub pending_accounts: Vec<ProviderAccountId>,
}

/// Answer of `GET /v1/machines/default`.
///
/// The machine flyco would provision right now, and the catalog entry it
/// came from, so a caller can show what it costs and how big it is without
/// searching the whole catalog for the type flyco named. The two travel
/// together because they are one answer: a choice whose entry the caller had
/// to look up again could be looked up against a catalog that has since
/// changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MachineDefault {
    /// What `POST /v1/sessions` would provision if it named no machine.
    pub choice: crate::session::MachineChoice,
    /// The catalog entry that choice points at, with its price and size.
    pub entry: MachineCatalogEntry,
    /// The linked accounts still being read when this choice was made.
    ///
    /// The choice is the best one flyco can make *from what it has read*.
    /// While this is non-empty a cheaper machine may still arrive, so a
    /// caller showing the answer as final would be overstating it.
    pub pending_accounts: Vec<ProviderAccountId>,
}

/// Request body of `POST /v1/sessions/{id}/machine/resize`.
///
/// The disk survives a resize; only compute is replaced. The provider and
/// region are not part of this, because moving a machine between them would
/// mean a new disk, which is a new session's problem rather than a resize.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ResizeMachine {
    /// Provider-native machine type to move to, from the catalog.
    pub machine_type: String,
}

#[cfg(test)]
mod tests {
    use super::{
        AUTO_MIN_MEMORY_MIB, AUTO_MIN_VCPUS, BillingMinimum, CloudProviderKind, CpuArchitecture,
        FreeGrant, MachineCapacity, MachineCatalogEntry, MachineLineage, MachinePricing,
        MachineSpec, OsFamily, Runtime, StoragePricing, auto_linux_choice,
    };
    use crate::id::ProviderAccountId;
    use crate::money::Usd;
    use uuid::Uuid;

    #[test]
    fn a_metered_entry_prices_spot_when_there_is_a_spot_price() {
        let pricing = MachinePricing::Metered {
            on_demand_hourly: Usd::from_micros(8_400),
            spot_hourly: Some(Usd::from_micros(7_560)),
            minimum: None,
            storage: StoragePricing::PerGibHourly {
                rate: Usd::from_micros(100),
            },
        };

        assert_eq!(pricing.hourly(true), Some(Usd::from_micros(7_560)));
        assert_eq!(pricing.hourly(false), Some(Usd::from_micros(8_400)));
    }

    #[test]
    fn a_type_with_no_spot_meter_is_priced_on_demand_either_way() {
        let pricing = MachinePricing::Metered {
            on_demand_hourly: Usd::from_micros(8_400),
            spot_hourly: None,
            minimum: Some(BillingMinimum::new(24, Usd::from_micros(8_400))),
            storage: StoragePricing::PerGibHourly {
                rate: Usd::from_micros(100),
            },
        };

        assert_eq!(pricing.hourly(true), Some(Usd::from_micros(8_400)));
    }

    #[test]
    fn hardware_the_user_owns_has_no_price_at_all() {
        // Not `Some(Usd::ZERO)`: a budget told an hour costs nothing would
        // conclude the session can run forever, which is a different claim
        // from "flyco does not meter this".
        assert_eq!(MachinePricing::UserOwned.hourly(false), None);
    }

    #[test]
    fn a_catalog_entry_round_trips_with_its_pricing_tag() {
        let entry = MachineCatalogEntry {
            account: None,
            region: "northcentralus".to_owned(),
            provider: CloudProviderKind::Azure,
            machine_type: "Standard_B2pts_v2".to_owned(),
            runtime: Runtime::Vm,
            free_grant: None,
            os: OsFamily::Linux,
            capacity: Some(MachineCapacity {
                vcpus: 2,
                memory_mib: 1_024,
            }),
            lineage: Some(MachineLineage {
                architecture: CpuArchitecture::Arm64,
                family: "bps".to_owned(),
                generation: Some(2),
            }),
            pricing: MachinePricing::Metered {
                on_demand_hourly: Usd::from_micros(8_400),
                spot_hourly: None,
                minimum: None,
                storage: StoragePricing::PerGibHourly {
                    rate: Usd::from_micros(100),
                },
            },
        };

        let json = serde_json::to_value(&entry).expect("serialize");
        assert_eq!(json["pricing"]["kind"], "metered");
        assert_eq!(
            serde_json::from_value::<MachineCatalogEntry>(json).expect("deserialize"),
            entry
        );
    }

    /// A Linux entry big enough for flyco to pick on its own.
    fn linux(machine_type: &str, hourly: Option<Usd>) -> MachineCatalogEntry {
        MachineCatalogEntry {
            account: Some(ProviderAccountId::from_uuid(Uuid::from_u128(1))),
            region: "us-east-1".to_owned(),
            provider: CloudProviderKind::Aws,
            machine_type: machine_type.to_owned(),
            runtime: Runtime::Vm,
            free_grant: None,
            os: OsFamily::Linux,
            capacity: Some(MachineCapacity {
                vcpus: AUTO_MIN_VCPUS,
                memory_mib: AUTO_MIN_MEMORY_MIB,
            }),
            lineage: Some(MachineLineage {
                architecture: CpuArchitecture::X8664,
                family: "m".to_owned(),
                generation: Some(7),
            }),
            pricing: hourly.map_or(MachinePricing::UserOwned, |on_demand_hourly| {
                MachinePricing::Metered {
                    on_demand_hourly,
                    spot_hourly: Some(Usd::from_micros(on_demand_hourly.micros() / 2)),
                    minimum: None,
                    storage: StoragePricing::PerGibHourly {
                        rate: Usd::from_micros(1),
                    },
                }
            }),
        }
    }

    #[test]
    fn automatic_choice_is_the_cheapest_eligible_linux_with_an_account() {
        let mac = MachineCatalogEntry {
            os: OsFamily::MacOs,
            ..linux("mac.metal", Some(Usd::from_cents(1)))
        };
        let expensive = linux("big", Some(Usd::from_cents(50)));
        let cheap = linux("right-sized", Some(Usd::from_cents(2)));
        let owned = MachineCatalogEntry {
            // Hardware the user owns publishes no size, and qualifies
            // anyway: flyco learns what it is when a daemon runs on it.
            capacity: None,
            ..linux("home", None)
        };
        let catalog = [mac, expensive, cheap, owned];

        assert_eq!(
            auto_linux_choice(&catalog, true).map(|entry| entry.machine_type.as_str()),
            Some("home")
        );
        assert_eq!(
            auto_linux_choice(&catalog[..3], true).map(|entry| entry.machine_type.as_str()),
            Some("right-sized")
        );
        assert!(auto_linux_choice(&catalog[..1], true).is_none());
    }

    #[test]
    fn an_entry_without_an_account_cannot_be_chosen() {
        let mut orphan = linux("right-sized", Some(Usd::from_cents(1)));
        orphan.account = None;
        assert!(auto_linux_choice(&[orphan], true).is_none());
    }

    #[test]
    fn a_machine_under_the_floor_is_never_chosen_automatically() {
        // Cheaper than the eligible entry, and still not the answer: there
        // is no fallback to something too small, only the refusal the
        // caller can act on.
        let tiny = MachineCatalogEntry {
            capacity: Some(MachineCapacity {
                vcpus: AUTO_MIN_VCPUS - 1,
                memory_mib: AUTO_MIN_MEMORY_MIB,
            }),
            ..linux("t3.small", Some(Usd::from_cents(1)))
        };
        let starved = MachineCatalogEntry {
            capacity: Some(MachineCapacity {
                vcpus: AUTO_MIN_VCPUS,
                memory_mib: AUTO_MIN_MEMORY_MIB - 1,
            }),
            ..linux("c7g.xlarge", Some(Usd::from_cents(1)))
        };
        let eligible = linux("m7g.xlarge", Some(Usd::from_cents(9)));

        assert!(!tiny.is_auto_eligible());
        assert!(!starved.is_auto_eligible());
        assert!(eligible.is_auto_eligible());

        // A granted container is held to the cores and not to the memory:
        // Container Apps stops at 4 vCPU / 8 GiB, and that size is the one
        // Auto exists to find.
        let granted = MachineCatalogEntry {
            runtime: Runtime::Container,
            free_grant: Some(aca_grant()),
            ..starved.clone()
        };
        assert!(granted.is_auto_eligible());
        let granted_small = MachineCatalogEntry {
            capacity: Some(MachineCapacity {
                vcpus: AUTO_MIN_VCPUS - 2,
                memory_mib: 4 * 1024,
            }),
            ..granted
        };
        assert!(!granted_small.is_auto_eligible());
        assert_eq!(
            auto_linux_choice(&[tiny.clone(), starved.clone(), eligible], true)
                .map(|entry| entry.machine_type.as_str()),
            Some("m7g.xlarge")
        );
        assert!(auto_linux_choice(&[tiny, starved], true).is_none());
    }

    /// The Azure Container Apps grant, as the driver PR will state it.
    fn aca_grant() -> FreeGrant {
        FreeGrant {
            vcpu_seconds_per_month: 180_000,
            gib_seconds_per_month: 360_000,
        }
    }

    #[test]
    fn a_container_the_provider_gives_away_wins_over_everything_cheaper() {
        // Including hardware the user owns, which is otherwise first: the
        // grant expires unspent at the end of the month and the machine at
        // home does not.
        let granted = MachineCatalogEntry {
            runtime: Runtime::Container,
            free_grant: Some(aca_grant()),
            ..linux("aca-4x8", Some(Usd::from_cents(21)))
        };
        let owned = MachineCatalogEntry {
            capacity: None,
            ..linux("home", None)
        };
        let cheap = linux("right-sized", Some(Usd::from_cents(2)));

        assert_eq!(
            auto_linux_choice(&[owned.clone(), cheap.clone(), granted.clone()], true)
                .map(|entry| entry.machine_type.as_str()),
            Some("aca-4x8")
        );
        // Without a grant the container is priced like anything else, and
        // the old rule stands untouched.
        let ungranted = MachineCatalogEntry {
            free_grant: None,
            ..granted
        };
        assert_eq!(
            auto_linux_choice(&[owned, cheap, ungranted], true)
                .map(|entry| entry.machine_type.as_str()),
            Some("home")
        );
    }

    #[test]
    fn two_grants_draw_on_one_pool_so_the_cheaper_container_wins() {
        let dear = MachineCatalogEntry {
            runtime: Runtime::Container,
            free_grant: Some(aca_grant()),
            ..linux("aca-8x16", Some(Usd::from_cents(42)))
        };
        let cheap = MachineCatalogEntry {
            runtime: Runtime::Container,
            free_grant: Some(aca_grant()),
            ..linux("aca-4x8", Some(Usd::from_cents(21)))
        };

        assert_eq!(
            auto_linux_choice(&[dear, cheap], true).map(|entry| entry.machine_type.as_str()),
            Some("aca-4x8")
        );
    }

    #[test]
    fn a_runtime_round_trips_as_its_wire_token_and_defaults_to_a_vm() {
        assert_eq!(
            serde_json::to_string(&Runtime::Container).expect("serialize"),
            "\"container\""
        );
        assert_eq!(Runtime::default(), Runtime::Vm);
        assert!(Runtime::Vm.keeps_disk());
        assert!(!Runtime::Container.keeps_disk());

        // A spec stored before the axis existed describes a VM, and reads
        // back as one rather than failing to parse.
        let older = serde_json::json!({
            "provider": "azure",
            "machine_type": "Standard_D4s_v6",
            "region": "westeurope",
            "spot": true,
            "disk_gib": 64,
        });
        let spec: MachineSpec = serde_json::from_value(older).expect("deserialize");
        assert_eq!(spec.runtime, Runtime::Vm);
    }

    #[test]
    fn a_container_entry_carries_its_grant_across_the_wire() {
        let entry = MachineCatalogEntry {
            runtime: Runtime::Container,
            free_grant: Some(aca_grant()),
            ..linux("aca-4x8", Some(Usd::from_cents(21)))
        };

        let json = serde_json::to_value(&entry).expect("serialize");
        assert_eq!(json["runtime"], "container");
        assert_eq!(json["free_grant"]["vcpu_seconds_per_month"], 180_000);
        assert!(entry.has_free_grant());
        assert_eq!(
            serde_json::from_value::<MachineCatalogEntry>(json).expect("deserialize"),
            entry
        );

        // A VM says nothing at all about a grant it does not have.
        let vm = linux("m7g.xlarge", Some(Usd::from_cents(9)));
        let json = serde_json::to_value(&vm).expect("serialize");
        assert_eq!(json["runtime"], "vm");
        assert!(json.get("free_grant").is_none());
        assert!(!vm.has_free_grant());
    }
}
