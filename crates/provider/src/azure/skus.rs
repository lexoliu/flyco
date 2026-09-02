//! Which machine types this subscription can actually deploy, and whether
//! its quota covers one more.
//!
//! Both halves exist because the convenient answers are wrong.
//!
//! * **Availability.** `az vm list-skus` hides every SKU carrying a
//!   `Location` restriction by default — measured at 532 visible of 1366
//!   real in `eastus` — so the driver reads the REST endpoint and classifies
//!   `restrictions` itself. The distinction that matters is `Location`
//!   (unusable in that region, full stop) versus `Zone` (perfectly usable,
//!   but only as a regional deployment). Zone-restricted-but-usable is the
//!   *dominant* pattern for the SKUs a student subscription can run, so a
//!   driver that treats any restriction as "unavailable" — or that sends
//!   `zones` out of habit — fails on almost everything.
//! * **Quota.** Availability is not entitlement. On the measured
//!   subscription 161 of 232 quota entries have a limit of zero, so a
//!   deployable-looking SKU routinely has nowhere to run. The family a SKU
//!   belongs to is read off its own `family` field and used as the quota key
//!   — those strings match the usage list exactly — rather than parsed out
//!   of the SKU name, which does not reliably encode it.
//!
//! **Spot draws on a different pool.** Interruptible capacity bypasses the
//! per-family quota entirely and spends only the region's
//! `lowPriorityCores`, so the check has to know which market is being asked
//! for. Checking a spot request against the family quota would refuse
//! machines the subscription can actually run, which on an account whose
//! families are mostly zero-limit means refusing nearly everything.
//!
//! Neither of these is the whole story: a region can be forbidden outright
//! by the subscription's own policy, which neither list mentions. See
//! [`super::policy`].

use flyco_core::machine::{CpuArchitecture, MachineLineage};
use serde::Deserialize;

use crate::{CapacityMode, ProviderError};

/// Resource type the driver cares about in the SKUs list.
pub const VIRTUAL_MACHINES: &str = "virtualMachines";

/// Capability naming the instruction set a SKU runs.
pub const CPU_ARCHITECTURE: &str = "CpuArchitectureType";

/// Capability naming the virtual CPU count.
pub const VCPUS: &str = "vCPUs";

/// Capability naming the memory size, in whole GiB.
pub const MEMORY_GB: &str = "MemoryGB";

/// Quota entry covering every on-demand virtual CPU in a region, whatever
/// the family.
pub const REGIONAL_CORES_QUOTA: &str = "cores";

/// Quota entry covering every interruptible virtual CPU in a region.
///
/// Spot has its own pool and does not touch the per-family quotas, which is
/// why [`Quotas::require_capacity_for`] takes the capacity mode.
pub const LOW_PRIORITY_CORES_QUOTA: &str = "lowPriorityCores";

/// One page of `Microsoft.Compute/skus`.
#[derive(Debug, Clone, Deserialize)]
pub struct SkuPage {
    /// The SKUs on this page.
    #[serde(default)]
    pub value: Vec<Sku>,
    /// The next page, when the list is longer than one.
    #[serde(rename = "nextLink")]
    pub next_link: Option<String>,
}

/// One resource SKU as ARM reports it.
#[derive(Debug, Clone, Deserialize)]
pub struct Sku {
    /// Provider-native name, e.g. `Standard_B2pts_v2`.
    pub name: String,
    /// The resource type it is a SKU of.
    #[serde(rename = "resourceType")]
    pub resource_type: String,
    /// Quota family, which is also the quota key. Absent on SKUs that are
    /// not billed against a family.
    #[serde(default)]
    pub family: Option<String>,
    /// Regions it exists in.
    #[serde(default)]
    pub locations: Vec<String>,
    /// Everything ARM says about its shape.
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    /// Why it cannot be used, where it cannot.
    #[serde(default)]
    pub restrictions: Vec<Restriction>,
}

