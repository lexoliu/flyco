//! What an hour costs, from the places AWS publishes it.
//!
//! Unlike Azure, AWS puts the on-demand price and the spot price behind
//! different services, so a priced catalog is several reads:
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
//! * **A container's rates** come from the same Price List under a different
//!   offer code, `AmazonECS`: Fargate is sold per vCPU-hour and per
//!   GiB-hour, with a second pair for Graviton and a third meter for the
//!   ephemeral disk beyond a task's free allowance. See [`FargatePrices`].
//!
//! All three are cached with a TTL for the reason Azure's retail prices are:
//! a spot price moves on its own schedule and a catalog read must not be a
//! page-walk every time.
//!
//! **Fargate Spot is the one price AWS publishes nowhere.** There is no Spot
//! meter under `AmazonECS`, no Fargate offer code beside it, and
//! `DescribeSpotPriceHistory` prices EC2 instance types rather than tasks;
//! [aws.amazon.com/fargate/pricing](https://aws.amazon.com/fargate/pricing/)
//! states only that the rate is "set by AWS Fargate and adjust[s] gradually"
//! at up to 70% off. So this module publishes no spot rate for a container
//! and [`fargate`](super::fargate) quotes the on-demand one — see
//! [`fargate::catalog`](super::fargate::catalog) for why that is the
//! quotable answer rather than a discount flyco invented.
//!
//! `clippy::future_not_send` is allowed across this module for the reason it
//! is in `azure::pricing`: `Send`-ness follows from the concrete transport,
//! and bounding `T: Sync` would forbid the recorded transport the whole
//! driver is tested against.
#![expect(clippy::future_not_send, reason = "see the module documentation")]

use flyco_core::machine::CpuArchitecture;
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

/// The service whose products are ECS's own meters, Fargate's included.
///
/// Fargate has no offer code of its own: a task-hour is billed under
/// `AmazonECS`, per vCPU and per GiB, which is why a container's price is
/// read from here rather than from [`EC2_SERVICE_CODE`].
pub const ECS_SERVICE_CODE: &str = "AmazonECS";

/// The product family both Fargate rate meters sit in.
///
/// Named because `AmazonECS` also publishes `Compute Metering` — the
/// zero-rated meters that account for tasks on EC2 capacity — under names
/// that are otherwise indistinguishable from Fargate's.
pub const COMPUTE_FAMILY: &str = "Compute";

/// The value AWS puts in `cpuArchitecture` on the Graviton Fargate meters.
///
/// The x86-64 meters carry no such attribute at all, so this is the whole
/// discriminator between the two price pairs.
pub const ARM_ARCHITECTURE: &str = "ARM";

/// The unit an instance-hour is published in.
pub const INSTANCE_HOUR_UNIT: &str = "Hrs";

/// The unit Fargate's vCPU and memory rates are published in.
///
/// Lowercase, and different from [`INSTANCE_HOUR_UNIT`] on the identical
/// question, which is why the unit is a parameter rather than a constant
/// inside the reader.
pub const FARGATE_HOUR_UNIT: &str = "hours";

/// The unit Fargate's ephemeral-storage rate is published in.
pub const FARGATE_STORAGE_UNIT: &str = "GB-Hours";

/// Hours in the month AWS prices monthly capacity by.
pub const HOURS_PER_MONTH: f64 = 730.0;

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

    /// The gp3 persistent-volume capacity meter for one region.
    #[must_use]
    pub fn gp3_storage(region: &str, next_token: Option<String>) -> Self {
        Self {
            service_code: EC2_SERVICE_CODE,
            filters: vec![
                ProductFilter::term("regionCode", region),
                ProductFilter::term("productFamily", "Storage"),
                ProductFilter::term("volumeApiName", "gp3"),
            ],
            max_results: 100,
            next_token,
        }
    }

    /// One region's Fargate meters, narrowed by the attribute that says what
    /// each of them counts.
    ///
    /// Three queries rather than one, because the only filters that separate
    /// Fargate's rates from the rest of `AmazonECS` are the ones naming the
    /// resource being metered: `cputype`, `memorytype` and `storagetype`. A
    /// single `productFamily` query would walk several hundred ECS Managed
    /// Instances products to find four.
    fn fargate(
        region: &str,
        resource: &'static str,
        value: &'static str,
        next_token: Option<String>,
    ) -> Self {
        Self {
            service_code: ECS_SERVICE_CODE,
            filters: vec![
                ProductFilter::term("regionCode", region),
                ProductFilter::term("productFamily", COMPUTE_FAMILY),
                ProductFilter::term(resource, value),
            ],
            max_results: 100,
            next_token,
        }
    }

    /// The per-vCPU-hour meters, one per architecture and one for Windows.
    #[must_use]
    pub fn fargate_vcpu(region: &str, next_token: Option<String>) -> Self {
        Self::fargate(region, "cputype", "perCPU", next_token)
    }

    /// The per-GiB-hour memory meters, likewise.
    #[must_use]
    pub fn fargate_memory(region: &str, next_token: Option<String>) -> Self {
        Self::fargate(region, "memorytype", "perGB", next_token)
    }

    /// The ephemeral-storage meter, of which there is one: the rate for the
    /// disk beyond a task's free allowance does not vary by architecture.
    #[must_use]
    pub fn fargate_storage(region: &str, next_token: Option<String>) -> Self {
        Self::fargate(region, "storagetype", "default", next_token)
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
    #[serde(rename = "volumeApiName", default)]
    volume_api_name: String,
    /// `ARM` on a Fargate meter that prices Graviton, absent on the x86-64
    /// one. Absence is how the two are told apart: AWS states the
    /// architecture only on the meter that is not the default.
    #[serde(rename = "cpuArchitecture", default)]
    cpu_architecture: String,
    /// `Windows` on the Windows-container meters, absent on the Linux ones.
    #[serde(rename = "operatingSystem", default)]
    operating_system: String,
}

