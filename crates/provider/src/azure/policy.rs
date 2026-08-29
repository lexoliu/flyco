//! The subscription's own policy on where it may deploy.
//!
//! This is a **third, independent gate**, and it is invisible from the two
//! obvious ones. A subscription can carry an Azure Policy assignment —
//! typically the built-in "Allowed resource deployment regions" — that
//! refuses every `PUT` into a region outside its list, for *every* resource
//! type including the virtual network. `az vm list-skus` and the SKUs REST
//! endpoint know nothing about it: they happily report a region's machine
//! types as unrestricted while a deployment there fails validation with
//! `RequestDisallowedByAzure`.
//!
//! Measured on the reference subscription: `westus2`, the region with the
//! most deployable machine types by a wide margin, is disallowed outright.
//! A driver that trusted SKU restrictions and quota alone would offer the
//! user a catalog full of machines that cannot be created.
//!
//! # Read, never assumed
//!
//! The allowed list is read from the subscription's policy assignments and
//! keyed on the **parameter name** `listOfAllowedLocations` rather than on a
//! particular policy definition id: the parameter is what has the effect,
//! any definition may carry it, and another user's subscription will not
//! have the same assignment. A subscription with no such assignment —
//! an ordinary pay-as-you-go account — is [`Unrestricted`], which means
//! *every* region, never *no* region.
//!
//! [`Unrestricted`]: RegionPolicy::Unrestricted

use serde::Deserialize;

/// The parameter whose value is the list of deployable regions.
pub const ALLOWED_LOCATIONS_PARAMETER: &str = "listOfAllowedLocations";

/// The policy-assignment list at subscription scope.
#[derive(Debug, Clone, Deserialize)]
pub struct AssignmentPage {
    /// The assignments in scope.
    #[serde(default)]
    pub value: Vec<Assignment>,
    /// The next page, when there is one.
    #[serde(rename = "nextLink")]
    pub next_link: Option<String>,
}

/// One policy assignment.
#[derive(Debug, Clone, Deserialize)]
pub struct Assignment {
    /// Assignment name, used to tell the user which policy refused them.
    #[serde(default)]
    pub name: String,
    /// Its properties.
    #[serde(default)]
    pub properties: AssignmentProperties,
}

/// A policy assignment's properties.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AssignmentProperties {
    /// Human-readable name, when the assignment carries one.
    #[serde(rename = "displayName", default)]
    pub display_name: Option<String>,
    /// Parameter values, keyed by parameter name.
    #[serde(default)]
    pub parameters: std::collections::BTreeMap<String, ParameterValue>,
}

/// One parameter's value.
///
/// Held as raw JSON because an assignment's parameters are typed by its
/// policy definition, not by this driver: a `requireTag` assignment beside
/// the one this module reads carries a plain string, and a struct that
/// insisted on a string array would fail to parse the whole page over a
/// parameter it does not care about.
#[derive(Debug, Clone, Deserialize)]
pub struct ParameterValue {
    /// The value, whatever shape the policy definition gives it.
    #[serde(default)]
    pub value: serde_json::Value,
}

impl ParameterValue {
    /// This value as a list of strings, when that is what it is.
    #[must_use]
    pub fn as_string_list(&self) -> Option<Vec<String>> {
        self.value
            .as_array()?
            .iter()
            .map(|item| item.as_str().map(ToOwned::to_owned))
            .collect()
    }
}

/// Where a subscription is allowed to deploy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegionPolicy {
    /// No assignment names an allowed-locations list, so every region is
    /// permitted. The absence of a policy is permission, not prohibition.
    Unrestricted,
    /// Only these regions, and the policy that says so.
    Allowed {
        /// The permitted regions, lowercased for comparison.
        regions: Vec<String>,
        /// What to name when telling a user why a region was refused.
        policy: String,
    },
}

