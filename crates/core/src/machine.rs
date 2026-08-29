//! Machines: cloud providers, the priced catalog agents choose from, and
//! machine lifecycle state.

use serde::{Deserialize, Serialize};

use crate::id::{MachineId, SessionId};
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

/// How much compute a catalog entry offers.
///
/// Absent from an entry the provider does not publish a size for — see
/// [`MachineCatalogEntry::capacity`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MachineCapacity {
    /// Virtual CPU count.
    pub vcpus: u32,
    /// Memory in MiB.
    pub memory_mib: u64,
}

/// What an hour on a machine costs.
///
/// A sum rather than an amount with a nullable field, because "the provider
/// bills nothing for this" and "the price is zero" are different claims and
/// only one of them is ever true. A machine the user already owns has no
/// rate flyco could quote, and quoting `$0.00` would tell a budget it can
/// run forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MachinePricing {
    /// The provider meters this machine and bills for it by the hour.
    Metered {
        /// On-demand price per hour.
        on_demand_hourly: Usd,
        /// Spot price per hour, when the type is available as spot.
        spot_hourly: Option<Usd>,
        /// Minimum billing commitment in hours, when the provider imposes
        /// one (e.g. EC2 Mac dedicated hosts bill a 24-hour minimum under
        /// the Apple license). The agent sees this before choosing.
        minimum_billing_hours: Option<u32>,
    },
    /// Hardware the user already owns and already pays for. Flyco meters
    /// nothing on it and a session running here spends no budget.
    UserOwned,
}

impl MachinePricing {
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
}

/// One entry in the machine catalog the agent sees when deciding whether
/// to keep, upgrade, or downgrade its machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MachineCatalogEntry {
    /// Which provider offers it.
    pub provider: CloudProviderKind,
    /// Provider-native region this entry is offered in.
    ///
    /// A catalog spans every region an account may deploy into, so an entry
    /// without one would not say where the machine it describes can be
    /// created. A registered SSH host names itself here: it is its own
    /// region, and there is nowhere else to put it.
    pub region: String,
    /// Provider-native machine type name (e.g. `Standard_B2ats_v2`).
    pub machine_type: String,
    /// Operating system family.
    pub os: OsFamily,
    /// How big it is, when the provider publishes a size.
    ///
    /// A cloud SKU always does. A host the user registered over SSH has
    /// whatever hardware it has, and flyco does not learn that until a
    /// daemon runs on it and says so — inventing a size here would be a
    /// number the agent could plan against and be wrong about.
    pub capacity: Option<MachineCapacity>,
    /// What it costs to run for an hour.
    pub pricing: MachinePricing,
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
    /// Provider-native region it landed in.
    pub region: String,
    /// When it was created, seconds since the Unix epoch.
    pub created_at_unix: u64,
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
        CloudProviderKind, MachineCapacity, MachineCatalogEntry, MachinePricing, OsFamily,
    };
    use crate::money::Usd;

    #[test]
    fn a_metered_entry_prices_spot_when_there_is_a_spot_price() {
        let pricing = MachinePricing::Metered {
            on_demand_hourly: Usd::from_micros(8_400),
            spot_hourly: Some(Usd::from_micros(7_560)),
            minimum_billing_hours: None,
        };

        assert_eq!(pricing.hourly(true), Some(Usd::from_micros(7_560)));
        assert_eq!(pricing.hourly(false), Some(Usd::from_micros(8_400)));
    }

    #[test]
    fn a_type_with_no_spot_meter_is_priced_on_demand_either_way() {
        let pricing = MachinePricing::Metered {
            on_demand_hourly: Usd::from_micros(8_400),
            spot_hourly: None,
            minimum_billing_hours: Some(24),
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
            region: "northcentralus".to_owned(),
            provider: CloudProviderKind::Azure,
            machine_type: "Standard_B2pts_v2".to_owned(),
            os: OsFamily::Linux,
            capacity: Some(MachineCapacity {
                vcpus: 2,
                memory_mib: 1_024,
            }),
            pricing: MachinePricing::Metered {
                on_demand_hourly: Usd::from_micros(8_400),
                spot_hourly: None,
                minimum_billing_hours: None,
            },
        };

        let json = serde_json::to_value(&entry).expect("serialize");
        assert_eq!(json["pricing"]["kind"], "metered");
        assert_eq!(
            serde_json::from_value::<MachineCatalogEntry>(json).expect("deserialize"),
            entry
        );
    }
}
