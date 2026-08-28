//! Machines: cloud providers, the priced catalog agents choose from, and
//! machine lifecycle state.

use serde::{Deserialize, Serialize};

use crate::money::Usd;

/// A supported compute provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CloudProviderKind {
    /// Microsoft Azure.
    Azure,
    /// Amazon Web Services.
    Aws,
    /// Google Cloud Platform.
    Gcp,
    /// A user-registered Linux host reached over SSH, sandboxed with Podman.
    ByoSsh,
}

/// Operating system family of a machine type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OsFamily {
    /// Linux (the default; cheapest and always the first machine).
    Linux,
    /// macOS (dedicated-host constraints apply).
    MacOs,
    /// Windows.
    Windows,
}

/// One entry in the machine catalog the agent sees when deciding whether
/// to keep, upgrade, or downgrade its machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MachineCatalogEntry {
    /// Which provider offers it.
    pub provider: CloudProviderKind,
    /// Provider-native machine type name (e.g. `Standard_B2ats_v2`).
    pub machine_type: String,
    /// Operating system family.
    pub os: OsFamily,
    /// Virtual CPU count.
    pub vcpus: u32,
    /// Memory in MiB.
    pub memory_mib: u64,
    /// On-demand price per hour.
    pub on_demand_hourly: Usd,
    /// Spot price per hour, when the type is available as spot.
    pub spot_hourly: Option<Usd>,
    /// Minimum billing commitment in hours, when the provider imposes one
    /// (e.g. EC2 Mac dedicated hosts bill a 24-hour minimum under the
    /// Apple license). The agent sees this before choosing.
    pub minimum_billing_hours: Option<u32>,
}

/// Everything needed to provision a machine for a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MachineSpec {
    /// Which provider to provision on.
    pub provider: CloudProviderKind,
    /// Provider-native machine type name.
    pub machine_type: String,
    /// Provider-native region name.
    pub region: String,
    /// Whether to request spot capacity (the default).
    pub spot: bool,
    /// Disk size in GiB.
    pub disk_gib: u32,
}

/// Lifecycle state of a provisioned machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
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
