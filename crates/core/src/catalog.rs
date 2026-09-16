//! Curating a raw provider catalog into the list a person can read.
//!
//! A cloud's own catalog is thousands of rows and almost entirely
//! redundant: every family is sold in five or six generations, and the older
//! ones cost more for less. Shown whole it is unusable as a slider, unusable
//! as an agent's menu, and expensive as a prompt. Curation is the pure
//! function that turns it into the short, strictly-ordered list of docs/ux.md
//! §7.6 and §7.7, and it lives here rather than in a driver because it is the
//! same three rules for every provider:
//!
//! 1. **Newest generation only.** Within one account, region, OS,
//!    architecture and family, keep the newest generation the provider
//!    offers. A generation the provider still sells but has superseded is
//!    slower *and* dearer, so there is no request it is the right answer to.
//! 2. **Pareto frontier on price against capacity.** Drop every entry that
//!    costs at least as much as another entry with at least as many vCPUs
//!    and at least as much memory. What survives is exactly the set where
//!    paying more buys more.
//! 3. **Order by price.** Cheapest first, which is the order the slider's
//!    detents run in and the order an agent should read.
//!
//! Three dimensions are never compared across, and each is the same mistake
//! if it is: **architecture**, because an arm64 machine is not a cheaper
//! x86-64 one; **operating system**, because a Mac is not a large Linux box;
//! and **runtime**, because a container that loses its filesystem when it
//! stops is not a cheap virtual machine — it is a different bargain, and a
//! cheap container dominating a whole line-up of VMs would leave a user who
//! needs a disk with nothing to pick. They are grouping keys, not axes. So
//! is the account: two linked accounts are two bills and two sets of
//! credentials, and an account whose whole line-up was dominated by a
//! cheaper account's would vanish from a chooser whose entire job is to let
//! the user pick between them.
//!
//! Everything here is total and pure. Entries the provider published no size
//! or no price for — hardware the user owns is both — are never dropped:
//! curation removes machines that are known to be worse, and an unknown is
//! not a known-worse.

use crate::id::ProviderAccountId;
use crate::machine::{
    CloudProviderKind, CpuArchitecture, MachineCatalogEntry, MachinePricing, OsFamily, Runtime,
};
use crate::money::Usd;

/// The dimensions two entries must share before either can be said to be
/// better than the other.
///
/// A grouping key rather than a comparison, because every one of these is a
/// fact about *which machine this is* rather than about how good it is:
/// entries that disagree on any of them answer different questions.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Group {
    provider: CloudProviderKind,
    account: Option<ProviderAccountId>,
    region: String,
    os: OsFamily,
    architecture: Option<CpuArchitecture>,
    runtime: Runtime,
}

impl Group {
    fn of(entry: &MachineCatalogEntry) -> Self {
        Self {
            provider: entry.provider,
            account: entry.account,
            region: entry.region.clone(),
            os: entry.os,
            architecture: entry.lineage.as_ref().map(|lineage| lineage.architecture),
            runtime: entry.runtime,
        }
    }
}

/// Every entry's group, computed once.
///
/// Both rules ask "is this other entry even comparable to me", which is a
/// question about a string, an id and three enums. Rebuilding the key inside
/// the quadratic scan would clone every region name once per pair.
fn groups(entries: &[MachineCatalogEntry]) -> Vec<Group> {
    entries.iter().map(Group::of).collect()
}

/// What one entry offers, when the provider published enough to say.
///
/// Present only for an entry with both a size and a metered price; an entry
/// missing either cannot be ranked and is carried through untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Offer {
    hourly: Usd,
    vcpus: u32,
    memory_mib: u64,
}

