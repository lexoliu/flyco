//! Whether the account's quota covers one more machine.
//!
//! The same distinction Azure draws, in AWS's vocabulary: **spot draws on a
//! different pool**. `Running On-Demand Standard (A, C, D, H, I, M, R, T, Z)
//! instances` and `All Standard (A, C, D, H, I, M, R, T, Z) Spot Instance
//! Requests` are separate quotas with separate limits, both counted in
//! vCPUs, and checking a spot request against the on-demand limit would
//! refuse machines the account can genuinely run.
//!
//! # The class comes out of the quota's own name
//!
//! A quota's name states which instance families it covers — `Standard (A,
//! C, D, H, I, M, R, T, Z)`, `P`, `G and VT`, `Inf`, `Dedicated mac2` — so
//! the mapping from an instance type to the quota that binds it is read from
//! the account's own answer rather than from a table in this file that would
//! be wrong the week AWS adds a family. `t4g.small` is family `t`;
//! `mac2.metal` is family `mac2`; `inf2.xlarge` is family `inf`.
//!
//! # Limits come from Service Quotas, usage from the instances themselves
//!
//! Azure publishes both halves in one list. Service Quotas publishes only
//! the limit, so the used half is counted from the account's own running
//! instances, whose types are the ones the catalog already read. Splitting
//! the read is not a design choice; it is what the two services offer.

use serde::Deserialize;

use crate::{CapacityMode, ProviderError, QuotaUnit};

use super::ec2::InstanceTypeInfo;

/// Signing name of the Service Quotas API.
pub const SERVICE: &str = "servicequotas";

/// The JSON-RPC target that lists a service's quotas.
pub const LIST_TARGET: &str = "ServiceQuotasV20190624.ListServiceQuotas";

/// The service whose quotas bind an instance launch.
pub const EC2_SERVICE_CODE: &str = "ec2";

/// Body of `ListServiceQuotas`.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ListServiceQuotas {
    /// `ec2`.
    pub service_code: &'static str,
    /// Page size, at the API's maximum.
    pub max_results: u32,
    /// Continuation token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_token: Option<String>,
}

/// One page of quotas.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct QuotaPage {
    /// The quotas on this page.
    #[serde(default)]
    pub quotas: Vec<Quota>,
    /// The next page, when there is one.
    #[serde(default)]
    pub next_token: Option<String>,
}

/// One quota, as Service Quotas reports it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Quota {
    /// Its code, e.g. `L-1216C47A`, which is what a support request names.
    #[serde(default)]
    pub quota_code: String,
    /// Its human-readable name, which is also what says what it covers.
    #[serde(default)]
    pub quota_name: String,
    /// The limit. Absent on a quota AWS reports without a value.
    #[serde(default)]
    pub value: Option<f64>,
}

/// Which launch a quota governs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Market {
    /// Ordinary capacity.
    OnDemand,
    /// Interruptible capacity.
    Spot,
}

impl From<CapacityMode> for Market {
    fn from(mode: CapacityMode) -> Self {
        if mode.is_spot() {
            Self::Spot
        } else {
            Self::OnDemand
        }
    }
}

/// One quota, read as "which families, which market, counted in what".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coverage {
    /// The instance-family tokens it covers, lowercased: `["a", "c", "d", …]`
    /// for the standard quota, `["mac2"]` for a Mac host quota.
    pub families: Vec<String>,
    /// Which market.
    pub market: Market,
    /// What it counts.
    /// What this quota counts. AWS states the Mac families in whole
    /// dedicated hosts and everything else in vCPUs, which decides what
    /// "one more machine" costs against it. Flyco does not enumerate the
    /// hosts already allocated, so a host limit above zero is entitlement
    /// rather than headroom; EC2 answers `InsufficientHostCapacity` — an
    /// actionable refusal naming the real problem — when the entitlement is
    /// real but spent.
    pub unit: QuotaUnit,
}

