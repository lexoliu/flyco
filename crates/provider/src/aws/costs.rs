//! What AWS's own meter says the account has been billed this month.
//!
//! Cost Explorer is the authority, for the reason Azure's Cost Management
//! is: a total flyco assembled from its own machine records would miss the
//! EBS, data-transfer and elastic-IP charges on the same invoice, and would
//! be a second opinion about a number the provider already publishes.
//!
//! # `UnblendedCost`, and an exclusive end
//!
//! `UnblendedCost` is what the invoice is made of, as opposed to
//! `AmortizedCost`, which spreads a reservation's up-front payment across
//! the months it covers — an account running spot instances has no
//! reservations, and the honest answer to "what have I been charged" is the
//! unblended one.
//!
//! Cost Explorer's `End` is **exclusive**, so a window that ends today
//! reports nothing about today. The window flyco asks for therefore ends
//! *tomorrow*, and the period it reports beside the amount is the one it
//! actually asked for: the first of the month to the instant of the query.

use flyco_core::CloudSpend;
use flyco_core::money::Usd;
use serde::{Deserialize, Serialize};

use crate::ProviderError;
use crate::datetime::{month_to_date, next_date};

/// Signing name of the Cost Explorer API.
pub const SERVICE: &str = "ce";

/// The region Cost Explorer is signed against; it has one endpoint.
pub const COST_EXPLORER_REGION: &str = "us-east-1";

/// Endpoint of the Cost Explorer API.
pub const COST_EXPLORER_ENDPOINT: &str = "https://ce.us-east-1.amazonaws.com/";

/// The JSON-RPC target that sums a period's cost.
pub const TARGET: &str = "AWSInsightsIndexService.GetCostAndUsage";

/// The metric an invoice is made of.
pub const METRIC: &str = "UnblendedCost";

/// The only currency flyco accounts in.
///
/// An account billed in anything else is refused rather than converted:
/// there is no rate to convert at, and a euro reported as a dollar is a
/// budget quietly told the wrong number.
pub const CURRENCY: &str = "USD";

/// Body of `GetCostAndUsage`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct GetCostAndUsage {
    /// The window, `Start` inclusive and `End` exclusive.
    pub time_period: TimePeriod,
    /// `MONTHLY`: one row for the whole period is the whole question.
    pub granularity: &'static str,
    /// `UnblendedCost`.
    pub metrics: Vec<&'static str>,
}

/// A billing window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct TimePeriod {
    /// First day covered, `YYYY-MM-DD`.
    pub start: String,
    /// First day *not* covered, `YYYY-MM-DD`.
    pub end: String,
}

impl GetCostAndUsage {
    /// This month's unblended cost, as a single row.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Malformed`] if `now_unix` is not a time.
    pub fn month_to_date(now_unix: u64) -> Result<Self, ProviderError> {
        let (start, _) = month_to_date(now_unix)?;
        Ok(Self {
            time_period: TimePeriod {
                start: crate::datetime::date(start)?,
                // Exclusive, so today's spend is only inside the window when
                // the window ends tomorrow.
                end: next_date(now_unix)?,
            },
            granularity: "MONTHLY",
            metrics: vec![METRIC],
        })
    }
}

/// What `GetCostAndUsage` answers.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct CostResult {
    /// One entry per granularity period; `MONTHLY` over one month is one.
    #[serde(default)]
    pub results_by_time: Vec<PeriodResult>,
}

/// One period's total.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PeriodResult {
    /// The metrics, keyed by name.
    #[serde(default)]
    pub total: std::collections::BTreeMap<String, MetricValue>,
}

/// One metric's amount and unit.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MetricValue {
    /// The amount, as a decimal string.
    #[serde(default)]
    pub amount: String,
    /// The currency it is stated in.
    #[serde(default)]
    pub unit: String,
}