/// One named property of a SKU.
#[derive(Debug, Clone, Deserialize)]
pub struct Capability {
    /// Property name.
    pub name: String,
    /// Property value, always a string in this API.
    pub value: String,
}

/// One reason a SKU is not usable somewhere.
#[derive(Debug, Clone, Deserialize)]
pub struct Restriction {
    /// `Location` or `Zone`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Why, e.g. `NotAvailableForSubscription`.
    #[serde(rename = "reasonCode", default)]
    pub reason_code: String,
    /// What the restriction applies to.
    #[serde(default)]
    pub values: Vec<String>,
    /// Which regions and zones it applies to.
    #[serde(rename = "restrictionInfo", default)]
    pub info: Option<RestrictionInfo>,
}

/// Where a restriction applies.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RestrictionInfo {
    /// Regions.
    #[serde(default)]
    pub locations: Vec<String>,
    /// Zones within those regions.
    #[serde(default)]
    pub zones: Vec<String>,
}

/// Reads an architecture out of the `CpuArchitectureType` capability.
///
/// Azure spells the two values `x64` and `Arm64`; the rest of flyco spells
/// them `x86_64` and `arm64`, because that is what EC2, Compute Engine and
/// every user spell them. The translation lives here, at the boundary, and
/// nothing downstream carries an Azure-shaped enum.
#[must_use]
pub fn parse_architecture(value: &str) -> Option<CpuArchitecture> {
    match value {
        "x64" => Some(CpuArchitecture::X8664),
        "Arm64" => Some(CpuArchitecture::Arm64),
        _ => None,
    }
}

/// The prefix Azure puts on every quota family name.
pub const FAMILY_PREFIX: &str = "standard";

/// The suffix Azure puts on every quota family name.
pub const FAMILY_SUFFIX: &str = "Family";

/// Splits a quota family into a generation-free key and its version.
///
/// The quota family — `standardDSv6Family`, `standardBasv2Family`,
/// `standardNCFamily` — is Azure's own statement of which line-up a SKU
/// belongs to, matched case-insensitively against the usage list, and a far
/// better source than the SKU name: the name encodes size and features
/// alongside the family, and the field does not.
///
/// The key is lowercased because Azure is not internally consistent about
/// the capitalisation (`standardDSv5Family` and `StandardDsv7Family` appear
/// on one subscription), and two spellings of one family would look like two
/// families and hide neither's older generation.
#[must_use]
pub fn parse_family(family: &str) -> (String, Option<u32>) {
    let without_prefix = family
        .strip_prefix(FAMILY_PREFIX)
        .or_else(|| family.strip_prefix("Standard"))
        .unwrap_or(family);
    let trimmed = without_prefix
        .strip_suffix(FAMILY_SUFFIX)
        .unwrap_or(without_prefix);

    // The version is a trailing `v<digits>`; anything else is part of the
    // family's own name, as `standardNCFamily` is.
    let digits: String = trimmed
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect();
    let head = &trimmed[..trimmed.len() - digits.len()];
    match (digits.is_empty(), head.strip_suffix(['v', 'V'])) {
        (false, Some(key)) => (
            key.to_ascii_lowercase(),
            digits.chars().rev().collect::<String>().parse().ok(),
        ),
        _ => (trimmed.to_ascii_lowercase(), None),
    }
}

/// What a SKU's restrictions mean for one region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// No restriction: deployable, zonal or regional.
    Unrestricted,
    /// Zone-restricted only. Deployable, but the request must name no
    /// zones — a zonal deployment fails and a regional one succeeds.
    RegionalOnly,
    /// Location-restricted: not deployable in this region on this
    /// subscription, at any size, in any zone.
    Unavailable {
        /// The reason code ARM gave.
        reason: String,
    },
}

impl Availability {
    /// Whether a machine can be created from this SKU here.
    #[must_use]
    pub const fn is_deployable(&self) -> bool {
        matches!(self, Self::Unrestricted | Self::RegionalOnly)
    }
}

