//! The retail-prices catalog.
//!
//! Unauthenticated — `prices.azure.com` needs no token — and filtered
//! server-side to one region's consumption meters for virtual machines. What
//! comes back is several rows per machine type, and the whole difficulty is
//! telling them apart:
//!
//! * **Windows** rows are the ones whose `productName` contains `Windows`.
//!   There is no Linux marker; Linux is the row without one.
//! * **Spot** rows are the ones whose `skuName` ends with ` Spot`.
//! * **Low Priority** rows (` Low Priority`) are the older scale-set meter
//!   and are *not* spot. Reading one as a spot price quotes a number nothing
//!   can be bought at.
//!
//! Spot rows carry a short `effectiveStartDate`/`effectiveEndDate` window
//! because they are republished monthly, so the catalog is cached with a TTL
//! rather than for the life of the process.
//!
//! `clippy::future_not_send` is allowed across this module for the same
//! reason it is in [`super::auth`]: `Send`-ness follows from the concrete
//! transport, and bounding `T: Sync` would forbid the recorded transport the
//! whole driver is tested against.
#![expect(clippy::future_not_send, reason = "see the module documentation")]

use core::fmt::Write as _;

use flyco_core::machine::{StoragePriceTier, StoragePricing};
use flyco_core::money::Usd;
use serde::Deserialize;

use crate::ProviderError;
use crate::clock::MonotonicClock;
use crate::http::{HttpRequest, HttpTransport, Method};

/// The unauthenticated retail-prices endpoint.
pub const RETAIL_PRICES_URL: &str = "https://prices.azure.com/api/retail/prices";

/// API version whose `$filter` supports `armRegionName`.
///
/// Its filter values are case-sensitive, unlike earlier versions', which is
/// why `Virtual Machines` below is spelled exactly so.
pub const PRICES_API_VERSION: &str = "2023-01-01-preview";

/// How long a region's prices are reused for.
///
/// Spot meters are republished monthly and on-demand ones change rarely, so
/// six hours is short enough to pick up a republication the same day and
/// long enough that a catalog read is not a page-walk every time.
pub const CACHE_TTL_SECONDS: u64 = 6 * 60 * 60;

/// Suffix marking a spot meter.
const SPOT_SUFFIX: &str = " Spot";

/// Suffix marking the older low-priority scale-set meter, which is not spot.
const LOW_PRIORITY_SUFFIX: &str = " Low Priority";

/// Substring marking a Windows meter.
const WINDOWS_MARKER: &str = "Windows";
const HOURS_PER_MONTH: u64 = 730;

/// One page of retail prices.
#[derive(Debug, Clone, Deserialize)]
pub struct PricePage {
    /// The rows on this page.
    #[serde(rename = "Items", default)]
    pub items: Vec<PriceRow>,
    /// The next page, or `null` at the end.
    #[serde(rename = "NextPageLink")]
    pub next_page_link: Option<String>,
}

/// One priced meter.
#[derive(Debug, Clone, Deserialize)]
pub struct PriceRow {
    /// The machine type this meter prices, e.g. `Standard_B2pts_v2`.
    #[serde(rename = "armSkuName", default)]
    pub arm_sku_name: String,
    /// Price per hour in the quoted currency.
    #[serde(rename = "retailPrice")]
    pub retail_price: f64,
    /// Product name; carries the `Windows` marker when it is a Windows
    /// meter.
    #[serde(rename = "productName", default)]
    pub product_name: String,
    /// SKU name; carries the ` Spot` and ` Low Priority` suffixes.
    #[serde(rename = "skuName", default)]
    pub sku_name: String,
    /// Meter name distinguishes disk capacity from mounts and operations.
    #[serde(rename = "meterName", default)]
    pub meter_name: String,
    /// Unit the price is quoted per.
    #[serde(rename = "unitOfMeasure", default)]
    pub unit_of_measure: String,
}

/// What kind of meter a row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeterKind {
    /// Ordinary Linux on-demand.
    OnDemand,
    /// Linux spot.
    Spot,
    /// Something this driver does not price: Windows, or the legacy
    /// low-priority scale-set meter.
    Ignored,
}

impl PriceRow {
    /// Classifies this row.
    #[must_use]
    pub fn kind(&self) -> MeterKind {
        if self.product_name.contains(WINDOWS_MARKER) {
            return MeterKind::Ignored;
        }
        if self.sku_name.ends_with(LOW_PRIORITY_SUFFIX) {
            return MeterKind::Ignored;
        }
        if self.sku_name.ends_with(SPOT_SUFFIX) {
            return MeterKind::Spot;
        }
        MeterKind::OnDemand
    }