impl Offer {
    /// Reads what an entry offers, on the on-demand meter.
    ///
    /// On-demand rather than spot because the spot price is a quote that
    /// moves and is absent on most types: a frontier computed against it
    /// would reorder itself between two reads of the same catalog, and the
    /// slider's detents would move under the user's thumb.
    fn of(entry: &MachineCatalogEntry) -> Option<Self> {
        let capacity = entry.capacity.as_ref()?;
        let MachinePricing::Metered {
            on_demand_hourly, ..
        } = entry.pricing
        else {
            return None;
        };
        Some(Self {
            hourly: on_demand_hourly,
            vcpus: capacity.vcpus,
            memory_mib: capacity.memory_mib,
        })
    }

    /// Whether `self` makes `other` pointless.
    ///
    /// At most the price, at least the size, and better in something — where
    /// "better in something" includes being the earlier name, so that two
    /// entries alike in every measurable way do not delete each other and
    /// leave the group empty.
    fn dominates(self, own_name: &str, other: Self, other_name: &str) -> bool {
        self.hourly <= other.hourly
            && self.vcpus >= other.vcpus
            && self.memory_mib >= other.memory_mib
            && (self.hourly < other.hourly
                || self.vcpus > other.vcpus
                || self.memory_mib > other.memory_mib
                || own_name < other_name)
    }
}

/// Applies the three rules of docs/ux.md §7.6 to a merged raw catalog.
///
/// Total: any input produces an output, and an entry is dropped only when
/// another entry in the same group is demonstrably at least as good. The
/// result is ordered cheapest first, with unpriced hardware the user owns at
/// the front — it costs nothing to run, which is as cheap as a machine gets.
#[must_use]
pub fn curate(entries: Vec<MachineCatalogEntry>) -> Vec<MachineCatalogEntry> {
    let entries = newest_generations(entries);
    let mut kept = undominated(entries);
    sort_by_price(&mut kept);
    kept
}

/// Rule 1: within a family, keep only the newest generation on offer.
///
/// An entry whose provider published no generation is kept whatever else is
/// in its family: "this family has a v6" is not a reason to hide a type the
/// provider never numbered, because nothing says the two are the same
/// machine.
fn newest_generations(entries: Vec<MachineCatalogEntry>) -> Vec<MachineCatalogEntry> {
    let keys = groups(&entries);
    let superseded: Vec<bool> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let Some(lineage) = entry.lineage.as_ref() else {
                return false;
            };
            let Some(generation) = lineage.generation else {
                return false;
            };
            entries.iter().enumerate().any(|(other, rival)| {
                other != index
                    && keys[other] == keys[index]
                    && rival.lineage.as_ref().is_some_and(|newer| {
                        newer.family == lineage.family
                            && newer.generation.is_some_and(|number| number > generation)
                    })
            })
        })
        .collect();

    entries
        .into_iter()
        .zip(superseded)
        .filter_map(|(entry, superseded)| (!superseded).then_some(entry))
        .collect()
}

/// Rule 2: keep only the entries nothing else in their group beats.
fn undominated(entries: Vec<MachineCatalogEntry>) -> Vec<MachineCatalogEntry> {
    // Offers are read once: the check is quadratic within a group, and
    // re-reading the price and size on every comparison would make it
    // quadratic in allocations too.
    let keys = groups(&entries);
    let offers: Vec<Option<Offer>> = entries.iter().map(Offer::of).collect();

    let beaten: Vec<bool> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let Some(offer) = offers[index] else {
                return false;
            };
            entries.iter().enumerate().any(|(other, candidate)| {
                other != index
                    && keys[other] == keys[index]
                    && offers[other].is_some_and(|rival| {
                        rival.dominates(&candidate.machine_type, offer, &entry.machine_type)
                    })
            })
        })
        .collect();

    entries
        .into_iter()
        .zip(beaten)
        .filter_map(|(entry, beaten)| (!beaten).then_some(entry))
        .collect()
}