/// Prefix of an on-demand instance-launch quota's name.
const ON_DEMAND_PREFIX: &str = "Running On-Demand ";

/// Suffix of an on-demand instance-launch quota's name.
const ON_DEMAND_SUFFIX: &str = " instances";

/// Prefix of a spot-request quota's name.
const SPOT_PREFIX: &str = "All ";

/// Suffix of a spot-request quota's name.
const SPOT_SUFFIX: &str = " Spot Instance Requests";

/// Prefix of a dedicated-host quota's name.
const HOST_PREFIX: &str = "Running Dedicated ";

/// Suffix of a dedicated-host quota's name.
const HOST_SUFFIX: &str = " Hosts";

/// The families a quota's class description covers.
///
/// `Standard (A, C, D, H, I, M, R, T, Z)` is the parenthesised list;
/// `G and VT` is the two words either side of the conjunction; `P` is the
/// single word. The parenthesised form wins where both are present, because
/// the word before it (`Standard`) is a label rather than a family.
fn families_of(class: &str) -> Vec<String> {
    if let Some(open) = class.find('(')
        && let Some(close) = class[open..].find(')')
    {
        return class[open + 1..open + close]
            .split(',')
            .map(|family| family.trim().to_ascii_lowercase())
            .filter(|family| !family.is_empty())
            .collect();
    }

    class
        .split(" and ")
        .map(|family| family.trim().to_ascii_lowercase())
        .filter(|family| !family.is_empty())
        .collect()
}

/// The market and unit a quota's name puts it in, with the class
/// description it states.
///
/// Three shapes govern a launch and nothing else does, so anything that
/// matches none of them answers `None` rather than being forced into the
/// model.
fn classify(name: &str) -> Option<(&str, Market, QuotaUnit)> {
    if let Some(class) = name
        .strip_prefix(ON_DEMAND_PREFIX)
        .and_then(|rest| rest.strip_suffix(ON_DEMAND_SUFFIX))
    {
        return Some((class, Market::OnDemand, QuotaUnit::Vcpus));
    }
    if let Some(class) = name
        .strip_prefix(SPOT_PREFIX)
        .and_then(|rest| rest.strip_suffix(SPOT_SUFFIX))
    {
        return Some((class, Market::Spot, QuotaUnit::Vcpus));
    }
    let class = name
        .strip_prefix(HOST_PREFIX)
        .and_then(|rest| rest.strip_suffix(HOST_SUFFIX))?;
    Some((class, Market::OnDemand, QuotaUnit::Hosts))
}

impl Quota {
    /// What this quota covers, when it is one of the three shapes that
    /// govern a launch.
    ///
    /// Everything else an account publishes under `ec2` — gateway counts,
    /// AMI limits, rule counts — has nothing to say about starting a
    /// machine, and answers `None` rather than being forced into the model.
    #[must_use]
    pub fn coverage(&self) -> Option<Coverage> {
        let (class, market, unit) = classify(self.quota_name.trim())?;
        let families = families_of(class);
        (!families.is_empty()).then_some(Coverage {
            families,
            market,
            unit,
        })
    }

    /// The limit as a whole number, floored.
    #[must_use]
    pub fn limit(&self) -> u32 {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a service quota is a small non-negative count"
        )]
        let limit = self.value.unwrap_or(0.0).max(0.0) as u32;
        limit
    }
}

/// An instance type's family token, as a quota names it.
///
/// The leading run of letters: `t4g.small` is `t`, `mac2.metal` is `mac`,
/// `inf2.xlarge` is `inf`, `u-6tb1.metal` is `u`. A Mac quota names the
/// generation too (`mac2`), so both forms are offered and the caller matches
/// on either.
#[must_use]
pub fn family_tokens(instance_type: &str) -> Vec<String> {
    let head = instance_type
        .split('.')
        .next()
        .unwrap_or(instance_type)
        .to_ascii_lowercase();
    let letters: String = head.chars().take_while(char::is_ascii_alphabetic).collect();

    if letters.is_empty() {
        return Vec::new();
    }
    if head == letters {
        return vec![letters];
    }
    // `mac2` matches the `Dedicated mac2 Hosts` quota; `t` matches the
    // standard one. Both are legitimate readings of `mac2.metal`, so both
    // are offered and the most specific match is the one that binds.
    vec![head, letters]
}

