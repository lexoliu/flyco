//! The least privilege this driver can be given.
//!
//! The AWS wizard shows the user the policy they are about to attach to an
//! IAM user, and a policy that is not the truth is worse than no policy at
//! all: too narrow and the first provision fails with an
//! `UnauthorizedOperation` nobody can act on, too wide and flyco asked for
//! rights it never uses. So the list is derived from what the driver
//! actually sends, and a test proves it:
//! [`tests::the_ec2_actions_are_exactly_the_ones_the_driver_calls`] reads
//! this crate's own source and refuses to pass if a call site names an EC2
//! action this module does not.
//!
//! Everything outside EC2 is derived rather than listed. Each of those
//! services is reached through exactly one JSON-RPC target, and the target
//! constant already carries the action name after its `.` — so
//! [`OTHER_SERVICES`] pairs a signing name with the constant and the action
//! is read off it. A driver that starts calling a second Cost Explorer
//! operation changes the constant, and the policy changes with it.

use crate::aws::{costs, identity, image, pricing, quotas};

/// IAM's service prefix for EC2.
pub const EC2: &str = "ec2";

/// Every EC2 action the driver names in a request of its own.
///
/// In the order the driver reaches them: reading what an account can deploy,
/// finding or building the workspace network, launching, addressing, and
/// finally the lifecycle operations a session's machine goes through.
pub const EC2_ACTIONS: [&str; 21] = [
    "DescribeRegions",
    "DescribeInstanceTypes",
    "DescribeInstanceTypeOfferings",
    "DescribeInstances",
    "DescribeVpcs",
    "DescribeSubnets",
    "DescribeSecurityGroups",
    "CreateSecurityGroup",
    "AuthorizeSecurityGroupIngress",
    "DescribeImages",
    "RunInstances",
    "DescribeAddresses",
    "DescribeVolumes",
    "AllocateAddress",
    "AssociateAddress",
    "StartInstances",
    "StopInstances",
    "ModifyInstanceAttribute",
    "TerminateInstances",
    "ReleaseAddress",
    "DeleteVolume",
];

/// EC2 actions no call site names and every provision needs anyway.
///
/// `RunInstances` and `AllocateAddress` both carry `TagSpecification`, and
/// EC2 authorises the tags in one as a separate `ec2:CreateTags` — a launch
/// without it fails with `UnauthorizedOperation` naming an action the driver
/// never sent. Listed apart from [`EC2_ACTIONS`] so the test above can hold
/// that list to exactly the call sites, and so the reason a right is granted
/// is written down beside it.
pub const IMPLIED_EC2_ACTIONS: [&str; 1] = ["CreateTags"];

/// The services reached through one JSON-RPC target each, and that target.
///
/// The action is the segment after the target's final `.`, which is how the
/// AWS wire protocol spells it and how IAM spells it — so nothing here is
/// written twice.
pub const OTHER_SERVICES: [(&str, &str); 4] = [
    (costs::SERVICE, costs::TARGET),
    (pricing::SERVICE, pricing::GET_PRODUCTS_TARGET),
    (quotas::SERVICE, quotas::LIST_TARGET),
    (image::SERVICE, image::TARGET),
];

/// Every action the minimal policy grants, `service:Action`, sorted.
///
/// Sorted because the policy is a document a user reads and diffs, and an
/// order that moved with the source layout would make every regeneration
/// look like a change.
#[must_use]
pub fn actions() -> Vec<String> {
    let mut actions: Vec<String> = EC2_ACTIONS
        .iter()
        .chain(&IMPLIED_EC2_ACTIONS)
        .map(|action| format!("{EC2}:{action}"))
        .chain(
            OTHER_SERVICES
                .iter()
                .map(|(service, target)| format!("{service}:{}", operation(target))),
        )
        // `sts:GetCallerIdentity` is the one call made through the Query
        // protocol rather than JSON-RPC, because STS answers in XML.
        .chain(core::iter::once(format!(
            "{}:{}",
            identity::SERVICE,
            identity::ACTION
        )))
        .collect();
    actions.sort_unstable();
    actions
}