impl Sku {
    /// Whether this is a virtual-machine SKU rather than a disk or a host.
    #[must_use]
    pub fn is_virtual_machine(&self) -> bool {
        self.resource_type == VIRTUAL_MACHINES
    }

    /// One capability's value.
    #[must_use]
    pub fn capability(&self, name: &str) -> Option<&str> {
        self.capabilities
            .iter()
            .find(|capability| capability.name == name)
            .map(|capability| capability.value.as_str())
    }

    /// The instruction set this SKU runs.
    #[must_use]
    pub fn architecture(&self) -> Option<CpuArchitecture> {
        self.capability(CPU_ARCHITECTURE)
            .and_then(parse_architecture)
    }

    /// Where this SKU sits in Azure's line-up, from its quota family.
    ///
    /// `None` when ARM published no family or no architecture for it, which
    /// makes it a SKU flyco cannot place *and* cannot choose an image for —
    /// see [`super::AzureProvider::entry_for`], which excludes it.
    #[must_use]
    pub fn lineage(&self) -> Option<MachineLineage> {
        let (family, generation) = parse_family(self.family.as_ref()?);
        Some(MachineLineage {
            architecture: self.architecture()?,
            family,
            generation,
        })
    }

    /// Virtual CPU count.
    #[must_use]
    pub fn vcpus(&self) -> Option<u32> {
        self.capability(VCPUS).and_then(|value| value.parse().ok())
    }

    /// Memory in MiB, from the `MemoryGB` capability.
    #[must_use]
    pub fn memory_mib(&self) -> Option<u64> {
        let gib: f64 = self.capability(MEMORY_GB).and_then(|v| v.parse().ok())?;
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "MemoryGB is a small positive number of gibibytes"
        )]
        let mib = (gib * 1024.0) as u64;
        Some(mib)
    }

    /// What this SKU's restrictions mean in `region`.
    ///
    /// A `Location` restriction anywhere in the list is decisive; otherwise
    /// a `Zone` restriction downgrades the deployment to regional. The
    /// `restrictionInfo.locations` list is consulted so a restriction
    /// recorded against another region does not disqualify this one.
    #[must_use]
    pub fn availability(&self, region: &str) -> Availability {
        let applies = |restriction: &Restriction| {
            restriction.info.as_ref().is_none_or(|info| {
                info.locations.is_empty()
                    || info
                        .locations
                        .iter()
                        .any(|location| location.eq_ignore_ascii_case(region))
            })
        };

        let mut zone_restricted = false;
        for restriction in self.restrictions.iter().filter(|r| applies(r)) {
            match restriction.kind.as_str() {
                "Location" => {
                    return Availability::Unavailable {
                        reason: restriction.reason_code.clone(),
                    };
                }
                "Zone" => zone_restricted = true,
                _ => {}
            }
        }

        if zone_restricted {
            Availability::RegionalOnly
        } else {
            Availability::Unrestricted
        }
    }
}

/// The usage list for one region.
#[derive(Debug, Clone, Deserialize)]
pub struct UsagePage {
    /// One entry per quota.
    #[serde(default)]
    pub value: Vec<Usage>,
}

/// One quota, and how much of it is spent.
#[derive(Debug, Clone, Deserialize)]
pub struct Usage {
    /// The quota's names.
    pub name: UsageName,
    /// How much is already in use.
    #[serde(rename = "currentValue")]
    pub current_value: i64,
    /// The limit. Negative means unlimited.
    pub limit: i64,
}

/// A quota's machine-readable and display names.
#[derive(Debug, Clone, Deserialize)]
pub struct UsageName {
    /// Machine-readable name, e.g. `standardBpsv2Family`. This is what a
    /// SKU's `family` field matches.
    pub value: String,
}

/// A region's quotas, ready to be asked whether one more machine fits.
#[derive(Debug, Clone, Default)]
pub struct Quotas {
    entries: Vec<(String, i64, i64)>,
}