/// The pricing half of a product.
#[derive(Debug, Clone, Deserialize)]
struct Terms {
    #[serde(rename = "OnDemand", default)]
    on_demand: serde_json::Map<String, serde_json::Value>,
}

/// The dollar amount of the one on-demand dimension quoted in `unit`.
///
/// The terms are nested two maps deep under offer and rate codes that are
/// generated per product, so they are walked rather than indexed: there is
/// exactly one on-demand term with exactly one price dimension in each of
/// these units, and reaching it by position would be a bug the first time
/// AWS publishes a second. The unit is part of the question because the same
/// product routinely carries more than one dimension — a `Quantity` beside
/// an `Hrs`, and every meter in this crate is a rate rather than a count.
///
/// A `0.0000000000` rate is a free-tier or placeholder meter rather than a
/// price anything can be run at, so it reads as no price at all.
fn rate_in(product: &Product, unit: &str) -> Option<f64> {
    let dimensions = product
        .terms
        .on_demand
        .values()
        .filter_map(|term| term.get("priceDimensions")?.as_object())
        .flat_map(serde_json::Map::values);

    for dimension in dimensions {
        if dimension.get("unit").and_then(serde_json::Value::as_str) != Some(unit) {
            continue;
        }
        let quoted = dimension
            .get("pricePerUnit")?
            .get("USD")
            .and_then(serde_json::Value::as_str)?;
        let price = quoted.parse::<f64>().ok()?;
        if price > 0.0 {
            return Some(price);
        }
    }
    None
}

/// The Linux on-demand price of one instance type in one region.
fn on_demand_hourly(product: &Product) -> Option<Usd> {
    rate_in(product, INSTANCE_HOUR_UNIT).map(micros)
}

fn storage_gib_hourly(product: &Product) -> Option<Usd> {
    if product.product.attributes.volume_api_name != "gp3" {
        return None;
    }
    // EBS capacity is published per GiB-month, and a month here is AWS's own
    // 730 hours rather than the calendar's.
    rate_in(product, "GB-Mo").map(|monthly| micros(monthly / HOURS_PER_MONTH))
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

/// What Fargate charges for one architecture's capacity, per hour.
///
/// Per vCPU and per GiB rather than per machine, because that is how the
/// service sells it: a size is a point flyco picks on the two meters, and
/// its hourly price is the pair multiplied out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FargateRates {
    /// One vCPU for one hour.
    pub vcpu_hourly: Usd,
    /// One GiB of memory for one hour.
    pub memory_gib_hourly: Usd,
}

/// One region's Fargate rates, per architecture.
///
/// Each half is optional because a region that publishes no meter for an
/// architecture is a region that does not sell it — Graviton Fargate reached
/// the regions at its own pace — and a container flyco cannot price is one it
/// must not offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FargatePrices {
    /// The x86-64 rates.
    pub x86_64: Option<FargateRates>,
    /// The Graviton rates.
    pub arm64: Option<FargateRates>,
    /// One GiB of ephemeral disk beyond the free allowance, for one hour.
    pub ephemeral_gib_hourly: Option<Usd>,
}

impl FargatePrices {
    /// The rates for one architecture, when the region sells it.
    #[must_use]
    pub const fn rates(&self, architecture: CpuArchitecture) -> Option<FargateRates> {
        match architecture {
            CpuArchitecture::X8664 => self.x86_64,
            CpuArchitecture::Arm64 => self.arm64,
        }
    }
}

