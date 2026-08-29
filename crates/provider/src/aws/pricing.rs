//! What an hour costs, from the two places AWS publishes it.
//!
//! Unlike Azure, AWS puts the on-demand price and the spot price behind
//! different services, so a priced catalog is two reads:
//!
//! * **On-demand** comes from the Price List Query API — `GetProducts` on
//!   `api.pricing.us-east-1.amazonaws.com`, filtered to one region's Linux,
//!   shared-tenancy, no-pre-installed-software, `Used` capacity meters. The
//!   filters are the whole difference between one price per instance type
//!   and eleven: the same type is also sold with Windows, with SQL Server
//!   pre-installed, on dedicated tenancy and as unused reserved capacity,
//!   and every one of those is a different number.
//! * **Spot** comes from EC2 itself, `DescribeSpotPriceHistory`, which
//!   prices per *availability zone*. The cheapest zone in the region is the
//!   one quoted, because a launch that names no zone is placed in whichever
//!   one has capacity and is billed at that zone's rate.
//!
//! Both are cached with a TTL for the reason Azure's retail prices are: a
//! spot price moves on its own schedule and a catalog read must not be a
//! page-walk every time.
//!
//! `clippy::future_not_send` is allowed across this module for the reason it
//! is in `azure::pricing`: `Send`-ness follows from the concrete transport,
//! and bounding `T: Sync` would forbid the recorded transport the whole
//! driver is tested against.
#![expect(clippy::future_not_send, reason = "see the module documentation")]

use flyco_core::money::Usd;
use serde::{Deserialize, Serialize};

use crate::ProviderError;
use crate::clock::MonotonicClock;
use crate::http::HttpTransport;

use super::ec2;
use super::sigv4::{AccessKey, Scope};

/// Signing name of the Price List Query API.
pub const SERVICE: &str = "pricing";

/// The region the Price List Query API is signed against.
///
/// It has three endpoints world-wide and prices every region from each; this
/// is the one that has existed longest and is enabled on every account.
pub const PRICING_REGION: &str = "us-east-1";

/// Endpoint of the Price List Query API.
pub const PRICING_ENDPOINT: &str = "https://api.pricing.us-east-1.amazonaws.com/";

/// The JSON-RPC target that lists priced products.
pub const GET_PRODUCTS_TARGET: &str = "AWSPriceListService.GetProducts";

/// The service whose products are EC2 instance-hours.
pub const EC2_SERVICE_CODE: &str = "AmazonEC2";

/// How long a region's prices are reused for.
pub const CACHE_TTL_SECONDS: u64 = 6 * 60 * 60;

/// The product description a Linux spot price is published under.
pub const LINUX_PRODUCT: &str = "Linux/UNIX";

/// Body of `GetProducts`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct GetProducts {
    /// `AmazonEC2`.
    pub service_code: &'static str,
    /// The filters that narrow eleven meters per instance type to one.
    pub filters: Vec<ProductFilter>,
    /// Page size, at the API's maximum.
    pub max_results: u32,
    /// Continuation token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_token: Option<String>,
}

/// One `GetProducts` filter.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ProductFilter {
    /// `TERM_MATCH`, the only type the API offers.
    #[serde(rename = "Type")]
    pub kind: &'static str,
    /// The product attribute to match.
    pub field: &'static str,
    /// The value it must equal.
    pub value: String,
}

impl ProductFilter {
    /// A filter matching one attribute exactly.
    fn term(field: &'static str, value: impl Into<String>) -> Self {
        Self {
            kind: "TERM_MATCH",
            field,
            value: value.into(),
        }
    }
}