impl Quotas {
    /// Builds a lookup from one region's usage list.
    #[must_use]
    pub fn from_page(page: UsagePage) -> Self {
        Self {
            entries: page
                .value
                .into_iter()
                .map(|usage| (usage.name.value, usage.current_value, usage.limit))
                .collect(),
        }
    }

    /// The `(used, limit)` pair for one quota, matched case-insensitively.
    ///
    /// Case-insensitively because the usage list and the SKUs list disagree
    /// on the capitalisation of a family name (`standardDSv5Family` against
    /// `StandardDsv7Family` in the same subscription), and a lookup that
    /// missed would read as "no such quota" — which is indistinguishable
    /// from unlimited if you let it be.
    #[must_use]
    pub fn get(&self, quota: &str) -> Option<(i64, i64)> {
        self.entries
            .iter()
            .find(|(name, _, _)| name.eq_ignore_ascii_case(quota))
            .map(|(_, used, limit)| (*used, *limit))
    }

    /// Refuses unless `vcpus` more virtual CPUs fit under `quota`.
    ///
    /// A quota the region does not list is not treated as unlimited: it is
    /// a name the driver got wrong, and provisioning against it would fail
    /// later with an opaque `QuotaExceeded` instead of here with the name.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::QuotaExceeded`] when the headroom is not
    /// there, or [`ProviderError::Unavailable`] when the quota is unknown.
    pub fn require(&self, quota: &str, region: &str, vcpus: u32) -> Result<(), ProviderError> {
        let Some((used, limit)) = self.get(quota) else {
            return Err(ProviderError::Unavailable {
                machine_type: quota.to_owned(),
                region: region.to_owned(),
                reason: "the subscription publishes no quota under this name".to_owned(),
            });
        };

        // A negative limit is Azure's way of saying unlimited.
        if limit < 0 {
            return Ok(());
        }
        if used.saturating_add(i64::from(vcpus)) <= limit {
            return Ok(());
        }

        Err(ProviderError::QuotaExceeded {
            quota: quota.to_owned(),
            region: region.to_owned(),
            limit: u32::try_from(limit).unwrap_or(u32::MAX),
            used: u32::try_from(used).unwrap_or(u32::MAX),
            requested: vcpus,
        })
    }

    /// Refuses unless the pools that fund this capacity mode cover one more
    /// machine of this size.
    ///
    /// On-demand spends two pools and either can bind: the machine type's
    /// family, and the region's total `cores`. A family with room does not
    /// help when the regional total is spent, and vice versa.
    ///
    /// Spot spends exactly one, [`LOW_PRIORITY_CORES_QUOTA`], and no family
    /// quota at all — which is what makes spot reachable on a subscription
    /// whose families are almost entirely zero-limit.
    ///
    /// # Errors
    ///
    /// Returns whichever check fails first.
    pub fn require_capacity_for(
        &self,
        sku: &Sku,
        region: &str,
        mode: CapacityMode,
    ) -> Result<(), ProviderError> {
        let vcpus = sku.vcpus().ok_or(ProviderError::Malformed(
            "an Azure VM SKU reported no vCPU count",
        ))?;

        if mode.is_spot() {
            return self.require(LOW_PRIORITY_CORES_QUOTA, region, vcpus);
        }

        if let Some(family) = &sku.family {
            self.require(family, region, vcpus)?;
        }
        self.require(REGIONAL_CORES_QUOTA, region, vcpus)
    }
}

#[cfg(test)]
mod tests {
    use super::{Availability, CpuArchitecture, Quotas, Sku, SkuPage, UsagePage, parse_family};
    use crate::{CapacityMode, ProviderError};

    /// The SKU list of `northcentralus`, one of the regions the reference
    /// subscription's policy actually permits.
    fn skus() -> Vec<Sku> {
        let page: SkuPage = serde_json::from_str(include_str!(
            "../../fixtures/azure/skus_northcentralus.json"
        ))
        .expect("the SKU fixture parses");
        page.value
    }

