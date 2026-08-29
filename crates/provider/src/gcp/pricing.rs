//! What an hour costs, from the Cloud Billing catalog.
//!
//! Google does not publish a price per machine type. It publishes a price
//! per **vCPU-hour** and per **GiB-hour**, per machine family, per region,
//! per market — and a machine type's price is the sum of the two multiplied
//! by its own shape. `e2-standard-2` costs two core-hours plus eight
//! RAM-hours of the `e2` rate in whichever region it runs in.
//!
//! That is why the catalog is a fold rather than a lookup: the SKU list is
//! walked once per region and reduced to a rate table keyed by
//! `(family, market)`, and each machine type is then priced from its own
//! vCPU and memory counts.
//!
//! # The SKUs are told apart by their descriptions
//!
//! A SKU's `category` says it is a Compute Engine CPU or RAM SKU and whether
//! it is `OnDemand` or `Preemptible`, but not which *family* it prices —
//! that is in the human-readable description, as `E2 Instance Core` and
//! `E2 Instance Ram`. Spot SKUs say `Spot Preemptible E2 Instance Core`, and
//! carry the same `Preemptible` usage type, so the market comes from the
//! category and the family from the description. Anything that matches
//! neither shape is skipped rather than guessed at: sole-tenancy premiums,
//! commitment SKUs and custom-machine rates all live in the same list and
//! price something else.
//!
//! `clippy::future_not_send` is allowed across this module for the reason it
//! is in `azure::pricing`.
#![expect(clippy::future_not_send, reason = "see the module documentation")]

use flyco_core::money::Usd;
use serde::Deserialize;

use crate::ProviderError;
use crate::clock::MonotonicClock;
use crate::http::{HttpRequest, HttpTransport, Method};

/// The Cloud Billing catalog API.
pub const BILLING_BASE: &str = "https://cloudbilling.googleapis.com/v1";

/// Compute Engine's service id in the catalog.
///
/// A stable identifier Google publishes rather than a name to search for:
/// `services.list` would be a page-walk to find the same constant.
pub const COMPUTE_SERVICE: &str = "services/6F81-5844-456A";

/// The largest page the catalog will return.
pub const PAGE_SIZE: u32 = 5_000;

/// How long a region's rates are reused for.
///
/// Spot rates are republished on Google's own schedule and on-demand ones
/// change rarely, so six hours is short enough to pick up a republication the
/// same day and long enough that a catalog read is not a page-walk every
/// time — the same reasoning as the Azure retail catalog.
pub const CACHE_TTL_SECONDS: u64 = 6 * 60 * 60;

/// The resource family every machine-hour SKU belongs to.
const COMPUTE_FAMILY: &str = "Compute";

/// Usage type of an ordinary machine-hour SKU.
const ON_DEMAND_USAGE: &str = "OnDemand";

/// Usage type of an interruptible machine-hour SKU, spot included.
const PREEMPTIBLE_USAGE: &str = "Preemptible";

/// What a core SKU's description says.
const CORE_MARKER: &str = "Instance Core";

/// What a RAM SKU's description says.
const RAM_MARKER: &str = "Instance Ram";
/// Description marker for the persistent-disk tier flyco provisions.
const BALANCED_DISK_MARKER: &str = "Balanced PD Capacity";
const STORAGE_FAMILY: &str = "Storage";
const HOURS_PER_MONTH: u64 = 730;

/// Nanos in one unit of currency, which is how the catalog states a price.
const NANOS_PER_UNIT: u64 = 1_000_000_000;

/// Micros in one unit of currency.
const MICROS_PER_UNIT: u64 = 1_000_000;

/// The only currency flyco accounts in.
pub const CURRENCY: &str = "USD";

/// One page of the SKU catalog.
#[derive(Debug, Clone, Deserialize)]
pub struct SkuPage {
    /// The SKUs on this page.
    #[serde(default)]
    pub skus: Vec<Sku>,
    /// The next page, or empty at the end.
    #[serde(rename = "nextPageToken", default)]
    pub next_page_token: Option<String>,
}

/// One priced SKU.
#[derive(Debug, Clone, Deserialize)]
pub struct Sku {
    /// Human-readable description; the only place the machine family
    /// appears.
    #[serde(default)]
    pub description: String,
    /// What kind of thing it prices.
    #[serde(default)]
    pub category: Category,
    /// Which regions it applies to, or `global`.
    #[serde(rename = "serviceRegions", default)]
    pub service_regions: Vec<String>,
    /// Its rates over time; the current one is the last.
    #[serde(rename = "pricingInfo", default)]
    pub pricing_info: Vec<PricingInfo>,
}