impl CostResult {
    /// The metered total, in exact microdollars.
    ///
    /// A result with no periods is a real zero: AWS has metered nothing this
    /// month, which is what an account that has run nothing looks like.
    /// Several periods are summed, because a finer granularity would split
    /// the same total across them rather than change it.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Malformed`] if a period states no readable
    /// amount, or [`ProviderError::Rejected`] if the account is metered in a
    /// currency flyco does not account in.
    pub fn total(&self) -> Result<Usd, ProviderError> {
        let mut total = Usd::ZERO;
        for period in &self.results_by_time {
            let Some(metric) = period.total.get(METRIC) else {
                continue;
            };
            if !metric.unit.is_empty() && !metric.unit.eq_ignore_ascii_case(CURRENCY) {
                return Err(ProviderError::Rejected(format!(
                    "AWS meters this account in {}, and flyco accounts in {CURRENCY}",
                    metric.unit
                )));
            }
            let amount = metric.amount.parse::<f64>().map_err(|_| {
                ProviderError::Malformed("an AWS cost result stated an amount that is not a number")
            })?;
            total += micros(amount)?;
        }
        Ok(total)
    }
}

/// A decimal amount of dollars as exact microdollars.
///
/// The API quotes a decimal string a float can only approximate, so rounding
/// to the nearest microdollar at the boundary is what keeps every amount
/// downstream an exact integer.
fn micros(amount: f64) -> Result<Usd, ProviderError> {
    if !amount.is_finite() || amount < 0.0 {
        return Err(ProviderError::Malformed(
            "an AWS cost result stated a cost that is not an amount",
        ));
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a month's cloud spend in microdollars is non-negative and far inside u64"
    )]
    let micros = (amount * 1_000_000.0).round() as u64;
    Ok(Usd::from_micros(micros))
}

/// The spend a result reports over the window it covered.
///
/// # Errors
///
/// Returns [`ProviderError`] if the result is unreadable or the account is
/// metered in another currency.
pub fn spend_of(result: &CostResult, now_unix: u64) -> Result<CloudSpend, ProviderError> {
    let (period_start_unix, period_end_unix) = month_to_date(now_unix)?;
    Ok(CloudSpend {
        period_start_unix,
        period_end_unix,
        spent: result.total()?,
        // AWS exposes promotional credit only through the Billing console
        // and its own `ListCredits` on a payer account, which an account-
        // scoped access key on a member account cannot read. Reporting
        // `Some(0)` would tell a user their free credit is gone.
        remaining_credit: None,
    })
}

#[cfg(test)]
mod tests {
    use flyco_core::money::Usd;

    use super::{CostResult, GetCostAndUsage, spend_of};

    /// 2026-08-29T12:00:00Z.
    const QUERIED_AT: u64 = 1_788_004_800;

    /// 2026-08-01T00:00:00Z.
    const MONTH_START: u64 = 1_785_542_400;

    fn result(json: &str) -> CostResult {
        serde_json::from_str(json).expect("the cost fixture parses")
    }

    #[test]
    fn the_window_starts_on_the_first_and_ends_the_day_after_today() {
        let query = GetCostAndUsage::month_to_date(QUERIED_AT).expect("a query");
        assert_eq!(query.time_period.start, "2026-08-01");
        assert_eq!(
            query.time_period.end, "2026-08-30",
            "the end is exclusive, so today is only covered when it ends tomorrow"
        );
        assert_eq!(query.granularity, "MONTHLY");
        assert_eq!(query.metrics, vec!["UnblendedCost"]);
    }

    #[test]
    fn a_metered_month_reports_the_amount_over_the_window_it_covered() {
        let spend = spend_of(
            &result(include_str!("../../fixtures/aws/get_cost_and_usage.json")),
            QUERIED_AT,
        )
        .expect("a spend");

        assert_eq!(spend.spent, Usd::from_micros(41_270_000));
        assert_eq!(spend.period_start_unix, MONTH_START);
        assert_eq!(spend.period_end_unix, QUERIED_AT);
        assert_eq!(
            spend.remaining_credit, None,
            "a credit balance is a payer-account read, and inventing a zero would say it is spent"
        );
    }

    #[test]
    fn a_month_with_nothing_metered_is_a_real_zero() {
        let spend = spend_of(
            &result(include_str!(
                "../../fixtures/aws/get_cost_and_usage_empty.json"
            )),
            QUERIED_AT,
        )
        .expect("a spend");
        assert_eq!(spend.spent, Usd::ZERO);
    }

    #[test]
    fn an_account_billed_in_another_currency_is_refused() {
        let error = spend_of(
            &result(include_str!(
                "../../fixtures/aws/get_cost_and_usage_euros.json"
            )),
            QUERIED_AT,
        )
        .expect_err("euros must not be reported as dollars");
        assert!(error.to_string().contains("EUR"));
    }
}
