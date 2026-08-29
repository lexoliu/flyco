//! Where this account is allowed to deploy at all.
//!
//! The third gate, and the one that is invisible from the other two. Every
//! AWS region introduced since 2019 is **disabled until the account opts
//! in**, and a disabled region's endpoint refuses a signed request outright
//! — `AuthFailure`, naming neither the region nor the reason. Nothing in the
//! instance-type or quota answers mentions it, for the same reason Azure's
//! SKU list says nothing about a subscription's deployment policy: it is a
//! property of the account rather than of the catalog.
//!
//! `DescribeRegions` with `AllRegions` is what makes the distinction
//! readable — a region the account has not enabled is listed, and says so —
//! and it is read from the account rather than assumed, because which
//! regions somebody has enabled is exactly the thing this code cannot know.

use super::ec2::{DescribeRegionsResponse, Region};

/// Opt-in status of a region that needs no opting into.
pub const NOT_REQUIRED: &str = "opt-in-not-required";

/// Opt-in status of a region the account has enabled.
pub const OPTED_IN: &str = "opted-in";

/// Which regions an account may deploy into.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RegionAccess {
    enabled: Vec<String>,
    disabled: Vec<String>,
}

impl RegionAccess {
    /// Folds a `DescribeRegions` answer into one decision.
    #[must_use]
    pub fn from_response(response: &DescribeRegionsResponse) -> Self {
        let mut access = Self::default();
        for region in &response.region_info.item {
            if Self::is_enabled(region) {
                access.enabled.push(region.region_name.clone());
            } else {
                access.disabled.push(region.region_name.clone());
            }
        }
        access
    }

    /// Whether one region's status means the account may use it.
    fn is_enabled(region: &Region) -> bool {
        matches!(region.opt_in_status.as_str(), NOT_REQUIRED | OPTED_IN)
    }

    /// Whether a deployment into `region` is permitted.
    ///
    /// A region the answer did not mention at all is refused rather than
    /// allowed: `AllRegions` lists every region there is, so silence about
    /// one means the caller named something that does not exist.
    #[must_use]
    pub fn allows(&self, region: &str) -> bool {
        self.enabled
            .iter()
            .any(|enabled| enabled.eq_ignore_ascii_case(region))
    }

    /// The regions the account may deploy into.
    #[must_use]
    pub fn enabled(&self) -> &[String] {
        &self.enabled
    }

    /// What to tell a user whose region was refused.
    #[must_use]
    pub fn refusal(&self, region: &str) -> String {
        if self
            .disabled
            .iter()
            .any(|disabled| disabled.eq_ignore_ascii_case(region))
        {
            return format!(
                "this account has not enabled {region}; opt into it in the AWS account settings"
            );
        }
        format!("AWS publishes no region called {region} to this account")
    }
}

#[cfg(test)]
mod tests {
    use super::RegionAccess;
    use crate::aws::ec2::{DescribeRegionsResponse, decode};
    use crate::http::HttpResponse;

    fn access() -> RegionAccess {
        let response: DescribeRegionsResponse = decode(&HttpResponse::new(
            200,
            include_bytes!("../../fixtures/aws/describe_regions.xml").to_vec(),
        ))
        .expect("the region fixture parses");
        RegionAccess::from_response(&response)
    }

    #[test]
    fn a_region_that_needs_no_opting_into_is_available() {
        assert!(access().allows("us-west-2"));
        assert!(access().allows("eu-west-1"));
    }

    #[test]
    fn a_region_the_account_opted_into_is_available_too() {
        assert!(access().allows("eu-south-2"));
    }

    #[test]
    fn a_region_the_account_never_enabled_is_refused_by_name() {
        let access = access();
        assert!(!access.allows("ap-east-1"));

        let refusal = access.refusal("ap-east-1");
        assert!(
            refusal.contains("has not enabled"),
            "the refusal names the problem the user can fix: {refusal}"
        );
    }

    #[test]
    fn a_region_that_does_not_exist_is_refused_rather_than_attempted() {
        let access = access();
        assert!(!access.allows("mars-north-1"));
        assert!(
            access
                .refusal("mars-north-1")
                .contains("publishes no region")
        );
    }

    #[test]
    fn a_region_matches_however_it_is_capitalised() {
        assert!(access().allows("US-West-2"));
    }
}
