//! Where each GCP region sits.
//!
//! Google publishes region and zone names but no coordinates for them, so
//! the geography is written down here, one row per public region, as the
//! [`RegionLocation`] a catalog entry stamps. The coordinates are the city
//! each region is documented to sit in; they rank regions by distance to
//! the caller, which a two-decimal position resolves as well as a surveyed
//! one.
//!
//! Compute entries name a *zone* (`us-central1-a`) where Cloud Run entries
//! name the region itself (`us-central1`), so the lookup takes either: a
//! trailing `-x` zone letter is stripped first. A name the table does not
//! know resolves to `None` rather than a guess.

use flyco_core::machine::RegionLocation;

/// `(region, latitude, longitude)` for every public GCP region.
const LOCATIONS: &[(&str, f64, f64)] = &[
    // Johannesburg.
    ("africa-south1", -26.20, 28.05),
    // Changhua County, Taiwan.
    ("asia-east1", 24.05, 120.52),
    // Hong Kong.
    ("asia-east2", 22.32, 114.17),
    // Tokyo.
    ("asia-northeast1", 35.68, 139.69),
    // Osaka.
    ("asia-northeast2", 34.69, 135.50),
    // Seoul.
    ("asia-northeast3", 37.57, 126.98),
    // Mumbai.
    ("asia-south1", 19.08, 72.88),
    // Delhi.
    ("asia-south2", 28.61, 77.21),
    // Singapore.
    ("asia-southeast1", 1.35, 103.82),
    // Jakarta.
    ("asia-southeast2", -6.21, 106.85),
    // Bangkok.
    ("asia-southeast3", 13.76, 100.50),
    // Kuala Lumpur.
    ("asia-southeast4", 3.139, 101.69),
    // Sydney.
    ("australia-southeast1", -33.87, 151.21),
    // Melbourne.
    ("australia-southeast2", -37.81, 144.96),
    // Warsaw.
    ("europe-central2", 52.23, 21.01),
    // Hamina, Finland.
    ("europe-north1", 60.57, 27.19),
    // Stockholm.
    ("europe-north2", 59.33, 18.07),
    // Madrid.
    ("europe-southwest1", 40.42, -3.70),
    // St. Ghislain, Belgium.
    ("europe-west1", 50.47, 3.82),
    // London.
    ("europe-west2", 51.51, -0.13),
    // Frankfurt.
    ("europe-west3", 50.11, 8.68),
    // Eemshaven, Netherlands.
    ("europe-west4", 53.44, 6.84),
    // Zurich.
    ("europe-west6", 47.38, 8.54),
    // Milan.
    ("europe-west8", 45.46, 9.19),
    // Paris.
    ("europe-west9", 48.86, 2.35),
    // Berlin.
    ("europe-west10", 52.52, 13.40),
    // Turin.
    ("europe-west12", 45.07, 7.69),
    // Doha.
    ("me-central1", 25.29, 51.53),
    // Dammam.
    ("me-central2", 26.43, 50.10),
    // Tel Aviv.
    ("me-west1", 32.08, 34.78),
    // Montreal.
    ("northamerica-northeast1", 45.50, -73.57),
    // Toronto.
    ("northamerica-northeast2", 43.65, -79.38),
    // Querétaro.
    ("northamerica-south1", 20.59, -100.39),
    // São Paulo.
    ("southamerica-east1", -23.55, -46.63),
    // Santiago.
    ("southamerica-west1", -33.45, -70.67),
    // Council Bluffs, Iowa.
    ("us-central1", 41.26, -95.86),
    // Pryor, Oklahoma.
    ("us-central2", 36.31, -95.32),
    // Moncks Corner, South Carolina.
    ("us-east1", 33.19, -79.99),
    // Ashburn, Virginia.
    ("us-east4", 39.04, -77.49),
    // Columbus, Ohio.
    ("us-east5", 39.96, -82.99),
    // Jackson County, Alabama.
    ("us-east7", 34.95, -85.72),
    // Dallas.
    ("us-south1", 32.78, -96.80),
    // The Dalles, Oregon.
    ("us-west1", 45.59, -121.18),
    // Los Angeles.
    ("us-west2", 34.05, -118.24),
    // Salt Lake City.
    ("us-west3", 40.76, -111.89),
    // Las Vegas.
    ("us-west4", 36.17, -115.14),
    // Phoenix.
    ("us-west8", 33.45, -112.07),
];

/// Where the named region — or the region a named zone belongs to — sits,
/// when the table knows it.
#[must_use]
pub fn of(region_or_zone: &str) -> Option<RegionLocation> {
    // A Compute Engine zone is its region plus one letter (`us-central1-a`);
    // the strip mirrors `compute::region_of` without the error it returns,
    // because an unlocatable name is `None` here rather than a refusal.
    let name = region_or_zone
        .rsplit_once('-')
        .filter(|(region, suffix)| !region.is_empty() && suffix.len() == 1)
        .map_or(region_or_zone, |(region, _)| region);
    LOCATIONS
        .iter()
        .find(|(region, ..)| *region == name)
        .map(|(_, latitude, longitude)| RegionLocation {
            latitude: *latitude,
            longitude: *longitude,
        })
}

#[cfg(test)]
mod tests {
    use super::{LOCATIONS, of};
    use crate::gcp::DEFAULT_CANDIDATE_ZONES;
    use crate::gcp::run::{TIER_ONE_REGIONS, TIER_TWO_REGIONS};

    #[test]
    fn every_region_flyco_can_emit_sits_somewhere() {
        // Compute Engine emits the candidate zones; Cloud Run emits every
        // region its two price tiers publish. A region either service can
        // offer without a table row catalogs an entry that can never be
        // the nearest.
        for zone in DEFAULT_CANDIDATE_ZONES {
            assert!(of(zone).is_some(), "{zone} has no row");
        }
        for region in TIER_ONE_REGIONS.iter().chain(&TIER_TWO_REGIONS) {
            assert!(of(region).is_some(), "{region} has no row");
        }
    }

    #[test]
    fn a_zone_letter_is_stripped_but_a_region_name_is_not_a_zone() {
        // `us-central1-a` is the Iowa region; `us-central1` is not a zone
        // of `us-central` and must not be read as one.
        let zone = of("us-central1-a").expect("the zone resolves");
        let region = of("us-central1").expect("the region resolves");
        assert_eq!(zone, region);
        assert_eq!(of("us-central"), None);
    }

    #[test]
    fn the_table_never_spells_a_region_twice() {
        let mut names = LOCATIONS.iter().map(|(name, ..)| *name).collect::<Vec<_>>();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), LOCATIONS.len());
    }
}
