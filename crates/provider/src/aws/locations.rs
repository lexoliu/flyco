//! Where each AWS region sits.
//!
//! AWS publishes the region names but not their coordinates — the
//! `DescribeRegions` answer carries a name and an opt-in status and nothing
//! else — so the geography is written down here, one row per public region,
//! as the [`RegionLocation`] a catalog entry stamps. The coordinates are
//! the city each region is documented to sit in; they rank regions by
//! distance to the caller, which a two-decimal position resolves as well
//! as a surveyed one.
//!
//! A region the table does not name — one AWS has introduced since, or a
//! partition outside the public set — resolves to `None` rather than a
//! guess: the entry still catalogs, it just does not claim a place.

use flyco_core::machine::RegionLocation;

/// `(region, latitude, longitude)` for every public AWS region.
const LOCATIONS: &[(&str, f64, f64)] = &[
    // Northern Virginia.
    ("us-east-1", 38.95, -77.45),
    // Ohio.
    ("us-east-2", 39.96, -82.99),
    // Northern California.
    ("us-west-1", 37.35, -121.96),
    // Oregon.
    ("us-west-2", 45.84, -119.70),
    // Cape Town.
    ("af-south-1", -33.92, 18.42),
    // Hong Kong.
    ("ap-east-1", 22.32, 114.17),
    // Taipei.
    ("ap-east-2", 25.03, 121.55),
    // Hyderabad.
    ("ap-south-2", 17.38, 78.49),
    // Mumbai.
    ("ap-south-1", 19.08, 72.88),
    // Tokyo.
    ("ap-northeast-1", 35.68, 139.69),
    // Seoul.
    ("ap-northeast-2", 37.57, 126.98),
    // Osaka.
    ("ap-northeast-3", 34.69, 135.50),
    // Singapore.
    ("ap-southeast-1", 1.35, 103.82),
    // Sydney.
    ("ap-southeast-2", -33.87, 151.21),
    // Jakarta.
    ("ap-southeast-3", -6.21, 106.85),
    // Melbourne.
    ("ap-southeast-4", -37.81, 144.96),
    // Kuala Lumpur.
    ("ap-southeast-5", 3.139, 101.69),
    // Auckland.
    ("ap-southeast-6", -36.85, 174.76),
    // Bangkok.
    ("ap-southeast-7", 13.76, 100.50),
    // Montreal.
    ("ca-central-1", 45.50, -73.57),
    // Calgary.
    ("ca-west-1", 51.05, -114.07),
    // Frankfurt.
    ("eu-central-1", 50.11, 8.68),
    // Zurich.
    ("eu-central-2", 47.38, 8.54),
    // Stockholm.
    ("eu-north-1", 59.33, 18.07),
    // Milan.
    ("eu-south-1", 45.46, 9.19),
    // Zaragoza.
    ("eu-south-2", 41.65, -0.89),
    // Ireland.
    ("eu-west-1", 53.35, -6.26),
    // London.
    ("eu-west-2", 51.51, -0.13),
    // Paris.
    ("eu-west-3", 48.86, 2.35),
    // Tel Aviv.
    ("il-central-1", 32.08, 34.78),
    // Dubai.
    ("me-central-1", 25.20, 55.27),
    // Bahrain.
    ("me-south-1", 26.23, 50.58),
    // Querétaro.
    ("mx-central-1", 20.59, -100.39),
    // São Paulo.
    ("sa-east-1", -23.55, -46.63),
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
    use crate::aws::DEFAULT_CANDIDATE_REGIONS;

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
        // place — a region AWS opens tomorrow is news, not a bug.
        assert_eq!(of("us-nowhere-1"), None);
    }

    #[test]
    fn the_table_never_spells_a_region_twice() {
        let mut names = LOCATIONS.iter().map(|(name, ..)| *name).collect::<Vec<_>>();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), LOCATIONS.len());
    }
}