impl GetProducts {
    /// The one query flyco makes: a region's ordinary Linux instance-hours.
    ///
    /// Every filter here removes a meter that prices the same instance type
    /// differently, and dropping any of them makes the answer ambiguous
    /// rather than merely larger.
    #[must_use]
    pub fn linux_on_demand(region: &str, next_token: Option<String>) -> Self {
        Self {
            service_code: EC2_SERVICE_CODE,
            filters: vec![
                ProductFilter::term("regionCode", region),
                ProductFilter::term("operatingSystem", "Linux"),
                // Shared tenancy: dedicated instances and dedicated hosts
                // are separately, and differently, priced.
                ProductFilter::term("tenancy", "Shared"),
                // No pre-installed database, which otherwise doubles the
                // price of the identical hardware.
                ProductFilter::term("preInstalledSw", "NA"),
                // `Used` rather than `AllocatedCapacityReservation`, which
                // prices capacity nothing is running on.
                ProductFilter::term("capacitystatus", "Used"),
                ProductFilter::term("marketoption", "OnDemand"),
            ],
            max_results: 100,
            next_token,
        }
    }
}

/// One page of priced products.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ProductPage {
    /// Each entry is a JSON *document* encoded as a string, which is how the
    /// API ships a product and its price terms together.
    #[serde(default)]
    pub price_list: Vec<String>,
    /// The next page, when there is one.
    #[serde(default)]
    pub next_token: Option<String>,
}

/// One priced product, as it appears inside a `PriceList` string.
#[derive(Debug, Clone, Deserialize)]
struct Product {
    product: ProductBody,
    terms: Terms,
}

/// The hardware half of a product.
#[derive(Debug, Clone, Deserialize)]
struct ProductBody {
    attributes: ProductAttributes,
}

/// The attributes that say which instance type a product prices.
#[derive(Debug, Clone, Deserialize)]
struct ProductAttributes {
    #[serde(rename = "instanceType", default)]
    instance_type: String,
}

/// The pricing half of a product.
#[derive(Debug, Clone, Deserialize)]
struct Terms {
    #[serde(rename = "OnDemand", default)]
    on_demand: serde_json::Map<String, serde_json::Value>,
}

/// The Linux on-demand price of one instance type in one region.
///
/// The terms are nested two maps deep under offer and rate codes that are
/// generated per product, so they are walked rather than indexed: there is
/// exactly one on-demand term with exactly one price dimension for these
/// filters, and reaching it by position would be a bug the first time AWS
/// publishes a second.
fn on_demand_hourly(product: &Product) -> Option<Usd> {
    let dimensions = product
        .terms
        .on_demand
        .values()
        .filter_map(|term| term.get("priceDimensions")?.as_object())
        .flat_map(serde_json::Map::values);

    for dimension in dimensions {
        // `Hrs` — the same product also carries a `Quantity` dimension on
        // some meters, which is not a rate per hour.
        if dimension.get("unit").and_then(serde_json::Value::as_str) != Some("Hrs") {
            continue;
        }
        let quoted = dimension
            .get("pricePerUnit")?
            .get("USD")
            .and_then(serde_json::Value::as_str)?;
        let price = quoted.parse::<f64>().ok()?;
        // A `0.0000000000` rate is a free-tier or placeholder meter, not a
        // price a machine can be run at.
        if price > 0.0 {
            return Some(micros(price));
        }
    }
    None
}

/// A decimal amount of dollars as exact microdollars.
fn micros(amount: f64) -> Usd {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "an hourly instance price in microdollars is small and non-negative"
    )]
    let micros = (amount * 1_000_000.0).round() as u64;
    Usd::from_micros(micros)
}

/// The prices of one instance type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MachinePrices {
    /// On-demand price per hour, when the region publishes one.
    pub on_demand: Option<Usd>,
    /// Spot price per hour in the cheapest zone, when the type is sold as
    /// spot at all.
    pub spot: Option<Usd>,
}

/// One region's prices, and when they stop being reused.
#[derive(Debug, Clone)]
struct CachedRegion {
    region: String,
    prices: Vec<(String, MachinePrices)>,
    expires_after: u64,
}