    /// The hourly price as exact microdollars.
    #[must_use]
    pub fn hourly(&self) -> Usd {
        // The API quotes a decimal that a float can only approximate;
        // rounding to the nearest microdollar at the boundary is what keeps
        // every amount downstream an exact integer.
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "an hourly VM price in microdollars is small and non-negative"
        )]
        let micros = (self.retail_price * 1_000_000.0).round() as u64;
        Usd::from_micros(micros)
    }
}

/// The Linux prices of one machine type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MachinePrices {
    /// On-demand price per hour, when the region publishes one.
    pub on_demand: Option<Usd>,
    /// Spot price per hour, when the region publishes one.
    pub spot: Option<Usd>,
}

/// One region's prices, and when they stop being reused.
#[derive(Debug, Clone)]
struct CachedRegion {
    region: String,
    prices: Vec<(String, MachinePrices)>,
    expires_after: u64,
}

#[derive(Debug, Clone)]
struct CachedStorage {
    region: String,
    pricing: StoragePricing,
    expires_after: u64,
}

/// Prices per region, with a time-to-live.
#[derive(Debug, Clone, Default)]
pub struct PriceCatalog {
    cached: Option<CachedRegion>,
    storage_cached: Option<CachedStorage>,
}

/// The `OData` filter for one region's Linux consumption meters.
#[must_use]
pub fn price_filter(region: &str) -> String {
    let mut filter = String::with_capacity(96);
    // Values are case-sensitive at this API version: `virtual machines`
    // matches nothing.
    filter.push_str("serviceName eq 'Virtual Machines' and armRegionName eq '");
    filter.push_str(region);
    filter.push_str("' and priceType eq 'Consumption'");
    filter
}

/// The first page's URL for one region.
#[must_use]
pub fn price_url(region: &str) -> String {
    let mut url = String::with_capacity(RETAIL_PRICES_URL.len() + 160);
    url.push_str(RETAIL_PRICES_URL);
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("api-version", PRICES_API_VERSION)
        .append_pair("$filter", &price_filter(region))
        .finish();
    let _ = write!(url, "?{query}");
    url
}

impl PriceCatalog {
    /// An empty catalog.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cached: None,
            storage_cached: None,
        }
    }

    /// The prices of one machine type in one region, reading the region's
    /// meters if the cache is cold, for another region, or expired.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] if the prices API refuses or answers with
    /// something that is not a price page.
    pub async fn prices_for<T: HttpTransport, C: MonotonicClock>(
        &mut self,
        transport: &T,
        clock: &C,
        region: &str,
        machine_type: &str,
    ) -> Result<MachinePrices, ProviderError> {
        self.region_prices(transport, clock, region)
            .await
            .map(|prices| {
                prices
                    .iter()
                    .find(|(name, _)| name == machine_type)
                    .map(|(_, prices)| *prices)
                    .unwrap_or_default()
            })
    }

    /// Every priced machine type in one region.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] if the prices API refuses or answers with
    /// something that is not a price page.
    ///
    /// # Panics
    ///
    /// Never in practice: the cache is filled immediately above the read
    /// that borrows it, and the borrow checker is what makes returning a
    /// reference to it need the `expect` at all.
    pub async fn region_prices<T: HttpTransport, C: MonotonicClock>(
        &mut self,
        transport: &T,
        clock: &C,
        region: &str,
    ) -> Result<&[(String, MachinePrices)], ProviderError> {
        let now = clock.elapsed_seconds();
        let fresh = self
            .cached
            .as_ref()
            .is_some_and(|cached| cached.region == region && now < cached.expires_after);

        if !fresh {
            let prices = read_region(transport, region).await?;
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

    /// Published Standard SSD LRS capacity tiers for one region.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] if the retail-prices API refuses the read,
    /// returns malformed data, or publishes no usable capacity tiers.
    ///
    /// # Panics
    ///
    /// Never in practice: the cache is filled immediately before the borrow.
    pub async fn storage_pricing<T: HttpTransport, C: MonotonicClock>(
        &mut self,
        transport: &T,
        clock: &C,
        region: &str,
    ) -> Result<&StoragePricing, ProviderError> {
        let now = clock.elapsed_seconds();
        let fresh = self
            .storage_cached
            .as_ref()
            .is_some_and(|cached| cached.region == region && now < cached.expires_after);
        if !fresh {
            self.storage_cached = Some(CachedStorage {
                region: region.to_owned(),
                pricing: read_storage(transport, region).await?,
                expires_after: now.saturating_add(CACHE_TTL_SECONDS),
            });
        }
        Ok(&self
            .storage_cached
            .as_ref()
            .expect("the storage cache was just filled")
            .pricing)
    }
}

fn storage_filter(region: &str) -> String {
    format!(
        "productName eq 'Standard SSD Managed Disks' and armRegionName eq '{region}' and priceType eq 'Consumption'"
    )
}

fn storage_url(region: &str) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("api-version", PRICES_API_VERSION)
        .append_pair("$filter", &storage_filter(region))
        .finish();
    format!("{RETAIL_PRICES_URL}?{query}")
}