/// A region's EC2 quotas, and how much of them is spent.
#[derive(Debug, Clone, Default)]
pub struct Quotas {
    covered: Vec<(Coverage, String, u32)>,
    used_vcpus: Vec<(Market, String, u32)>,
}

impl Quotas {
    /// Builds a lookup from the account's quotas and its running instances.
    ///
    /// `running` is every instance already on compute, paired with the shape
    /// of its type: the vCPU count is what a quota is spent in, and EC2 does
    /// not state it on the instance.
    #[must_use]
    pub fn new(quotas: Vec<Quota>, running: &[(Market, &InstanceTypeInfo)]) -> Self {
        let covered = quotas
            .into_iter()
            .filter_map(|quota| {
                let coverage = quota.coverage()?;
                Some((coverage, quota.quota_code.clone(), quota.limit()))
            })
            .collect();

        let mut used_vcpus: Vec<(Market, String, u32)> = Vec::new();
        for (market, info) in running {
            for family in family_tokens(&info.instance_type) {
                if let Some(entry) = used_vcpus
                    .iter_mut()
                    .find(|(seen, name, _)| seen == market && *name == family)
                {
                    entry.2 = entry.2.saturating_add(info.vcpu_info.default_vcpus);
                } else {
                    used_vcpus.push((*market, family, info.vcpu_info.default_vcpus));
                }
            }
        }

        Self {
            covered,
            used_vcpus,
        }
    }

    /// The quota that binds one instance type in one market.
    ///
    /// The most specific match wins: `mac2.metal` is covered by both the
    /// `mac2` host quota and — on the letter alone — nothing else, and a
    /// type whose family appears in a family-specific quota must not be
    /// checked against the standard pool.
    #[must_use]
    pub fn binding(&self, instance_type: &str, market: Market) -> Option<&(Coverage, String, u32)> {
        let tokens = family_tokens(instance_type);
        tokens.iter().find_map(|token| {
            self.covered.iter().find(|(coverage, _, _)| {
                coverage.market == market && coverage.families.iter().any(|family| family == token)
            })
        })
    }

    /// How many vCPUs of one family are already running in one market.
    #[must_use]
    pub fn used(&self, family: &str, market: Market) -> u32 {
        self.used_vcpus
            .iter()
            .filter(|(seen, name, _)| *seen == market && name == family)
            .map(|(_, _, vcpus)| *vcpus)
            .sum()
    }

    /// Refuses unless the pool that funds this capacity mode covers one more
    /// machine of this type.
    ///
    /// A type the account publishes no launch quota for is an error rather
    /// than an unlimited allowance, for the reason Azure's is: an unknown
    /// quota name is a name this driver got wrong, and provisioning against
    /// it would fail later with an opaque refusal instead of here with the
    /// name.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::QuotaExceeded`] when the headroom is not
    /// there, or [`ProviderError::Unavailable`] when nothing covers the
    /// type — including a spot request for a type outside the spot market.
    pub fn require_capacity_for(
        &self,
        instance_type: &InstanceTypeInfo,
        region: &str,
        mode: CapacityMode,
    ) -> Result<(), ProviderError> {
        let market = Market::from(mode);
        let name = instance_type.instance_type.as_str();

        if market == Market::Spot && !instance_type.supports_spot() {
            return Err(ProviderError::Unavailable {
                machine_type: name.to_owned(),
                region: region.to_owned(),
                reason: "EC2 does not sell this instance type on the spot market".to_owned(),
            });
        }

        let Some((coverage, code, limit)) = self.binding(name, market) else {
            return Err(ProviderError::Unavailable {
                machine_type: name.to_owned(),
                region: region.to_owned(),
                reason: "the account publishes no launch quota covering this instance family"
                    .to_owned(),
            });
        };

        let (used, requested) = match coverage.unit {
            QuotaUnit::Vcpus => {
                let family = family_tokens(name)
                    .into_iter()
                    .find(|token| coverage.families.contains(token))
                    .unwrap_or_default();
                (
                    self.used(&family, market),
                    instance_type.vcpu_info.default_vcpus,
                )
            }
            // One host, and an allocation flyco does not enumerate.
            QuotaUnit::Hosts => (0, 1),
        };

        if used.saturating_add(requested) <= *limit {
            return Ok(());
        }

        Err(ProviderError::QuotaExceeded {
            unit: coverage.unit,
            quota: code.clone(),
            region: region.to_owned(),
            limit: *limit,
            used,
            requested,
        })
    }

