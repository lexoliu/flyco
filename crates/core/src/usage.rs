//! The two usage panels: cloud spend, and LLM account limits.
//!
//! The halves are shaped differently on purpose. Cloud spend is *authorative*
//! — every provider bills against a meter flyco can query — so
//! [`CloudUsageView`] reports amounts. LLM usage is *reactive*: neither
//! Anthropic nor `OpenAI` publishes a remaining-quota API, so
//! [`LlmUsageView`] reports only what flyco has observed happening — the cost
//! telemetry the harness emitted, and the rate limits it ran into.
//!
//! That difference reaches all the way down. A cloud figure is *read* from
//! the provider when the panel is asked for. An LLM figure can only be
//! *accumulated*: [`HarnessObservation`] is one thing a session's daemon saw
//! happen, posted as it happens, and the panel is the sum of them.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::harness::HarnessKind;
use crate::id::{HarnessAccountId, ProviderAccountId};
use crate::machine::CloudProviderKind;
use crate::money::Usd;

/// How far back [`LlmUsageView`] sums what it observed.
///
/// Reactive reporting has no billing period to key on: neither vendor
/// publishes one, and the plan limits that actually bind are rolling
/// windows the vendor does not name either. So the window is flyco's own
/// and it is stated rather than implied — a rolling day, long enough to
/// cover a working session and short enough that yesterday's spend is not
/// presented as today's.
pub const OBSERVATION_WINDOW_SECONDS: u64 = 24 * 60 * 60;

/// What a provider's own meter reports for one subscription over one
/// billing period.
///
/// The account-neutral half of [`CloudUsageView`]: a driver knows what the
/// subscription it holds credentials for was billed, and the control plane
/// knows which linked account that subscription is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloudSpend {
    /// Start of the billing period, seconds since the Unix epoch.
    pub period_start_unix: u64,
    /// End of the period the amount covers — the moment it was read —
    /// seconds since the Unix epoch.
    pub period_end_unix: u64,
    /// Spend the provider has metered over exactly that window.
    pub spent: Usd,
    /// Promotional credit left, when the provider exposes a balance.
    pub remaining_credit: Option<Usd>,
}

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

impl CloudUsageView {
    /// Attributes one provider's metered spend to the account it was read
    /// through.
    #[must_use]
    pub const fn of(
        account: ProviderAccountId,
        provider: CloudProviderKind,
        spend: CloudSpend,
    ) -> Self {
        Self {
            account,
            provider,
            period_start_unix: spend.period_start_unix,
            period_end_unix: spend.period_end_unix,
            spent: spend.spent,
            remaining_credit: spend.remaining_credit,
        }
    }
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

/// One thing a session's daemon saw happen to the harness account driving
/// it, as `POST /v1/sessions/{id}/harness-observations` records it.
///
/// The daemon is the only place both facts exist: it holds the turn's
/// [`UsageReport`](crate::harness::UsageReport) and it is what receives
/// [`HarnessEvent::UsageLimited`](crate::harness::HarnessEvent::UsageLimited).
/// Neither can be asked for after the fact, which is why they are posted as
/// they happen rather than polled.
///
/// An observation that reports neither is nothing to record, and the
/// control plane refuses it rather than storing a row that says nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HarnessObservation {
    /// Cost the harness reported for the turn that just finished, when it
    /// reported one.
    #[serde(default)]
    pub observed_cost: Option<Usd>,
    /// Present when this observation is the account hitting its limit.
    #[serde(default)]
    pub rate_limit: Option<RateLimitObservation>,
}

impl HarnessObservation {
    /// Whether this observation records anything at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.observed_cost.is_none() && self.rate_limit.is_none()
    }
}

/// The account hit its usage limit.
///
/// A struct rather than a bare timestamp field beside the cost, so "when it
/// resets" cannot be set on an observation that is not a rate limit: the
/// reset time only exists inside the event that has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RateLimitObservation {
    /// When the harness said the limit resets, when it named a time.
    #[serde(default)]
    pub resets_at_unix: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::{
        CloudSpend, CloudUsageView, HarnessObservation, LlmUsageView, RateLimitObservation,
    };
    use crate::harness::HarnessKind;
    use crate::id::{HarnessAccountId, ProviderAccountId};
    use crate::machine::CloudProviderKind;
    use crate::money::Usd;

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

    #[test]
    fn an_observation_that_reports_nothing_is_recognisably_empty() {
        let nothing = HarnessObservation {
            observed_cost: None,
            rate_limit: None,
        };
        assert!(nothing.is_empty());

        assert!(
            !HarnessObservation {
                observed_cost: None,
                rate_limit: Some(RateLimitObservation {
                    resets_at_unix: None
                }),
            }
            .is_empty(),
            "a limit with no stated reset time is still an observation"
        );
    }

    #[test]
    fn a_reset_time_cannot_exist_without_the_limit_it_belongs_to() {
        // The shape is the guarantee: there is no field to put a reset time
        // in unless the observation is a rate limit.
        let json = serde_json::json!({ "observed_cost": 1_500_000 });
        let observation: HarnessObservation = serde_json::from_value(json).expect("deserialize");
        assert_eq!(observation.observed_cost, Some(Usd::from_micros(1_500_000)));
        assert!(observation.rate_limit.is_none());
    }

    #[test]
    fn metered_spend_carries_the_window_it_was_read_over() {
        let account = ProviderAccountId::generate();
        let view = CloudUsageView::of(
            account,
            CloudProviderKind::Azure,
            CloudSpend {
                period_start_unix: 1_798_761_600,
                period_end_unix: 1_800_000_000,
                spent: Usd::from_cents(1_234),
                remaining_credit: None,
            },
        );

        assert_eq!(view.account, account);
        assert_eq!(view.spent, Usd::from_cents(1_234));
        assert_eq!(view.period_end_unix, 1_800_000_000);
    }
}