/// A SKU's category.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Category {
    /// `Compute` for a machine-hour SKU.
    #[serde(rename = "resourceFamily", default)]
    pub resource_family: String,
    /// `CPU` or `RAM` for a machine-hour SKU.
    #[serde(rename = "resourceGroup", default)]
    pub resource_group: String,
    /// `OnDemand`, `Preemptible`, `Commit1Yr`, …
    #[serde(rename = "usageType", default)]
    pub usage_type: String,
}

/// One rate, valid from an instant.
#[derive(Debug, Clone, Deserialize)]
pub struct PricingInfo {
    /// How the rate is expressed.
    #[serde(rename = "pricingExpression", default)]
    pub pricing_expression: PricingExpression,
}

/// A rate's unit and tiers.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PricingExpression {
    /// The unit the rate is per, e.g. `h` or `GiBy.h`.
    #[serde(rename = "usageUnit", default)]
    pub usage_unit: String,
    /// The tiers. A machine-hour SKU has one, starting at zero.
    #[serde(rename = "tieredRates", default)]
    pub tiered_rates: Vec<TieredRate>,
}

/// One tier of a rate.
#[derive(Debug, Clone, Deserialize)]
pub struct TieredRate {
    /// Where this tier starts.
    #[serde(rename = "startUsageAmount", default)]
    pub start_usage_amount: f64,
    /// What it costs.
    #[serde(rename = "unitPrice", default)]
    pub unit_price: Money,
}

/// An amount, as the catalog states it: whole units plus nanos.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Money {
    /// Currency code.
    #[serde(rename = "currencyCode", default)]
    pub currency_code: String,
    /// Whole units, as a decimal string.
    #[serde(default)]
    pub units: Option<String>,
    /// Billionths of a unit.
    #[serde(default)]
    pub nanos: i64,
}

impl Money {
    /// This amount in exact microdollars.
    ///
    /// The catalog states a price as units plus nanos rather than as a
    /// float, which means it can be converted exactly — no rounding at the
    /// boundary and no approximation to carry downstream. Nanos are a
    /// thousand times finer than a microdollar, so the division is the one
    /// place precision is lost, and it is lost the same way every time.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Rejected`] when the catalog quotes a
    /// currency flyco does not account in: there is no rate to convert at,
    /// and a euro reported as a dollar is a budget quietly told the wrong
    /// number.
    pub fn micros(&self) -> Result<u64, ProviderError> {
        if !self.currency_code.is_empty() && !self.currency_code.eq_ignore_ascii_case(CURRENCY) {
            return Err(ProviderError::Rejected(format!(
                "Google prices this project in {}, and flyco accounts in {CURRENCY}",
                self.currency_code
            )));
        }

        let units: u64 = self.units.as_deref().unwrap_or("0").parse().map_err(|_| {
            ProviderError::Malformed("a Google price stated units that are not a number")
        })?;
        let nanos = u64::try_from(self.nanos.max(0)).unwrap_or(0);
        Ok(units
            .saturating_mul(MICROS_PER_UNIT)
            .saturating_add(nanos / (NANOS_PER_UNIT / MICROS_PER_UNIT)))
    }
}

/// The family Google left unnamed, back when it was the only one.
const UNNAMED_FAMILY: &str = "n1";

/// Whether a word is a machine-family code.
///
/// Letters then digits, optionally then letters again: `e2`, `n1`, `c3d`,
/// `t2d`. That shape is what separates a family from the adjectives sharing
/// the description with it — `Spot`, `Preemptible`, `Predefined`, `Custom`,
/// `AMD` — none of which carries a digit.
fn is_family_code(word: &str) -> bool {
    let mut characters = word.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && word.chars().any(|character| character.is_ascii_digit())
        && word
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
}

/// Which market a SKU prices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Market {
    /// Ordinary capacity.
    OnDemand,
    /// Interruptible capacity — which the catalog calls `Preemptible` even
    /// for the SKUs whose descriptions say `Spot`.
    Spot,
}

/// What part of a machine a SKU prices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component {
    /// A vCPU-hour.
    Core,
    /// A GiB-hour of memory.
    Ram,
}

impl Sku {
    /// Which market this SKU prices, when it prices a machine-hour at all.
    #[must_use]
    pub fn market(&self) -> Option<Market> {
        if self.category.resource_family != COMPUTE_FAMILY {
            return None;
        }
        match self.category.usage_type.as_str() {
            ON_DEMAND_USAGE => Some(Market::OnDemand),
            PREEMPTIBLE_USAGE => Some(Market::Spot),
            // Commitment and reservation SKUs price something a session
            // cannot buy by the hour.
            _ => None,
        }
    }