/// Rule 3: cheapest first, and the same catalog twice in the same order.
///
/// Price alone is not a total order — a region routinely sells two sizes at
/// the same rate — so the tie is broken by size and then by name, which are
/// the two things a user reads next.
fn sort_by_price(entries: &mut [MachineCatalogEntry]) {
    entries.sort_by(|left, right| {
        let key = |entry: &MachineCatalogEntry| {
            let offer = Offer::of(entry);
            (
                // Hardware the user owns is metered by nobody, which sorts
                // ahead of every price rather than being priced at zero.
                u8::from(offer.is_some()),
                offer.map_or(Usd::ZERO, |offer| offer.hourly),
                offer.map_or(0, |offer| offer.vcpus),
                entry.region.clone(),
                entry.machine_type.clone(),
            )
        };
        key(left).cmp(&key(right))
    });
}

#[cfg(test)]
mod tests {

    use uuid::Uuid;

    use super::curate;
    use crate::id::ProviderAccountId;
    use crate::machine::{
        BillingMinimum, CloudProviderKind, CpuArchitecture, MachineCapacity, MachineCatalogEntry,
        MachineLineage, MachinePricing, OsFamily, Runtime, StoragePricing,
    };
    use crate::money::Usd;

    fn account(seed: u128) -> ProviderAccountId {
        ProviderAccountId::from_uuid(Uuid::from_u128(seed))
    }

    /// An Azure-shaped entry: a family, a generation, a size and a price.
    fn entry(
        machine_type: &str,
        family: &str,
        generation: Option<u32>,
        vcpus: u32,
        memory_gib: u64,
        cents: u64,
    ) -> MachineCatalogEntry {
        MachineCatalogEntry {
            provider: CloudProviderKind::Azure,
            account: Some(account(1)),
            region: "eastus".to_owned(),
            location: None,
            machine_type: machine_type.to_owned(),
            runtime: Runtime::Vm,
            free_grant: None,
            os: OsFamily::Linux,
            capacity: Some(MachineCapacity {
                vcpus,
                memory_mib: memory_gib * 1024,
            }),
            lineage: Some(MachineLineage {
                architecture: CpuArchitecture::X8664,
                family: family.to_owned(),
                generation,
            }),
            pricing: MachinePricing::Metered {
                on_demand_hourly: Usd::from_cents(cents),
                spot_hourly: Some(Usd::from_cents(cents / 2)),
                minimum: None,
                storage: StoragePricing::PerGibHourly {
                    rate: Usd::from_micros(100),
                },
            },
        }
    }

    fn names(entries: &[MachineCatalogEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| entry.machine_type.clone())
            .collect()
    }

    #[test]
    fn an_empty_catalog_curates_to_an_empty_catalog() {
        assert!(curate(Vec::new()).is_empty());
    }

    #[test]
    fn only_the_newest_generation_of_a_family_survives() {
        // The older generation is cheaper here, which is the point: it is
        // dropped for being superseded, before price is ever consulted.
        let catalog = vec![
            entry("D4s_v6", "ds", Some(6), 4, 16, 19),
            entry("D4s_v5", "ds", Some(5), 4, 16, 17),
            entry("D4s_v4", "ds", Some(4), 4, 16, 15),
        ];

        assert_eq!(names(&curate(catalog)), ["D4s_v6"]);
    }

    #[test]
    fn generations_are_counted_per_family_not_across_the_line_up() {
        let catalog = vec![
            entry("D4s_v6", "ds", Some(6), 4, 16, 19),
            entry("E4s_v5", "es", Some(5), 4, 32, 25),
            entry("E4s_v4", "es", Some(4), 4, 32, 23),
        ];

        // `es` has no v6, so its own newest survives beside `ds`'s.
        assert_eq!(names(&curate(catalog)), ["D4s_v6", "E4s_v5"]);
    }

