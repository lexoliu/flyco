//! What Azure's own meter says the subscription has been billed.
//!
//! Cost Management is the only authority on this — a sum flyco computed
//! from its own machine records would be a second opinion, and it would be
//! wrong: it knows nothing about the storage, egress and support charges on
//! the same invoice. So the panel asks Azure and reports the number it gets
//! back.
//!
//! # `ActualCost`, month to date
//!
//! `type: "ActualCost"` is what an invoice is made of, as opposed to
//! `AmortizedCost`, which spreads a reservation's up-front payment across
//! the months it covers — a subscription running spot VMs has no
//! reservations, and the honest answer to "what have I been charged" is the
//! actual one.
//!
//! `timeframe: "MonthToDate"` rather than `BillingMonthToDate`: the latter
//! is a billing-account concept and is refused at subscription scope, which
//! is the only scope a flyco service principal is ever granted. For the
//! subscription types flyco can provision into the two are the same window,
//! and reporting it as [`CloudSpend`] means the period flyco *states* is
//! exactly the period Azure summed rather than a second guess beside it.
//!
//! # Columns are read by name, never by position
//!
//! A query result is `columns` plus positional `rows`, and the column set
//! moves with the request: the cost column is named after the metric that
//! was asked for, `Currency` is appended without being asked for at all,
//! and a granularity adds a `UsageDate`. Reading row index 0 as "the cost"
//! is a bug waiting for the day the shape changes, so the index comes from
//! the column list every time.

use flyco_core::CloudSpend;
use flyco_core::money::Usd;
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, Time};

use crate::ProviderError;

/// The only currency flyco accounts in.
///
/// A subscription billed in anything else is refused rather than converted:
/// there is no rate to convert at, and a euro reported as a dollar is a
/// budget quietly told the wrong number.
pub const CURRENCY: &str = "USD";

/// Alias the aggregation is requested under.
///
/// Azure names the response column after the *metric*, not this key, so the
/// two are separate constants and the response is matched against
/// [`COST_METRIC`].
const TOTAL_COST_ALIAS: &str = "totalCost";

/// The metric an `ActualCost` query sums, and therefore the name of the
/// column it comes back in.
pub const COST_METRIC: &str = "Cost";

/// The column Azure appends to every query result.
pub const CURRENCY_COLUMN: &str = "Currency";

/// Body of a Cost Management usage query.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CostQuery {
    /// `ActualCost` — what the invoice will say.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// `MonthToDate`.
    pub timeframe: &'static str,
    /// What to sum, and how finely.
    pub dataset: CostDataset,
}

/// The dataset half of a cost query.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CostDataset {
    /// `None`: one row for the whole period is the whole question.
    pub granularity: &'static str,
    /// Aggregations, keyed by an alias of the caller's choosing.
    pub aggregation: std::collections::BTreeMap<&'static str, CostAggregation>,
}

/// One aggregation.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CostAggregation {
    /// The metric to aggregate.
    pub name: &'static str,
    /// `Sum`.
    pub function: &'static str,
}

impl CostQuery {
    /// The one query flyco makes: this month's actual cost, as a single row.
    #[must_use]
    pub fn month_to_date() -> Self {
        Self {
            kind: "ActualCost",
            timeframe: "MonthToDate",
            dataset: CostDataset {
                granularity: "None",
                aggregation: core::iter::once((
                    TOTAL_COST_ALIAS,
                    CostAggregation {
                        name: COST_METRIC,
                        function: "Sum",
                    },
                ))
                .collect(),
            },
        }
    }
}

/// A Cost Management query result.
#[derive(Debug, Clone, Deserialize)]
pub struct CostResult {
    /// The result itself.
    #[serde(default)]
    pub properties: CostProperties,
}

/// The columns and rows of a query result.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CostProperties {
    /// What each position in a row means.
    #[serde(default)]
    pub columns: Vec<CostColumn>,
    /// The rows, positional against `columns`.
    #[serde(default)]
    pub rows: Vec<Vec<serde_json::Value>>,
}