/// One region's prices, and when they stop being reused.
#[derive(Debug, Clone)]
struct CachedRegion {
    region: String,
    prices: Vec<(String, MachinePrices)>,
    storage_gib_hourly: Usd,
    expires_after: u64,
}

/// One region's Fargate rates, and when they stop being reused.
#[derive(Debug, Clone)]
struct CachedFargate {
    region: String,
    prices: FargatePrices,
    expires_after: u64,
}

/// Prices per region, with a time-to-live.
#[derive(Debug, Clone, Default)]
pub struct PriceCatalog {
    cached: Option<CachedRegion>,
    fargate: Option<CachedFargate>,
}

impl PriceCatalog {
    /// An empty catalog.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cached: None,
            fargate: None,
        }
    }

    /// One region's Fargate rates, cached for [`CACHE_TTL_SECONDS`].
    ///
    /// Cached separately from the instance prices because they are read for
    /// a different reason: a region's container entries need these three
    /// meters and none of the instance page-walk, and a driver that read
    /// both would spend eleven calls to publish four sizes.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] if the Price List refuses a read or answers
    /// with something this driver cannot read.
    pub async fn fargate_prices<T, C>(
        &mut self,
        transport: &T,
        clock: &C,
        key: &AccessKey,
        region: &str,
        now_unix: u64,
    ) -> Result<FargatePrices, ProviderError>
    where
        T: HttpTransport,
        C: MonotonicClock,
    {
        let now = clock.elapsed_seconds();
        if let Some(cached) = &self.fargate
            && cached.region == region
            && now < cached.expires_after
        {
            return Ok(cached.prices);
        }

        let prices = read_fargate(transport, key, region, now_unix).await?;
        self.fargate = Some(CachedFargate {
            region: region.to_owned(),
            prices,
            expires_after: now.saturating_add(CACHE_TTL_SECONDS),
        });
        Ok(prices)
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
    ) -> Result<(&[(String, MachinePrices)], Usd), ProviderError>
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
            let storage_gib_hourly = read_storage(transport, key, region, now_unix).await?;
            apply_spot(transport, key, region, now_unix, &mut prices).await?;
            self.cached = Some(CachedRegion {
                region: region.to_owned(),
                prices,
                storage_gib_hourly,
                expires_after: now.saturating_add(CACHE_TTL_SECONDS),
            });
        }

        let cached = self.cached.as_ref().expect("the cache was just filled");
        Ok((&cached.prices, cached.storage_gib_hourly))
    }
}

/// One region's three Fargate meters, as the rates a size is priced from.
///
/// A pair is kept only when both halves of it were published: half of an
/// hour's cost is not a price flyco may quote, and an architecture with no
/// meter in this region is one the region does not sell.
async fn read_fargate<T: HttpTransport>(
    transport: &T,
    key: &AccessKey,
    region: &str,
    now_unix: u64,
) -> Result<FargatePrices, ProviderError> {
    let vcpus =
        read_architecture_rates(transport, key, region, now_unix, GetProducts::fargate_vcpu)
            .await?;
    let memory = read_architecture_rates(
        transport,
        key,
        region,
        now_unix,
        GetProducts::fargate_memory,
    )
    .await?;
    let ephemeral_gib_hourly = read_ephemeral_rate(transport, key, region, now_unix).await?;

    let rate_of = |rates: &[(CpuArchitecture, Usd)], architecture: CpuArchitecture| {
        rates
            .iter()
            .find(|(published, _)| *published == architecture)
            .map(|(_, rate)| *rate)
    };
    let pair = |architecture: CpuArchitecture| {
        Some(FargateRates {
            vcpu_hourly: rate_of(&vcpus, architecture)?,
            memory_gib_hourly: rate_of(&memory, architecture)?,
        })
    };

    Ok(FargatePrices {
        x86_64: pair(CpuArchitecture::X8664),
        arm64: pair(CpuArchitecture::Arm64),
        ephemeral_gib_hourly,
    })
}