    /// Which component this SKU prices, and for which machine family.
    ///
    /// The family is the first *family-shaped* word before the marker, not
    /// simply the word before it: the descriptions read `E2 Instance Core`
    /// but also `N1 Predefined Instance Core` and `Spot Preemptible E2
    /// Instance Core`, so "the previous word" would price the `n1` family
    /// under `predefined`. A family code is letters then digits —
    /// `e2`, `n1`, `c3d`, `t2d` — which none of `Spot`, `Preemptible`,
    /// `Predefined` or `Custom` is.
    ///
    /// A description with no family word at all (`Custom Instance Core`) is
    /// `n1`, the family Google left unnamed when it was the only one.
    #[must_use]
    pub fn component(&self) -> Option<(String, Component)> {
        let (marker, component) = if self.description.contains(CORE_MARKER) {
            (CORE_MARKER, Component::Core)
        } else if self.description.contains(RAM_MARKER) {
            (RAM_MARKER, Component::Ram)
        } else {
            return None;
        };

        let family = self
            .description
            .split(marker)
            .next()?
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .find(|word| is_family_code(word))
            .unwrap_or_else(|| UNNAMED_FAMILY.to_owned());
        Some((family, component))
    }

    /// The current rate, in microdollars per unit.
    ///
    /// The *last* pricing entry is the current one — the catalog lists them
    /// oldest first — and the first tier is the one a session pays, because
    /// a machine-hour SKU has exactly one tier starting at zero.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the rate is quoted in another
    /// currency or is not a number.
    pub fn rate_micros(&self) -> Result<Option<u64>, ProviderError> {
        let Some(expression) = self
            .pricing_info
            .last()
            .map(|info| &info.pricing_expression)
        else {
            return Ok(None);
        };
        let Some(tier) = expression
            .tiered_rates
            .iter()
            .find(|tier| tier.start_usage_amount <= 0.0)
            .or_else(|| expression.tiered_rates.first())
        else {
            return Ok(None);
        };
        tier.unit_price.micros().map(Some)
    }

    /// Whether this SKU applies in a region.
    #[must_use]
    pub fn covers(&self, region: &str) -> bool {
        self.service_regions
            .iter()
            .any(|covered| covered == region || covered == "global")
    }
}

/// The per-hour rates of one machine family in one market.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FamilyRates {
    /// Microdollars per vCPU-hour.
    pub core: Option<u64>,
    /// Microdollars per GiB-hour.
    pub ram: Option<u64>,
}

impl FamilyRates {
    /// What one hour of a machine of this shape costs, when both halves of
    /// the rate are published.
    ///
    /// Memory arrives in MiB and the rate is per GiB, so the multiplication
    /// happens in MiB and the division comes last: dividing first would
    /// round a 1.5 GiB machine down to one.
    #[must_use]
    pub fn hourly(&self, vcpus: u32, memory_mib: u64) -> Option<Usd> {
        let core = self.core?;
        let ram = self.ram?;
        Some(Usd::from_micros(
            core.saturating_mul(u64::from(vcpus))
                .saturating_add(ram.saturating_mul(memory_mib) / 1_024),
        ))
    }
}

/// One region's rates, keyed by `(family, market)`.
#[derive(Debug, Clone, Default)]
pub struct RegionRates {
    entries: Vec<(String, Market, FamilyRates)>,
    storage_gib_hourly: Option<Usd>,
}

impl RegionRates {
    /// Published price of one GiB-hour of balanced persistent disk.
    #[must_use]
    pub const fn storage_gib_hourly(&self) -> Option<Usd> {
        self.storage_gib_hourly
    }

    /// The rates of one machine family in one market.
    #[must_use]
    pub fn family(&self, family: &str, market: Market) -> Option<FamilyRates> {
        self.entries
            .iter()
            .find(|(name, seen, _)| name == family && *seen == market)
            .map(|(_, _, rates)| *rates)
    }

    /// Records one rate, creating the family's entry if it is the first.
    fn record(&mut self, family: &str, market: Market, component: Component, micros: u64) {
        if !self
            .entries
            .iter()
            .any(|(name, seen, _)| name == family && *seen == market)
        {
            self.entries
                .push((family.to_owned(), market, FamilyRates::default()));
        }
        let entry = &mut self
            .entries
            .iter_mut()
            .find(|(name, seen, _)| name == family && *seen == market)
            .expect("the family's entry was just ensured to be present")
            .2;
        match component {
            Component::Core => entry.core = Some(micros),
            Component::Ram => entry.ram = Some(micros),
        }
    }