/// One column of a query result.
#[derive(Debug, Clone, Deserialize)]
pub struct CostColumn {
    /// Column name, e.g. `Cost` or `Currency`.
    #[serde(default)]
    pub name: String,
}

impl CostProperties {
    /// The position of a named column, if the result has one.
    fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.name == name)
    }

    /// The metered total, in exact microdollars.
    ///
    /// A result with no rows is a real zero: Azure has metered nothing this
    /// month, which is what a subscription that has run nothing looks like.
    /// That case is answered before the columns are consulted, because a
    /// `204 No Content` says the same thing and names no columns at all.
    /// Several rows are summed, because a future grouping would split the
    /// same total across them rather than change it.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Malformed`] if a result that has rows names
    /// no cost column, or [`ProviderError::Rejected`] if the subscription
    /// is metered in a currency flyco does not account in.
    pub fn total(&self) -> Result<Usd, ProviderError> {
        if self.rows.is_empty() {
            return Ok(Usd::ZERO);
        }

        let cost = self.column(COST_METRIC).ok_or(ProviderError::Malformed(
            "an Azure cost query answered without a cost column",
        ))?;
        let currency = self.column(CURRENCY_COLUMN);

        let mut total = Usd::ZERO;
        for row in &self.rows {
            if let Some(currency) = currency
                .and_then(|at| row.get(at))
                .and_then(|value| value.as_str())
                && !currency.eq_ignore_ascii_case(CURRENCY)
            {
                return Err(ProviderError::Rejected(format!(
                    "Azure meters this subscription in {currency}, and flyco accounts in {CURRENCY}"
                )));
            }

            let amount = row.get(cost).and_then(serde_json::Value::as_f64).ok_or(
                ProviderError::Malformed(
                    "an Azure cost query answered with a cost that is not a number",
                ),
            )?;
            total += micros(amount)?;
        }
        Ok(total)
    }
}

/// A decimal amount of dollars as exact microdollars.
///
/// The API quotes a decimal a float can only approximate, so rounding to
/// the nearest microdollar at the boundary is what keeps every amount
/// downstream an exact integer — the same conversion the retail-price
/// catalog does.
fn micros(amount: f64) -> Result<Usd, ProviderError> {
    if !amount.is_finite() || amount < 0.0 {
        return Err(ProviderError::Malformed(
            "an Azure cost query answered with a cost that is not an amount",
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

/// The window `MonthToDate` covers, given the instant the query was made.
///
/// Azure's month-to-date runs from midnight UTC on the first of the current
/// calendar month up to now, so the period flyco reports beside the amount
/// is the period Azure summed rather than an approximation of it.
///
/// # Errors
///
/// Returns [`ProviderError::Malformed`] if `now_unix` is not a time.
pub fn month_to_date(now_unix: u64) -> Result<(u64, u64), ProviderError> {
    let now = i64::try_from(now_unix)
        .ok()
        .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok())
        .ok_or(ProviderError::Malformed(
            "a cost query was made at an instant outside the representable range",
        ))?;

    let start = now
        .replace_day(1)
        .map_err(|_| ProviderError::Malformed("every month has a first day"))?
        .replace_time(Time::MIDNIGHT)
        .unix_timestamp();

    let start = u64::try_from(start)
        .map_err(|_| ProviderError::Malformed("a billing month began before the Unix epoch"))?;
    Ok((start, now_unix))
}

/// The spend a query result reports over the window it covered.
///
/// # Errors
///
/// Returns [`ProviderError`] if the result is unreadable or the
/// subscription is metered in another currency.
pub fn spend_of(properties: &CostProperties, now_unix: u64) -> Result<CloudSpend, ProviderError> {
    let (period_start_unix, period_end_unix) = month_to_date(now_unix)?;
    Ok(CloudSpend {
        period_start_unix,
        period_end_unix,
        spent: properties.total()?,
        // Azure exposes a credit balance only through the EA-only
        // `Microsoft.Consumption/balances`, which a service principal on an
        // ordinary subscription cannot read. Reporting `Some(0)` would tell
        // a user their free credit is gone.
        remaining_credit: None,
    })
}