    #[test]
    fn an_unnumbered_family_member_is_never_hidden_by_a_numbered_one() {
        let mut unnumbered = entry("M8ms", "ms", None, 8, 218, 90);
        unnumbered.lineage = Some(MachineLineage {
            architecture: CpuArchitecture::X8664,
            family: "ms".to_owned(),
            generation: None,
        });
        let catalog = vec![entry("M8ms_v3", "ms", Some(3), 8, 218, 95), unnumbered];

        // Both survive rule 1; rule 2 then drops the dearer of two identical
        // sizes, which is the numbered one.
        assert_eq!(names(&curate(catalog)), ["M8ms"]);
    }

    #[test]
    fn a_type_that_costs_more_for_no_more_machine_is_dropped() {
        let catalog = vec![
            entry("D4s_v6", "ds", Some(6), 4, 16, 19),
            entry("F4s_v6", "fs", Some(6), 4, 16, 21),
        ];

        assert_eq!(names(&curate(catalog)), ["D4s_v6"]);
    }

    #[test]
    fn paying_more_for_more_machine_survives() {
        let catalog = vec![
            entry("D4s_v6", "ds", Some(6), 4, 16, 19),
            entry("D8s_v6", "ds", Some(6), 8, 32, 38),
            entry("E4s_v6", "es", Some(6), 4, 32, 25),
        ];

        assert_eq!(names(&curate(catalog)), ["D4s_v6", "E4s_v6", "D8s_v6"]);
    }

    #[test]
    fn two_entries_alike_in_every_way_leave_exactly_one_behind() {
        // Mutual domination would delete both and empty the catalog, so the
        // tie is broken by name and the first survives.
        let catalog = vec![
            entry("D4s_v6", "ds", Some(6), 4, 16, 19),
            entry("D4as_v6", "das", Some(6), 4, 16, 19),
        ];

        assert_eq!(names(&curate(catalog)), ["D4as_v6"]);
    }

    #[test]
    fn architecture_is_a_grouping_key_and_never_an_axis() {
        let arm = MachineCatalogEntry {
            lineage: Some(MachineLineage {
                architecture: CpuArchitecture::Arm64,
                family: "dps".to_owned(),
                generation: Some(6),
            }),
            ..entry("D4ps_v6", "dps", Some(6), 4, 16, 15)
        };
        let catalog = vec![entry("D4s_v6", "ds", Some(6), 4, 16, 19), arm];

        // The Arm type is cheaper at the same size and still does not hide
        // the x86-64 one: they are different machines.
        assert_eq!(names(&curate(catalog)), ["D4ps_v6", "D4s_v6"]);
    }

    #[test]
    fn an_older_generation_of_another_architecture_is_kept() {
        let arm_old = MachineCatalogEntry {
            lineage: Some(MachineLineage {
                architecture: CpuArchitecture::Arm64,
                family: "ds".to_owned(),
                generation: Some(5),
            }),
            ..entry("D4ps_v5", "ds", Some(5), 4, 16, 15)
        };
        let catalog = vec![entry("D4s_v6", "ds", Some(6), 4, 16, 19), arm_old];

        // Same family string, different architecture: the x86-64 v6 does not
        // supersede an Arm v5, because there is no Arm v6 on offer.
        assert_eq!(names(&curate(catalog)), ["D4ps_v5", "D4s_v6"]);
    }

    #[test]
    fn regions_are_curated_independently_of_each_other() {
        let elsewhere = MachineCatalogEntry {
            region: "westeurope".to_owned(),
            location: None,
            ..entry("D4s_v6", "ds", Some(6), 4, 16, 25)
        };
        let catalog = vec![entry("D4s_v6", "ds", Some(6), 4, 16, 19), elsewhere];

        // The dearer region is not a worse machine; it is somewhere else.
        assert_eq!(curate(catalog).len(), 2);
    }

    #[test]
    fn accounts_are_curated_independently_of_each_other() {
        let second = MachineCatalogEntry {
            account: Some(account(2)),
            ..entry("D4s_v6", "ds", Some(6), 4, 16, 25)
        };
        let catalog = vec![entry("D4s_v6", "ds", Some(6), 4, 16, 19), second];

        let curated = curate(catalog);
        assert_eq!(curated.len(), 2);
        assert_eq!(
            curated
                .iter()
                .filter(|entry| entry.account == Some(account(2)))
                .count(),
            1
        );
    }