    fn sku(name: &str) -> Sku {
        skus()
            .into_iter()
            .find(|sku| sku.name == name)
            .unwrap_or_else(|| panic!("the fixture holds `{name}`"))
    }

    fn arm64_sku() -> Sku {
        let page: SkuPage =
            serde_json::from_str(include_str!("../../fixtures/azure/skus_canadacentral.json"))
                .expect("the SKU fixture parses");
        page.value
            .into_iter()
            .find(|sku| sku.name == "Standard_D2pls_v5")
            .expect("the fixture holds the Arm64 type")
    }

    fn quotas_from(json: &str) -> Quotas {
        let page: UsagePage = serde_json::from_str(json).expect("the usage fixture parses");
        Quotas::from_page(page)
    }

    fn quotas() -> Quotas {
        quotas_from(include_str!("../../fixtures/azure/usages.json"))
    }

    #[test]
    fn an_unrestricted_sku_is_deployable_and_reports_its_shape() {
        let sku = sku("Standard_D2als_v6");
        assert_eq!(
            sku.availability("northcentralus"),
            Availability::Unrestricted
        );
        assert_eq!(sku.architecture(), Some(CpuArchitecture::X8664));
        assert_eq!(sku.vcpus(), Some(2));
        assert_eq!(sku.memory_mib(), Some(4_096));
        assert_eq!(sku.family.as_deref(), Some("StandardDalsv6Family"));
    }

    #[test]
    fn a_quota_family_splits_into_a_key_and_a_version() {
        // The real spellings, from two subscriptions' SKU lists.
        assert_eq!(
            parse_family("StandardDalsv6Family"),
            ("dals".to_owned(), Some(6))
        );
        assert_eq!(
            parse_family("standardDADSv5Family"),
            ("dads".to_owned(), Some(5))
        );
        assert_eq!(
            parse_family("standardBasv2Family"),
            ("bas".to_owned(), Some(2))
        );
        // Case is normalised: one subscription spells one family two ways,
        // and two spellings would look like two families.
        assert_eq!(
            parse_family("standardDSv5Family").0,
            parse_family("StandardDsv5Family").0
        );
        // A family Azure never versioned is a family of one.
        assert_eq!(parse_family("standardNCFamily"), ("nc".to_owned(), None));
        // A trailing number that is not a version stays in the key.
        assert_eq!(
            parse_family("standardND96Family"),
            ("nd96".to_owned(), None)
        );
    }

    #[test]
    fn a_lineage_pairs_the_family_with_the_instruction_set() {
        let lineage = sku("Standard_D2als_v6").lineage().expect("a lineage");
        assert_eq!(lineage.architecture, CpuArchitecture::X8664);
        assert_eq!(lineage.family, "dals");
        assert_eq!(lineage.generation, Some(6));
        assert_eq!(
            arm64_sku().lineage().expect("a lineage").architecture,
            CpuArchitecture::Arm64
        );
    }

    #[test]
    fn a_sku_arm_published_no_family_for_has_no_lineage() {
        let mut unplaced = sku("Standard_D2als_v6");
        unplaced.family = None;
        assert!(unplaced.lineage().is_none());
    }

    #[test]
    fn the_arm64_types_are_recognised_as_arm64() {
        // canadacentral's small unrestricted types are Arm64, which is what
        // makes the image SKU a data-driven choice rather than a constant.
        assert_eq!(arm64_sku().architecture(), Some(CpuArchitecture::Arm64));
    }

    #[test]
    fn a_zone_restricted_sku_is_deployable_but_only_regionally() {
        // Zone-restricted-and-location-clear is the dominant pattern.
        // Reading it as "unavailable" would leave the account with far less
        // than it can run; sending `zones` would fail every deployment.
        let sku = sku("Standard_B2ats_v2");
        assert_eq!(
            sku.availability("northcentralus"),
            Availability::RegionalOnly
        );
        assert!(sku.availability("northcentralus").is_deployable());
    }