fn disk_capacity(sku_name: &str) -> Option<u32> {
    let code = sku_name.strip_suffix(" LRS")?;
    Some(match code {
        "E1" => 4,
        "E2" => 8,
        "E3" => 16,
        "E4" => 32,
        "E6" => 64,
        "E10" => 128,
        "E15" => 256,
        "E20" => 512,
        "E30" => 1_024,
        "E40" => 2_048,
        "E50" => 4_096,
        "E60" => 8_192,
        "E70" => 16_384,
        "E80" => 32_767,
        _ => return None,
    })
}

async fn read_storage<T: HttpTransport>(
    transport: &T,
    region: &str,
) -> Result<StoragePricing, ProviderError> {
    let mut tiers = Vec::new();
    let mut next = Some(storage_url(region));
    while let Some(url) = next {
        let response = transport.send(HttpRequest::new(Method::Get, url)).await?;
        if !response.is_success() {
            return Err(ProviderError::Rejected(format!(
                "Azure retail prices refused Standard SSD pricing with HTTP {}",
                response.status
            )));
        }
        let page: PricePage = response.json()?;
        for row in &page.items {
            if row.unit_of_measure != "1/Month" || !row.meter_name.ends_with(" Disk") {
                continue;
            }
            let Some(capacity_gib) = disk_capacity(&row.sku_name) else {
                continue;
            };
            let monthly = row.hourly().micros();
            tiers.push(StoragePriceTier {
                capacity_gib,
                hourly: Usd::from_micros(
                    monthly.saturating_add(HOURS_PER_MONTH - 1) / HOURS_PER_MONTH,
                ),
            });
        }
        next = page.next_page_link;
    }
    tiers.sort_by_key(|tier| tier.capacity_gib);
    if tiers.is_empty() {
        return Err(ProviderError::Malformed(
            "Azure published no Standard SSD LRS capacity tiers for the requested region",
        ));
    }
    Ok(StoragePricing::CapacityTiers { tiers })
}

/// Walks every page of one region's prices and folds them per machine type.
async fn read_region<T: HttpTransport>(
    transport: &T,
    region: &str,
) -> Result<Vec<(String, MachinePrices)>, ProviderError> {
    let mut folded: Vec<(String, MachinePrices)> = Vec::new();
    let mut next = Some(price_url(region));

    while let Some(url) = next {
        let response = transport.send(HttpRequest::new(Method::Get, url)).await?;
        if !response.is_success() {
            return Err(ProviderError::Rejected(format!(
                "the Azure retail prices API answered HTTP {}",
                response.status
            )));
        }

        let page: PricePage = response.json()?;
        for row in &page.items {
            let kind = row.kind();
            if kind == MeterKind::Ignored || row.arm_sku_name.is_empty() {
                continue;
            }

            if !folded.iter().any(|(name, _)| *name == row.arm_sku_name) {
                folded.push((row.arm_sku_name.clone(), MachinePrices::default()));
            }
            let entry = folded
                .iter_mut()
                .find(|(name, _)| *name == row.arm_sku_name)
                .map(|(_, prices)| prices)
                .expect("the row's machine type was just ensured to be present");

            match kind {
                MeterKind::OnDemand => entry.on_demand = Some(row.hourly()),
                MeterKind::Spot => entry.spot = Some(row.hourly()),
                MeterKind::Ignored => unreachable!("ignored rows were skipped above"),
            }
        }

        next = page.next_page_link;
    }

    Ok(folded)
}

#[cfg(test)]
mod tests {
    use flyco_core::money::Usd;

    use super::{MeterKind, PRICES_API_VERSION, PriceCatalog, PricePage, price_url};
    use crate::clock::ManualClock;
    use crate::http::{HttpResponse, Method};
    use crate::testing::RecordedTransport;

    const PRICES: &str = include_str!("../../fixtures/azure/retail_prices.json");
    /// A first page that names a `NextPageLink`; `PRICES` is the last page.
    const PRICES_FIRST_PAGE: &str =
        include_str!("../../fixtures/azure/retail_prices_first_page.json");

    fn page(body: &str) -> HttpResponse {
        HttpResponse::new(200, body.as_bytes().to_vec())
    }

    #[test]
    fn the_price_query_is_case_sensitively_filtered_to_one_region() {
        let url = price_url("northcentralus");
        assert!(url.starts_with("https://prices.azure.com/api/retail/prices?"));
        assert!(url.contains(&format!("api-version={PRICES_API_VERSION}")));
        assert!(url.contains("serviceName+eq+%27Virtual+Machines%27"));
        assert!(url.contains("armRegionName+eq+%27northcentralus%27"));
        assert!(url.contains("priceType+eq+%27Consumption%27"));
    }

