//! The two usage panels: cloud spend, and LLM account limits.
//!
//! The halves are shaped differently on purpose. Cloud spend is *authorative*
//! — every provider bills against a meter flyco can query — so
//! [`CloudUsageView`] reports amounts. LLM usage is *reactive*: neither
//! Anthropic nor `OpenAI` publishes a remaining-quota API, so
//! [`LlmUsageView`] reports only what flyco has observed happening — the cost
//! telemetry the harness emitted, and the rate limits it ran into.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::harness::HarnessKind;
use crate::id::{HarnessAccountId, ProviderAccountId};
use crate::machine::CloudProviderKind;
use crate::money::Usd;

/// One row of `GET /v1/usage/cloud`, covering one linked account over one
/// billing period.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CloudUsageView {
    /// Account the spend is billed to.
    pub account: ProviderAccountId,
    /// Provider that account belongs to.
    pub provider: CloudProviderKind,
    /// Start of the billing period, seconds since the Unix epoch.
    pub period_start_unix: u64,
    /// End of the billing period, seconds since the Unix epoch.
    pub period_end_unix: u64,
    /// Spend the provider has metered so far this period.
    pub spent: Usd,
    /// Promotional credit left, when the provider exposes a balance.
    pub remaining_credit: Option<Usd>,
}

/// One row of `GET /v1/usage/llm`, covering one linked harness account.
///
/// Every field is an observation rather than a quota. A panel built from
/// this says "you have spent this much and were limited at this time",
/// which is the honest claim; "you have N requests left" is not one flyco
/// can make.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LlmUsageView {
    /// Account the observations belong to.
    pub account: HarnessAccountId,
    /// Which harness that account drives.
    pub harness: HarnessKind,
    /// Label the account was linked under.
    pub label: String,
    /// Start of the window these observations cover, seconds since the Unix
    /// epoch.
    pub period_start_unix: u64,
    /// Cost the harness itself reported over that window, summed from its
    /// telemetry. Absent when the harness reported none.
    pub observed_cost: Option<Usd>,
    /// When this account last hit its usage limit, if it has.
    pub rate_limited_at_unix: Option<u64>,
    /// When that limit resets, when the harness named a time.
    pub resets_at_unix: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::LlmUsageView;
    use crate::harness::HarnessKind;
    use crate::id::HarnessAccountId;

    #[test]
    fn an_account_with_no_observations_yet_is_representable() {
        let view = LlmUsageView {
            account: HarnessAccountId::generate(),
            harness: HarnessKind::ClaudeCode,
            label: "personal".to_owned(),
            period_start_unix: 1_800_000_000,
            observed_cost: None,
            rate_limited_at_unix: None,
            resets_at_unix: None,
        };

        let json = serde_json::to_value(&view).expect("serialize");
        assert!(json["observed_cost"].is_null());
        let back: LlmUsageView = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, view);
    }
}