    /// Folds one region's SKUs into a rate table.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when a rate is quoted in another currency.
    pub fn fold(skus: &[Sku], region: &str) -> Result<Self, ProviderError> {
        let mut rates = Self::default();
        for sku in skus {
            if !sku.covers(region) {
                continue;
            }
            if sku.category.resource_family == STORAGE_FAMILY
                && sku.description.contains(BALANCED_DISK_MARKER)
                && sku.category.usage_type == ON_DEMAND_USAGE
            {
                if let Some(monthly) = sku.rate_micros()? {
                    rates.storage_gib_hourly = Some(Usd::from_micros(
                        monthly.saturating_add(HOURS_PER_MONTH - 1) / HOURS_PER_MONTH,
                    ));
                }
                continue;
            }
            let (Some(market), Some((family, component))) = (sku.market(), sku.component()) else {
                continue;
            };
            if let Some(micros) = sku.rate_micros()? {
                rates.record(&family, market, component, micros);
            }
        }
        Ok(rates)
    }
}

/// One region's rates, and when they stop being reused.
#[derive(Debug, Clone)]
struct CachedRegion {
    region: String,
    rates: RegionRates,
    expires_after: u64,
}

/// Rates per region, with a time-to-live.
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

    /// The first page's URL for the Compute Engine SKU list.
    #[must_use]
    pub fn skus_url(page_token: Option<&str>) -> String {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query.append_pair("pageSize", &PAGE_SIZE.to_string());
        if let Some(token) = page_token {
            query.append_pair("pageToken", token);
        }
        format!("{BILLING_BASE}/{COMPUTE_SERVICE}/skus?{}", query.finish())
    }

    /// One region's rate table, reading the catalog if the cache is cold,
    /// for another region, or expired.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] if the catalog refuses the read or quotes a
    /// currency flyco does not account in.
    ///
    /// # Panics
    ///
    /// Never in practice: the cache is filled immediately above the read
    /// that borrows it.
    pub async fn region_rates<T: HttpTransport, C: MonotonicClock>(
        &mut self,
        transport: &T,
        clock: &C,
        token: &str,
        region: &str,
    ) -> Result<&RegionRates, ProviderError> {
        let now = clock.elapsed_seconds();
        let fresh = self
            .cached
            .as_ref()
            .is_some_and(|cached| cached.region == region && now < cached.expires_after);

        if !fresh {
            let rates = read_region(transport, token, region).await?;
            self.cached = Some(CachedRegion {
                region: region.to_owned(),
                rates,
                expires_after: now.saturating_add(CACHE_TTL_SECONDS),
            });
        }

        Ok(&self
            .cached
            .as_ref()
            .expect("the cache was just filled")
            .rates)
    }
}

/// Walks every page of the SKU catalog and folds one region out of it.
async fn read_region<T: HttpTransport>(
    transport: &T,
    token: &str,
    region: &str,
) -> Result<RegionRates, ProviderError> {
    let mut collected = Vec::new();
    let mut next: Option<String> = None;

    loop {
        let request =
            HttpRequest::new(Method::Get, PriceCatalog::skus_url(next.as_deref())).bearer(token);
        let response = transport.send(request).await?;
        if !response.is_success() {
            return Err(super::compute::refusal(&response));
        }

        let page: SkuPage = response.json()?;
        collected.extend(page.skus);
        next = page.next_page_token.filter(|token| !token.is_empty());
        if next.is_none() {
            return RegionRates::fold(&collected, region);
        }
    }
}

#[cfg(test)]
mod tests {
    use flyco_core::money::Usd;

    use super::{Component, Market, PriceCatalog, RegionRates, Sku, SkuPage};

    const SKUS: &str = include_str!("../../fixtures/gcp/skus.json");

    fn skus() -> Vec<Sku> {
        serde_json::from_str::<SkuPage>(SKUS)
            .expect("the SKU fixture parses")
            .skus
    }

    fn rates(region: &str) -> RegionRates {
        RegionRates::fold(&skus(), region).expect("fold")
    }

    #[test]
    fn the_sku_query_asks_for_compute_engines_own_service() {
        let url = PriceCatalog::skus_url(None);
        assert!(
            url.starts_with("https://cloudbilling.googleapis.com/v1/services/6F81-5844-456A/skus?")
        );
        assert!(url.contains("pageSize=5000"));
        assert!(!url.contains("pageToken"));
        assert!(PriceCatalog::skus_url(Some("next")).contains("pageToken=next"));
    }