    #[test]
    fn windows_spot_and_low_priority_rows_are_told_apart() {
        let page: PricePage = serde_json::from_str(PRICES).expect("the fixture parses");
        let kind = |sku: &str, product: &str| {
            page.items
                .iter()
                .find(|row| row.sku_name == sku && row.product_name == product)
                .unwrap_or_else(|| panic!("the fixture holds `{sku}` / `{product}`"))
                .kind()
        };

        assert_eq!(
            kind("D2als v6", "Virtual Machines Dalsv6 Series"),
            MeterKind::OnDemand
        );
        assert_eq!(
            kind("D2als v6 Spot", "Virtual Machines Dalsv6 Series"),
            MeterKind::Spot
        );
        assert_eq!(
            kind("D2als v6 Low Priority", "Virtual Machines Dalsv6 Series"),
            MeterKind::Ignored
        );
        assert_eq!(
            kind("D2als v6", "Virtual Machines Dalsv6 Series Windows"),
            MeterKind::Ignored
        );
    }

    #[tokio::test]
    async fn a_regions_prices_fold_into_one_on_demand_and_one_spot_amount() {
        let transport = RecordedTransport::new(vec![page(PRICES)]);
        let clock = ManualClock::new();
        let mut catalog = PriceCatalog::new();

        let prices = catalog
            .prices_for(&transport, &clock, "northcentralus", "Standard_D2als_v6")
            .await
            .expect("read prices");

        assert_eq!(prices.on_demand, Some(Usd::from_micros(76_400)));
        assert_eq!(prices.spot, Some(Usd::from_micros(14_126)));
        assert_eq!(transport.request(0).method, Method::Get);
    }

    #[tokio::test]
    async fn every_page_is_walked_before_the_catalog_answers() {
        let transport = RecordedTransport::new(vec![page(PRICES_FIRST_PAGE), page(PRICES)]);
        let clock = ManualClock::new();
        let mut catalog = PriceCatalog::new();

        let prices = catalog
            .prices_for(&transport, &clock, "canadacentral", "Standard_D2pls_v5")
            .await
            .expect("read prices");

        assert_eq!(transport.request_count(), 2);
        assert_eq!(
            transport.request(1).url,
            "https://prices.azure.com/api/retail/prices?page=2"
        );
        assert_eq!(prices.on_demand, Some(Usd::from_micros(49_000)));
        assert_eq!(prices.spot, Some(Usd::from_micros(9_063)));
    }

    #[tokio::test]
    async fn prices_are_cached_until_the_ttl_expires() {
        let transport = RecordedTransport::new(vec![page(PRICES), page(PRICES)]);
        let clock = ManualClock::new();
        let mut catalog = PriceCatalog::new();

        catalog
            .prices_for(&transport, &clock, "northcentralus", "Standard_D2als_v6")
            .await
            .expect("read prices");
        clock.advance(super::CACHE_TTL_SECONDS - 1);
        catalog
            .prices_for(&transport, &clock, "northcentralus", "Standard_D2als_v6")
            .await
            .expect("reuse prices");
        assert_eq!(transport.request_count(), 1);

        // Spot meters are republished monthly, so the cache must expire.
        clock.advance(1);
        catalog
            .prices_for(&transport, &clock, "northcentralus", "Standard_D2als_v6")
            .await
            .expect("re-read prices");
        assert_eq!(transport.request_count(), 2);
    }

    #[tokio::test]
    async fn another_region_is_never_answered_from_this_regions_cache() {
        let transport = RecordedTransport::new(vec![page(PRICES), page(PRICES)]);
        let clock = ManualClock::new();
        let mut catalog = PriceCatalog::new();

        catalog
            .prices_for(&transport, &clock, "northcentralus", "Standard_D2als_v6")
            .await
            .expect("northcentralus");
        catalog
            .prices_for(&transport, &clock, "canadacentral", "Standard_D2pls_v5")
            .await
            .expect("canadacentral");

        assert_eq!(transport.request_count(), 2);
        assert!(transport.request(1).url.contains("canadacentral"));
    }

    #[tokio::test]
    async fn a_machine_type_the_region_does_not_price_has_no_prices() {
        let transport = RecordedTransport::new(vec![page(PRICES)]);
        let clock = ManualClock::new();
        let mut catalog = PriceCatalog::new();

        let prices = catalog
            .prices_for(&transport, &clock, "northcentralus", "Standard_NotReal")
            .await
            .expect("read prices");
        assert_eq!(prices.on_demand, None);
        assert_eq!(prices.spot, None);
    }
}
