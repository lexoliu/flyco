//! Where each Azure region sits.
//!
//! Azure's `locations` API does publish a `metadata.latitude`/`longitude`
//! pair, but a catalog builds entries for the regions the subscription's
//! policy already named — and a policy can name any region Azure offers —
//! so the geography is written down here rather than fetched, one row per
//! public region. Reading it back costs a round trip for a value that
//! changes about as often as Azure opens a datacentre; when it does, the
//! row is added and the entry starts claiming a place.
//!
//! A region the table does not name resolves to `None` rather than a
//! guess: the entry still catalogs, it just does not claim a place.

use flyco_core::machine::RegionLocation;

/// `(region, latitude, longitude)` for every public Azure region.
const LOCATIONS: &[(&str, f64, f64)] = &[
    // Canberra.
    ("australiacentral", -35.28, 149.13),
    // Canberra.
    ("australiacentral2", -35.28, 149.13),
    // New South Wales.
    ("australiaeast", -33.87, 151.21),
    // Victoria.
    ("australiasoutheast", -37.81, 144.96),
    // Vienna.
    ("austriaeast", 48.21, 16.37),
    // Brussels.
    ("belgiumcentral", 50.85, 4.35),
    // São Paulo state.
    ("brazilsouth", -23.55, -46.63),
    // Rio de Janeiro state.
    ("brazilsoutheast", -22.91, -43.17),
    // São Paulo state (US-operated).
    ("brazilus", -23.55, -46.63),
    // Toronto.
    ("canadacentral", 43.65, -79.38),
    // Quebec City.
    ("canadaeast", 46.82, -71.22),
    // Pune.
    ("centralindia", 18.52, 73.86),
    // Iowa.
    ("centralus", 41.59, -93.62),
    // Santiago.
    ("chilecentral", -33.45, -70.67),
    // Hong Kong.
    ("eastasia", 22.32, 114.17),
    // Virginia.
    ("eastus", 37.43, -78.66),
    // Virginia.
    ("eastus2", 36.82, -78.43),
    // Georgia.
    ("eastus3", 33.75, -84.39),
    // Paris.
    ("francecentral", 48.86, 2.35),
    // Marseille.
    ("francesouth", 43.30, 5.37),
    // Berlin.
    ("germanynorth", 52.52, 13.40),
    // Frankfurt.
    ("germanywestcentral", 50.11, 8.68),
    // Jakarta.
    ("indonesiacentral", -6.21, 106.85),
    // Tel Aviv.
    ("israelcentral", 32.08, 34.78),
    // Milan.
    ("italynorth", 45.46, 9.19),
    // Tokyo.
    ("japaneast", 35.68, 139.69),
    // Osaka.
    ("japanwest", 34.69, 135.50),
    // Nagpur.
    ("jioindiacentral", 21.15, 79.09),
    // Jamnagar.
    ("jioindiawest", 22.47, 70.06),
    // Seoul.
    ("koreacentral", 37.57, 126.98),
    // Busan.
    ("koreasouth", 35.18, 129.08),
    // Johor.
    ("malaysiasouth", 1.49, 103.74),
    // Querétaro.
    ("mexicocentral", 20.59, -100.39),
    // Auckland.
    ("newzealandnorth", -36.85, 174.76),
    // Illinois.
    ("northcentralus", 41.88, -87.63),
    // Ireland.
    ("northeurope", 53.35, -6.26),
    // Oslo.
    ("norwayeast", 59.91, 10.75),
    // Stavanger.
    ("norwaywest", 58.97, 5.73),
    // Warsaw.
    ("polandcentral", 52.23, 21.01),
    // Doha.
    ("qatarcentral", 25.29, 51.53),
    // Johannesburg.
    ("southafricanorth", -26.20, 28.05),
    // Cape Town.
    ("southafricawest", -33.92, 18.42),
    // Texas.
    ("southcentralus", 29.42, -98.49),
    // Singapore.
    ("southeastasia", 1.35, 103.82),
    // Chennai.
    ("southindia", 13.08, 80.27),
    // Madrid.
    ("spaincentral", 40.42, -3.70),
    // Gävle.
    ("swedencentral", 60.67, 17.14),
    // Malmö.
    ("swedensouth", 55.61, 13.00),
    // Zurich.
    ("switzerlandnorth", 47.38, 8.54),
    // Geneva.
    ("switzerlandwest", 46.20, 6.14),
    // Taipei.
    ("taiwannorth", 25.03, 121.55),
    // Taipei.
    ("taiwannorthwest", 25.03, 121.55),
    // Abu Dhabi.
    ("uaecentral", 24.45, 54.38),
    // Dubai.
    ("uaenorth", 25.20, 55.27),
    // London.
    ("uksouth", 51.51, -0.13),
    // Cardiff.
    ("ukwest", 51.48, -3.18),
    // Wyoming.
    ("westcentralus", 42.87, -106.31),
    // Netherlands.
    ("westeurope", 52.37, 4.90),
    // Mumbai.
    ("westindia", 19.08, 72.88),
    // California.
    ("westus", 37.78, -122.42),
    // Quincy, Washington.
    ("westus2", 47.23, -119.85),
    // Phoenix.
    ("westus3", 33.45, -112.07),
];

/// Where the named region sits, when the table knows it.
#[must_use]
pub fn of(region: &str) -> Option<RegionLocation> {
    LOCATIONS
        .iter()
        .find(|(name, ..)| *name == region)
        .map(|(_, latitude, longitude)| RegionLocation {
            latitude: *latitude,
            longitude: *longitude,
        })
}

#[cfg(test)]
mod tests {
    use super::{LOCATIONS, of};
    use crate::azure::DEFAULT_CANDIDATE_REGIONS;

    #[test]
    fn every_region_flyco_can_emit_sits_somewhere() {
        // A region the provider can offer without a table row catalogs an
        // entry that can never be the nearest — the exact silent failure
        // the key exists to prevent.
        for region in DEFAULT_CANDIDATE_REGIONS {
            assert!(of(region).is_some(), "{region} has no row");
        }
    }

    #[test]
    fn a_region_the_table_does_not_know_resolves_to_none() {
        // Not a refusal: the entry still catalogs, it just claims no
        // place — a region Azure opens tomorrow is news, not a bug.
        assert_eq!(of("atlantis"), None);
    }

    #[test]
    fn the_table_never_spells_a_region_twice() {
        let mut names = LOCATIONS.iter().map(|(name, ..)| *name).collect::<Vec<_>>();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), LOCATIONS.len());
    }
}