/// Prices per region, with a time-to-live.
#[derive(Debug, Clone, Default)]
pub struct PriceCatalog {
    cached: Option<CachedRegion>,
}

impl PriceCatalog {
    /// An empty catalog.
    #[must_use]
    pub const fn new() -> Self {
        Self { cached: None }
    }

    /// Every priced instance type in one region.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] if either pricing read is refused or
    /// answers with something this driver cannot read.
    ///
    /// # Panics
    ///
    /// Never in practice: the cache is filled immediately above the read
    /// that borrows it, and the borrow checker is what makes returning a
    /// reference to it need the `expect` at all.
    pub async fn region_prices<T, C>(
        &mut self,
        transport: &T,
        clock: &C,
        key: &AccessKey,
        region: &str,
        now_unix: u64,
    ) -> Result<&[(String, MachinePrices)], ProviderError>
    where
        T: HttpTransport,
        C: MonotonicClock,
    {
        let now = clock.elapsed_seconds();
        let fresh = self
            .cached
            .as_ref()
            .is_some_and(|cached| cached.region == region && now < cached.expires_after);

        if !fresh {
            let mut prices = read_on_demand(transport, key, region, now_unix).await?;
            apply_spot(transport, key, region, now_unix, &mut prices).await?;
            self.cached = Some(CachedRegion {
                region: region.to_owned(),
                prices,
                expires_after: now.saturating_add(CACHE_TTL_SECONDS),
            });
        }

        Ok(&self
            .cached
            .as_ref()
            .expect("the cache was just filled")
            .prices)
    }
}

/// Sends one signed JSON-RPC call to an AWS service that speaks it.
async fn json_rpc<T: HttpTransport, B: Serialize, R: serde::de::DeserializeOwned>(
    transport: &T,
    key: &AccessKey,
    endpoint: &str,
    scope: Scope<'_>,
    target: &str,
    body: &B,
    now_unix: u64,
) -> Result<R, ProviderError> {
    let request = super::json_rpc_request(endpoint, target, body)?;
    let response = transport
        .send(super::sigv4::sign(request, key, scope, now_unix)?)
        .await?;
    if !response.is_success() {
        return Err(super::json_refusal(&response));
    }
    response.json().map_err(Into::into)
}

/// Walks every page of a region's on-demand prices.
async fn read_on_demand<T: HttpTransport>(
    transport: &T,
    key: &AccessKey,
    region: &str,
    now_unix: u64,
) -> Result<Vec<(String, MachinePrices)>, ProviderError> {
    let mut folded: Vec<(String, MachinePrices)> = Vec::new();
    let mut next = None;

    loop {
        let page: ProductPage = json_rpc(
            transport,
            key,
            PRICING_ENDPOINT,
            Scope {
                region: PRICING_REGION,
                service: SERVICE,
            },
            GET_PRODUCTS_TARGET,
            &GetProducts::linux_on_demand(region, next),
            now_unix,
        )
        .await?;

        for encoded in &page.price_list {
            let Ok(product) = serde_json::from_str::<Product>(encoded) else {
                continue;
            };
            let name = product.product.attributes.instance_type.clone();
            if name.is_empty() {
                continue;
            }
            let Some(hourly) = on_demand_hourly(&product) else {
                continue;
            };
            if let Some(entry) = folded.iter_mut().find(|(seen, _)| *seen == name) {
                entry.1.on_demand = Some(hourly);
            } else {
                folded.push((
                    name,
                    MachinePrices {
                        on_demand: Some(hourly),
                        spot: None,
                    },
                ));
            }
        }

        next = page.next_token.filter(|token| !token.is_empty());
        if next.is_none() {
            return Ok(folded);
        }
    }
}