    #[test]
    fn a_location_restricted_sku_is_not_deployable_there() {
        let availability = sku("Standard_B2ls_v2").availability("northcentralus");
        assert_eq!(
            availability,
            Availability::Unavailable {
                reason: "NotAvailableForSubscription".to_owned()
            }
        );
        assert!(!availability.is_deployable());
    }

    #[test]
    fn a_restriction_recorded_against_another_region_does_not_bind_here() {
        // A classifier that ignored `restrictionInfo.locations` would
        // disqualify every region at once.
        assert!(
            sku("Standard_B2ls_v2")
                .availability("canadacentral")
                .is_deployable()
        );
    }

    #[test]
    fn a_non_virtual_machine_sku_is_recognised_as_such() {
        assert!(sku("Standard_D2als_v6").is_virtual_machine());
        assert!(!sku("Premium_LRS").is_virtual_machine());
    }

    #[test]
    fn quota_is_read_by_the_skus_own_family_name() {
        let quotas = quotas();
        assert_eq!(quotas.get("StandardDalsv6Family"), Some((0, 10)));
        // The usage list and the SKUs list disagree on capitalisation.
        assert_eq!(quotas.get("standarddalsv6family"), Some((0, 10)));
    }

    #[test]
    fn a_family_with_no_quota_is_refused_before_anything_is_attempted() {
        let error = quotas()
            .require_capacity_for(
                &sku("Standard_D2ads_v5"),
                "northcentralus",
                CapacityMode::OnDemand,
            )
            .expect_err("a zero-limit family cannot take an on-demand machine");

        assert!(matches!(
            error,
            ProviderError::QuotaExceeded {
                limit: 0,
                requested: 2,
                ..
            }
        ));
    }

    #[test]
    fn spot_ignores_the_family_quota_and_spends_the_low_priority_pool() {
        // The same machine type the family quota refuses above is fine as
        // spot: interruptible capacity does not touch a family's quota.
        quotas()
            .require_capacity_for(
                &sku("Standard_D2ads_v5"),
                "northcentralus",
                CapacityMode::Spot,
            )
            .expect("spot draws only on lowPriorityCores");
    }

    #[test]
    fn the_low_priority_pool_binds_spot_and_nothing_else() {
        let spent = quotas_from(include_str!(
            "../../fixtures/azure/usages_low_priority_spent.json"
        ));
        let sku = sku("Standard_D2als_v6");

        let error = spent
            .require_capacity_for(&sku, "northcentralus", CapacityMode::Spot)
            .expect_err("two of three spot vCPUs are already spent");
        assert!(matches!(
            error,
            ProviderError::QuotaExceeded { ref quota, .. } if quota == "lowPriorityCores"
        ));

        spent
            .require_capacity_for(&sku, "northcentralus", CapacityMode::OnDemand)
            .expect("the on-demand pools are untouched by spot usage");
    }

    #[test]
    fn the_regional_core_pool_binds_on_demand_even_when_the_family_has_room() {
        let exhausted = quotas_from(include_str!("../../fixtures/azure/usages_cores_spent.json"));

        let error = exhausted
            .require_capacity_for(
                &sku("Standard_D2als_v6"),
                "northcentralus",
                CapacityMode::OnDemand,
            )
            .expect_err("the regional pool is spent");
        assert!(matches!(
            error,
            ProviderError::QuotaExceeded { ref quota, .. } if quota == "cores"
        ));
    }

    #[test]
    fn a_family_with_room_and_a_free_region_passes() {
        quotas()
            .require_capacity_for(
                &sku("Standard_D2als_v6"),
                "northcentralus",
                CapacityMode::OnDemand,
            )
            .expect("two vCPUs fit under a family limit of ten and a regional pool of six");
    }

    #[test]
    fn a_quota_the_region_does_not_publish_is_an_error_rather_than_unlimited() {
        let error = quotas()
            .require("standardNotAFamily", "northcentralus", 2)
            .expect_err("an unknown quota name must not read as unlimited");
        assert!(matches!(error, ProviderError::Unavailable { .. }));
    }
}