    /// Whether an instance type is billed against a dedicated-host quota.
    ///
    /// The Mac families are, and a launch of one has to name host tenancy —
    /// which is the whole of the difference between them and every other
    /// instance type flyco can start.
    #[must_use]
    pub fn needs_dedicated_host(&self, instance_type: &str) -> bool {
        self.binding(instance_type, Market::OnDemand)
            .is_some_and(|(coverage, _, _)| coverage.unit == QuotaUnit::Hosts)
    }
}

#[cfg(test)]
mod tests {
    use super::{Market, QuotaPage, Quotas, family_tokens};
    use crate::CapacityMode;
    use crate::ProviderError;
    use crate::QuotaUnit;
    use crate::aws::ec2::{
        ArchitectureSet, DescribeInstanceTypesResponse, InstanceTypeInfo, ProcessorInfo,
    };

    fn quotas_from(json: &str) -> Vec<super::Quota> {
        serde_json::from_str::<QuotaPage>(json)
            .expect("the quota fixture parses")
            .quotas
    }

    fn types() -> Vec<InstanceTypeInfo> {
        crate::aws::ec2::decode::<DescribeInstanceTypesResponse>(&crate::http::HttpResponse::new(
            200,
            include_bytes!("../../fixtures/aws/describe_instance_types.xml").to_vec(),
        ))
        .expect("the instance-type fixture parses")
        .instance_type_set
        .item
    }

    fn info(name: &str) -> InstanceTypeInfo {
        types()
            .into_iter()
            .find(|entry| entry.instance_type == name)
            .unwrap_or_else(|| panic!("the fixture holds `{name}`"))
    }

    fn quotas() -> Quotas {
        Quotas::new(
            quotas_from(include_str!("../../fixtures/aws/list_service_quotas.json")),
            &[],
        )
    }

    #[test]
    fn a_family_token_is_the_leading_letters_and_the_generation() {
        assert_eq!(family_tokens("t4g.small"), vec!["t4g", "t"]);
        assert_eq!(family_tokens("mac2.metal"), vec!["mac2", "mac"]);
        assert_eq!(family_tokens("p4d.24xlarge"), vec!["p4d", "p"]);
    }

    #[test]
    fn a_quotas_coverage_is_read_out_of_its_own_name() {
        let quotas = quotas_from(include_str!("../../fixtures/aws/list_service_quotas.json"));
        let coverage = |name: &str| {
            quotas
                .iter()
                .find(|quota| quota.quota_name == name)
                .unwrap_or_else(|| panic!("the fixture holds `{name}`"))
                .coverage()
        };

        let standard = coverage("Running On-Demand Standard (A, C, D, H, I, M, R, T, Z) instances")
            .expect("an on-demand launch quota");
        assert!(standard.families.contains(&"t".to_owned()));
        assert_eq!(standard.market, Market::OnDemand);
        assert_eq!(standard.unit, QuotaUnit::Vcpus);

        let spot = coverage("All Standard (A, C, D, H, I, M, R, T, Z) Spot Instance Requests")
            .expect("a spot launch quota");
        assert_eq!(spot.market, Market::Spot);

        let hosts = coverage("Running Dedicated mac2 Hosts").expect("a dedicated-host quota");
        assert_eq!(hosts.families, vec!["mac2".to_owned()]);
        assert_eq!(hosts.unit, QuotaUnit::Hosts);

        // A quota that has nothing to do with starting a machine stays out
        // of the model rather than being forced into it.
        assert!(coverage("VPCs per Region").is_none());
    }