    #[test]
    fn an_operating_system_is_never_compared_against_another() {
        let mac = MachineCatalogEntry {
            os: OsFamily::MacOs,
            pricing: MachinePricing::Metered {
                on_demand_hourly: Usd::from_cents(65),
                spot_hourly: None,
                minimum: Some(BillingMinimum::new(24, Usd::from_cents(65))),
                storage: StoragePricing::PerGibHourly {
                    rate: Usd::from_micros(100),
                },
            },
            ..entry("mac2.metal", "mac", Some(2), 8, 16, 65)
        };
        let catalog = vec![entry("D8s_v6", "ds", Some(6), 8, 32, 38), mac];

        // The Linux type is cheaper *and* bigger and still does not hide the
        // Mac: nobody asking for macOS is served by a Linux box.
        let curated = curate(catalog);
        assert_eq!(names(&curated), ["D8s_v6", "mac2.metal"]);
        assert!(curated.iter().any(|entry| matches!(
            entry.pricing,
            MachinePricing::Metered {
                minimum: Some(BillingMinimum { hours: 24, charge }),
                ..
                // 24 hours of $0.65, which is what a Mac actually costs
            // to touch — not the $0.65 the hourly rate implies.
        } if charge == Usd::from_cents(65 * 24)
        )));
    }

    #[test]
    fn a_container_is_never_compared_against_a_virtual_machine() {
        // Cheaper and bigger, and it still does not hide the VM: a session
        // that needs a disk which survives a stop is not served by an
        // execution whose filesystem ends with it.
        let container = MachineCatalogEntry {
            runtime: Runtime::Container,
            ..entry("aca-8x16", "aca", None, 8, 16, 21)
        };
        let catalog = vec![entry("D4s_v6", "ds", Some(6), 4, 16, 38), container];

        assert_eq!(names(&curate(catalog)), ["aca-8x16", "D4s_v6"]);
    }

    #[test]
    fn hardware_the_user_owns_is_never_dropped_and_sorts_first() {
        let owned = MachineCatalogEntry {
            provider: CloudProviderKind::Host,
            account: Some(account(3)),
            region: "build.lexo.cool".to_owned(),
            location: None,
            machine_type: "build.lexo.cool".to_owned(),
            // A session on hardware the user owns is a Podman container, as
            // `flyco_provider::host` plans it.
            runtime: Runtime::Container,
            free_grant: None,
            os: OsFamily::Linux,
            capacity: None,
            lineage: None,
            pricing: MachinePricing::UserOwned,
        };
        let catalog = vec![entry("D4s_v6", "ds", Some(6), 4, 16, 19), owned];

        assert_eq!(names(&curate(catalog)), ["build.lexo.cool", "D4s_v6"]);
    }

    #[test]
    fn the_result_is_ordered_by_price() {
        let catalog = vec![
            entry("D16s_v6", "ds", Some(6), 16, 64, 76),
            entry("D4s_v6", "ds", Some(6), 4, 16, 19),
            entry("D8s_v6", "ds", Some(6), 8, 32, 38),
        ];

        assert_eq!(names(&curate(catalog)), ["D4s_v6", "D8s_v6", "D16s_v6"]);
    }

    #[test]
    fn curation_is_idempotent() {
        let catalog = vec![
            entry("D4s_v6", "ds", Some(6), 4, 16, 19),
            entry("D4s_v5", "ds", Some(5), 4, 16, 17),
            entry("E8s_v6", "es", Some(6), 8, 64, 50),
            entry("F4s_v6", "fs", Some(6), 4, 16, 21),
        ];

        let once = curate(catalog);
        let twice = curate(once.clone());
        assert_eq!(once, twice);
    }
}