impl RegionPolicy {
    /// Folds a page of assignments into one decision.
    ///
    /// Several assignments can each name a list, and all of them bind, so
    /// the result is the intersection: a region has to survive every policy
    /// that has an opinion about it.
    #[must_use]
    pub fn from_page(page: &AssignmentPage) -> Self {
        let mut policy = Self::Unrestricted;

        for assignment in &page.value {
            let Some(locations) = assignment
                .properties
                .parameters
                .get(ALLOWED_LOCATIONS_PARAMETER)
                .and_then(ParameterValue::as_string_list)
            else {
                continue;
            };

            let named: Vec<String> = locations
                .iter()
                .map(|region| region.to_ascii_lowercase())
                .collect();
            let label = assignment
                .properties
                .display_name
                .clone()
                .unwrap_or_else(|| assignment.name.clone());

            policy = match policy {
                Self::Unrestricted => Self::Allowed {
                    regions: named,
                    policy: label,
                },
                Self::Allowed { regions, policy } => Self::Allowed {
                    regions: regions
                        .into_iter()
                        .filter(|region| named.contains(region))
                        .collect(),
                    policy: format!("{policy}, {label}"),
                },
            };
        }

        policy
    }

    /// Whether a deployment into `region` is permitted.
    #[must_use]
    pub fn allows(&self, region: &str) -> bool {
        match self {
            Self::Unrestricted => true,
            Self::Allowed { regions, .. } => regions
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(region)),
        }
    }

    /// The permitted regions, when the subscription names any.
    #[must_use]
    pub fn regions(&self) -> Option<&[String]> {
        match self {
            Self::Unrestricted => None,
            Self::Allowed { regions, .. } => Some(regions),
        }
    }

    /// What to tell a user whose region was refused.
    #[must_use]
    pub fn refusal(&self, region: &str) -> String {
        match self {
            Self::Unrestricted => format!("{region} is not permitted"),
            Self::Allowed { regions, policy } => format!(
                "the subscription's policy `{policy}` allows deployments only into {}",
                regions.join(", ")
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AssignmentPage, RegionPolicy};

    fn page(json: &str) -> AssignmentPage {
        serde_json::from_str(json).expect("the fixture parses")
    }

    fn assigned() -> RegionPolicy {
        RegionPolicy::from_page(&page(include_str!(
            "../../fixtures/azure/policy_assignments.json"
        )))
    }

    #[test]
    fn an_allowed_locations_assignment_restricts_the_subscription() {
        let policy = assigned();
        assert!(policy.allows("northcentralus"));
        assert!(policy.allows("canadacentral"));
        // The region with the most deployable machine types on this
        // subscription, and the one a driver would otherwise pick.
        assert!(!policy.allows("westus2"));
    }

    #[test]
    fn a_region_matches_however_it_is_capitalised() {
        assert!(assigned().allows("NorthCentralUS"));
    }

    #[test]
    fn a_refusal_names_the_policy_and_what_it_permits() {
        let refusal = assigned().refusal("westus2");
        assert!(refusal.contains("Allowed resource deployment regions"));
        assert!(refusal.contains("northcentralus"));
    }

    #[test]
    fn a_subscription_with_no_such_assignment_may_deploy_anywhere() {
        // The absence of a policy is permission. Reading it as "no regions"
        // would leave an ordinary pay-as-you-go account with an empty
        // catalogue and no explanation.
        let policy = RegionPolicy::from_page(&page(include_str!(
            "../../fixtures/azure/policy_assignments_none.json"
        )));
        assert_eq!(policy, RegionPolicy::Unrestricted);
        assert!(policy.allows("westus2"));
        assert!(policy.regions().is_none());
    }

    #[test]
    fn two_assignments_both_bind() {
        let page = page(include_str!(
            "../../fixtures/azure/policy_assignments_two.json"
        ));
        let policy = RegionPolicy::from_page(&page);

        // Only the region both lists name survives.
        assert!(policy.allows("canadacentral"));
        assert!(!policy.allows("northcentralus"));
        assert!(!policy.allows("norwayeast"));
    }
}