    #[test]
    fn spot_and_on_demand_are_funded_from_different_pools() {
        let quotas = quotas();
        let standard = info("t3.small");

        assert_eq!(
            quotas
                .binding("t3.small", Market::OnDemand)
                .expect("an on-demand quota")
                .1,
            "L-1216C47A"
        );
        assert_eq!(
            quotas
                .binding("t3.small", Market::Spot)
                .expect("a spot quota")
                .1,
            "L-34B43A08"
        );

        quotas
            .require_capacity_for(&standard, "us-west-2", CapacityMode::Spot)
            .expect("two vCPUs fit under a spot limit of four");
    }

    #[test]
    fn a_pool_the_running_instances_have_spent_refuses_one_more() {
        // Four of the four spot vCPUs are already running, so the fifth does
        // not fit — the same shape as Azure's spent `lowPriorityCores`.
        let running = info("t3.medium");
        let quotas = Quotas::new(
            quotas_from(include_str!("../../fixtures/aws/list_service_quotas.json")),
            &[(Market::Spot, &running), (Market::Spot, &running)],
        );

        let error = quotas
            .require_capacity_for(&info("t3.small"), "us-west-2", CapacityMode::Spot)
            .expect_err("the spot pool is spent");
        assert!(matches!(
            error,
            ProviderError::QuotaExceeded { ref quota, used: 4, limit: 4, .. } if quota == "L-34B43A08"
        ));

        // The on-demand pool is untouched by spot usage, exactly as Azure's
        // family quota is.
        quotas
            .require_capacity_for(&info("t3.small"), "us-west-2", CapacityMode::OnDemand)
            .expect("the on-demand pool is funded separately");
    }

    #[test]
    fn a_type_outside_the_spot_market_is_refused_as_spot_and_allowed_as_on_demand() {
        let quotas = quotas();
        let mac = info("mac2.metal");

        let error = quotas
            .require_capacity_for(&mac, "us-west-2", CapacityMode::Spot)
            .expect_err("EC2 sells no Mac on the spot market");
        assert!(matches!(error, ProviderError::Unavailable { .. }));

        // Its entitlement is a dedicated host rather than a pool of vCPUs.
        quotas
            .require_capacity_for(&mac, "us-west-2", CapacityMode::OnDemand)
            .expect("the account is entitled to a mac2 host");
        assert!(quotas.needs_dedicated_host("mac2.metal"));
        assert!(!quotas.needs_dedicated_host("t3.small"));
    }

    #[test]
    fn a_family_no_quota_covers_is_an_error_rather_than_unlimited() {
        let unknown = InstanceTypeInfo {
            instance_type: "zz9.xlarge".to_owned(),
            supported_usage_classes: super::super::ec2::UsageClassSet {
                item: vec!["on-demand".to_owned()],
            },
            processor_info: ProcessorInfo {
                supported_architectures: ArchitectureSet {
                    item: vec!["x86_64".to_owned()],
                },
            },
            vcpu_info: super::super::ec2::VCpuInfo { default_vcpus: 4 },
            memory_info: super::super::ec2::MemoryInfo { size_in_mib: 8_192 },
        };

        let error = quotas()
            .require_capacity_for(&unknown, "us-west-2", CapacityMode::OnDemand)
            .expect_err("an unknown family must not read as unlimited");
        assert!(matches!(error, ProviderError::Unavailable { .. }));
    }
}