    #[test]
    fn a_skus_market_and_family_come_from_its_category_and_description() {
        let of = |description: &str| {
            skus()
                .into_iter()
                .find(|sku| sku.description == description)
                .unwrap_or_else(|| panic!("the fixture holds `{description}`"))
        };

        let core = of("E2 Instance Core running in Americas");
        assert_eq!(core.market(), Some(Market::OnDemand));
        assert_eq!(core.component(), Some(("e2".to_owned(), Component::Core)));

        let ram = of("E2 Instance Ram running in Americas");
        assert_eq!(ram.component(), Some(("e2".to_owned(), Component::Ram)));

        // Spot SKUs carry the `Preemptible` usage type and say `Spot` in the
        // description, so the market comes from the category.
        let spot = of("Spot Preemptible E2 Instance Core running in Americas");
        assert_eq!(spot.market(), Some(Market::Spot));
        assert_eq!(spot.component(), Some(("e2".to_owned(), Component::Core)));

        // The family is the first family-shaped word, not the previous one:
        // "the word before the marker" would price `n1` under `predefined`.
        assert_eq!(
            of("N1 Predefined Instance Core running in Americas").component(),
            Some(("n1".to_owned(), Component::Core))
        );

        // A commitment prices something a session cannot buy by the hour.
        assert_eq!(
            of("Commitment v1: Cpu in Americas for 1 Year").market(),
            None
        );
        // And a licence is not a machine-hour at all.
        assert_eq!(
            of("Licensing Fee for Ubuntu Pro on VM with 2 VCPU").component(),
            None
        );
    }

    #[test]
    fn a_price_converts_exactly_from_units_and_nanos() {
        // The catalog states units plus nanos rather than a float, so the
        // conversion is exact rather than rounded at the boundary.
        let rates = rates("us-central1");
        let on_demand = rates
            .family("e2", Market::OnDemand)
            .expect("the E2 family is priced");
        assert_eq!(on_demand.core, Some(21_811));
        assert_eq!(on_demand.ram, Some(2_923));
    }

    #[test]
    fn a_machine_types_price_is_its_shape_times_the_familys_rates() {
        // `e2-standard-2` is two vCPUs and 8 GiB, so it is two core-hours
        // plus eight RAM-hours: there is no per-machine-type price to look
        // up.
        let rates = rates("us-central1");
        assert_eq!(
            rates
                .family("e2", Market::OnDemand)
                .expect("priced")
                .hourly(2, 8_192),
            Some(Usd::from_micros(2 * 21_811 + 8 * 2_923))
        );

        // Spot is a different, much lower rate for the same shape.
        assert_eq!(
            rates
                .family("e2", Market::Spot)
                .expect("priced")
                .hourly(2, 8_192),
            Some(Usd::from_micros(2 * 6_543 + 8 * 877))
        );
    }

    #[test]
    fn memory_is_multiplied_before_it_is_divided() {
        // A 1.5 GiB machine must not be rounded down to one, which is what
        // dividing MiB by 1024 first would do.
        let rates = rates("us-central1");
        let family = rates.family("e2", Market::OnDemand).expect("priced");
        assert_eq!(
            family.hourly(2, 1_536),
            Some(Usd::from_micros(2 * 21_811 + (2_923 * 1_536) / 1_024))
        );
    }

    #[test]
    fn a_region_the_skus_do_not_cover_is_unpriced_rather_than_mispriced() {
        // Quoting one region's rate for another would be a number nothing
        // can be bought at, so a region no SKU names has no rates at all.
        assert!(rates("asia-east1").family("e2", Market::OnDemand).is_none());

        // And a region with only half a rate — `europe-west4` publishes an
        // E2 core price in the fixture and no RAM price — cannot be quoted
        // either.
        assert_eq!(
            rates("europe-west4")
                .family("e2", Market::OnDemand)
                .expect("half-priced")
                .hourly(2, 8_192),
            None
        );
    }

    #[test]
    fn a_family_with_only_half_a_rate_cannot_be_priced() {
        // `n2` has a core rate in the fixture and no RAM rate, and half a
        // price is not a price.
        let rates = rates("us-central1");
        let partial = rates.family("n2", Market::OnDemand).expect("half-priced");
        assert_eq!(partial.core, Some(31_611));
        assert_eq!(partial.ram, None);
        assert_eq!(partial.hourly(2, 8_192), None);
    }

    #[test]
    fn a_rate_quoted_in_another_currency_is_refused() {
        let euros: SkuPage =
            serde_json::from_str(include_str!("../../fixtures/gcp/skus_euros.json"))
                .expect("the fixture parses");
        let error = RegionRates::fold(&euros.skus, "europe-west4")
            .expect_err("euros must not be reported as dollars");
        assert!(error.to_string().contains("EUR"));
    }
}