/// Every architecture one Fargate query prices, with its hourly rate.
///
/// The Windows meters are dropped rather than folded in: flyco boots Ubuntu,
/// and they are the only products in these queries that name an operating
/// system at all, so absence is the test. What is left is at most one rate
/// per architecture, and the ARM one is the only one AWS labels — an
/// unlabelled Linux meter is the x86-64 rate.
async fn read_architecture_rates<T: HttpTransport>(
    transport: &T,
    key: &AccessKey,
    region: &str,
    now_unix: u64,
    query: fn(&str, Option<String>) -> GetProducts,
) -> Result<Vec<(CpuArchitecture, Usd)>, ProviderError> {
    let mut rates: Vec<(CpuArchitecture, Usd)> = Vec::new();
    let mut next = None;

    loop {
        let page = read_products(transport, key, region, now_unix, query, next).await?;
        for product in products_of(&page) {
            if !product.product.attributes.operating_system.is_empty() {
                continue;
            }
            let Some(rate) = rate_in(&product, FARGATE_HOUR_UNIT) else {
                continue;
            };
            let architecture = if product.product.attributes.cpu_architecture == ARM_ARCHITECTURE {
                CpuArchitecture::Arm64
            } else {
                CpuArchitecture::X8664
            };
            if !rates.iter().any(|(seen, _)| *seen == architecture) {
                rates.push((architecture, micros(rate)));
            }
        }
        next = page.next_token.filter(|token| !token.is_empty());
        if next.is_none() {
            return Ok(rates);
        }
    }
}

/// The one ephemeral-storage rate a region publishes, when it publishes one.
///
/// Unlike the capacity meters it carries no architecture: the disk beyond a
/// task's free allowance costs the same on Graviton as on x86-64.
async fn read_ephemeral_rate<T: HttpTransport>(
    transport: &T,
    key: &AccessKey,
    region: &str,
    now_unix: u64,
) -> Result<Option<Usd>, ProviderError> {
    let mut next = None;
    loop {
        let page = read_products(
            transport,
            key,
            region,
            now_unix,
            GetProducts::fargate_storage,
            next,
        )
        .await?;
        for product in products_of(&page) {
            if let Some(rate) = rate_in(&product, FARGATE_STORAGE_UNIT) {
                return Ok(Some(micros(rate)));
            }
        }
        next = page.next_token.filter(|token| !token.is_empty());
        if next.is_none() {
            return Ok(None);
        }
    }
}

/// One page of a Price List query.
async fn read_products<T: HttpTransport>(
    transport: &T,
    key: &AccessKey,
    region: &str,
    now_unix: u64,
    query: fn(&str, Option<String>) -> GetProducts,
    next_token: Option<String>,
) -> Result<ProductPage, ProviderError> {
    json_rpc(
        transport,
        key,
        PRICING_ENDPOINT,
        Scope {
            region: PRICING_REGION,
            service: SERVICE,
        },
        GET_PRODUCTS_TARGET,
        &query(region, next_token),
        now_unix,
    )
    .await
}

/// The products of one page that this driver can read at all.
///
/// A `PriceList` entry that does not parse is skipped rather than fatal, for
/// the reason the instance walk skips one: the query returns every meter AWS
/// publishes under those filters, and one flyco has no shape for must not
/// take the region's whole price list with it.
fn products_of(page: &ProductPage) -> impl Iterator<Item = Product> + '_ {
    page.price_list
        .iter()
        .filter_map(|encoded| serde_json::from_str::<Product>(encoded).ok())
}

async fn read_storage<T: HttpTransport>(
    transport: &T,
    key: &AccessKey,
    region: &str,
    now_unix: u64,
) -> Result<Usd, ProviderError> {
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
            &GetProducts::gp3_storage(region, next),
            now_unix,
        )
        .await?;
        for encoded in &page.price_list {
            let Ok(product) = serde_json::from_str::<Product>(encoded) else {
                continue;
            };
            if let Some(hourly) = storage_gib_hourly(&product) {
                return Ok(hourly);
            }
        }
        next = page.next_token.filter(|token| !token.is_empty());
        if next.is_none() {
            return Err(ProviderError::Malformed(
                "AWS published no gp3 capacity price for the requested region",
            ));
        }
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

    use super::{GetProducts, ProductPage, on_demand_hourly, storage_gib_hourly};

    const PRODUCTS: &str = include_str!("../../fixtures/aws/get_products.json");
    const STORAGE_PRODUCTS: &str = include_str!("../../fixtures/aws/get_storage_products.json");

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
    fn the_storage_query_and_rate_name_gp3_capacity_exactly() {
        let query = GetProducts::gp3_storage("us-west-2", None);
        assert!(
            query
                .filters
                .iter()
                .any(|filter| { filter.field == "volumeApiName" && filter.value == "gp3" })
        );
        let page: ProductPage = serde_json::from_str(STORAGE_PRODUCTS).expect("fixture parses");
        let product =
            serde_json::from_str::<super::Product>(&page.price_list[0]).expect("product parses");
        assert_eq!(storage_gib_hourly(&product), Some(Usd::from_micros(110)));
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