/// Folds the cheapest zone's spot price into each instance type it prices.
///
/// A spot price this driver has no on-demand price for is dropped rather
/// than added: an entry with a spot price and no on-demand one would be a
/// machine flyco could not quote at all if the spot request were refused.
async fn apply_spot<T: HttpTransport>(
    transport: &T,
    key: &AccessKey,
    region: &str,
    now_unix: u64,
    prices: &mut [(String, MachinePrices)],
) -> Result<(), ProviderError> {
    let mut next = None;
    loop {
        let body = ec2::DescribeSpotPriceHistory {
            product_description: vec![LINUX_PRODUCT.to_owned()],
            start_time: crate::datetime::rfc3339(now_unix)?,
            max_results: 1_000,
            next_token: next,
        };
        let response = super::send_ec2(
            transport,
            key,
            region,
            "DescribeSpotPriceHistory",
            &body,
            now_unix,
        )
        .await?;
        let page: ec2::DescribeSpotPriceHistoryResponse = ec2::decode(&response)?;

        for quote in &page.spot_price_history_set.item {
            let Ok(price) = quote.spot_price.parse::<f64>() else {
                continue;
            };
            if price <= 0.0 {
                continue;
            }
            let Some(entry) = prices
                .iter_mut()
                .find(|(name, _)| *name == quote.instance_type)
            else {
                continue;
            };
            let quoted = micros(price);
            // The cheapest zone, because a launch that names no zone lands
            // wherever there is capacity and is billed at that zone's rate.
            entry.1.spot = Some(entry.1.spot.map_or(quoted, |seen| seen.min(quoted)));
        }

        next = page.next_token.filter(|token| !token.is_empty());
        if next.is_none() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use flyco_core::money::Usd;

    use super::{GetProducts, ProductPage, on_demand_hourly};

    const PRODUCTS: &str = include_str!("../../fixtures/aws/get_products.json");

    #[test]
    fn the_product_query_narrows_to_one_meter_per_instance_type() {
        let query = GetProducts::linux_on_demand("us-west-2", None);
        let field = |name: &str| {
            query
                .filters
                .iter()
                .find(|filter| filter.field == name)
                .unwrap_or_else(|| panic!("the query filters on `{name}`"))
                .value
                .clone()
        };

        assert_eq!(field("regionCode"), "us-west-2");
        assert_eq!(field("operatingSystem"), "Linux");
        // Each of these removes a meter that prices the same hardware
        // differently; dropping one makes the answer ambiguous.
        assert_eq!(field("tenancy"), "Shared");
        assert_eq!(field("preInstalledSw"), "NA");
        assert_eq!(field("capacitystatus"), "Used");
        assert_eq!(field("marketoption"), "OnDemand");
    }

    #[test]
    fn an_hourly_rate_is_found_under_the_generated_offer_and_rate_codes() {
        let page: ProductPage = serde_json::from_str(PRODUCTS).expect("the fixture parses");
        let priced: Vec<(String, Option<Usd>)> = page
            .price_list
            .iter()
            .filter_map(|encoded| serde_json::from_str::<super::Product>(encoded).ok())
            .map(|product| {
                (
                    product.product.attributes.instance_type.clone(),
                    on_demand_hourly(&product),
                )
            })
            .collect();

        assert_eq!(
            priced
                .iter()
                .find(|(name, _)| name == "t3.small")
                .and_then(|(_, price)| *price),
            Some(Usd::from_micros(20_800))
        );
        assert_eq!(
            priced
                .iter()
                .find(|(name, _)| name == "mac2.metal")
                .and_then(|(_, price)| *price),
            Some(Usd::from_micros(650_000))
        );
    }

    #[test]
    fn a_zero_rate_is_not_a_price_a_machine_can_be_run_at() {
        let page: ProductPage = serde_json::from_str(PRODUCTS).expect("the fixture parses");
        let free = page
            .price_list
            .iter()
            .filter_map(|encoded| serde_json::from_str::<super::Product>(encoded).ok())
            .find(|product| product.product.attributes.instance_type == "t2.micro")
            .expect("the fixture holds the free-tier meter");
        assert_eq!(on_demand_hourly(&free), None);
    }
}