/// The operation half of an `X-Amz-Target`, which is `Service.Operation`.
fn operation(target: &str) -> &str {
    target.rsplit('.').next().unwrap_or(target)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{EC2_ACTIONS, IMPLIED_EC2_ACTIONS, actions, operation};

    /// The driver's own source, read at compile time.
    ///
    /// The only way to check a list of call sites against the call sites: a
    /// list maintained by hand beside code that changes is a list that goes
    /// stale, and going stale here means shipping a policy that does not let
    /// a session start.
    const DRIVER: &str = include_str!("mod.rs");

    /// The first argument of every EC2 call site, in every spelling the
    /// driver uses.
    ///
    /// Whitespace-stripped, because the calls are formatted across four
    /// lines as often as one. A call site whose region argument is spelled
    /// some other way is invisible to the scan below and its action then
    /// shows up as an `EC2_ACTIONS` entry nothing calls — which fails the
    /// test rather than silently under-reading the driver.
    const REGION_ARGUMENTS: [&str; 3] = ["region,", "&region,", "identity::REGION,"];

    /// Every action literal the driver hands to `ec2` or `ec2_call`.
    ///
    /// Both take the region first and the action second, as a `&'static str`
    /// literal, so a call site is `.ec2(` or `.ec2_call(`, a region
    /// argument, and then the action. Scanned rather than regex-matched
    /// because this crate has no regex dependency and does not need one.
    fn called() -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        for opener in [".ec2(", ".ec2_call("] {
            for site in DRIVER.split(opener).skip(1) {
                let Some(open) = site.find('"') else { continue };
                let Some(close) = site[open + 1..].find('"') else {
                    continue;
                };
                let between: String = site[..open].split_whitespace().collect();
                if !REGION_ARGUMENTS.contains(&between.as_str()) {
                    continue;
                }
                found.insert(site[open + 1..open + 1 + close].to_owned());
            }
        }
        found
    }

    #[test]
    fn the_ec2_actions_are_exactly_the_ones_the_driver_calls() {
        let listed: BTreeSet<String> = EC2_ACTIONS.iter().map(|&a| a.to_owned()).collect();
        assert_eq!(
            called(),
            listed,
            "EC2_ACTIONS must name every action the driver sends, and no others"
        );
    }

    #[test]
    fn an_implied_action_is_never_also_a_call_site() {
        for implied in IMPLIED_EC2_ACTIONS {
            assert!(
                !EC2_ACTIONS.contains(&implied),
                "{implied} is granted as an implied right and also listed as a call site"
            );
        }
    }

    #[test]
    fn the_policy_names_every_service_the_driver_signs_for() {
        let actions = actions();
        for expected in [
            "ec2:RunInstances",
            "ec2:CreateTags",
            "sts:GetCallerIdentity",
            "ce:GetCostAndUsage",
            "pricing:GetProducts",
            "servicequotas:ListServiceQuotas",
            "ssm:GetParameter",
        ] {
            assert!(actions.contains(&expected.to_owned()), "missing {expected}");
        }
        assert_eq!(
            actions.len(),
            EC2_ACTIONS.len() + IMPLIED_EC2_ACTIONS.len() + 5
        );
    }

    #[test]
    fn the_policy_is_sorted_and_free_of_duplicates() {
        let actions = actions();
        let unique: BTreeSet<&String> = actions.iter().collect();
        assert_eq!(unique.len(), actions.len());
        assert!(actions.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn an_operation_is_the_tail_of_its_target() {
        assert_eq!(
            operation("AWSInsightsIndexService.GetCostAndUsage"),
            "GetCostAndUsage"
        );
        assert_eq!(operation("GetProducts"), "GetProducts");
    }
}
