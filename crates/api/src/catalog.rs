//! The cached machine catalog: what a linked account can deploy, read
//! outside the request that needs it.
//!
//! A cloud account's catalog is not a lookup. Azure lists around thirteen
//! hundred SKUs per region, then its quotas, then tens of pages of retail
//! prices, then the storage tiers — three regions of that took twenty to
//! thirty seconds each against a live subscription, was throttled by
//! `prices.azure.com`, and finally exceeded the Worker's CPU limit, which
//! kills the isolate and takes every other request in flight down with it.
//! So it is not computed in a request, ever.
//!
//! Instead each account has one document in KV, written by the provisioning
//! queue and only read on the request path. The document is per region,
//! because that is the unit one queue message can finish: a region that
//! answers is served while its neighbour is still being read, and a region
//! the provider refuses is a hole with a reason in it rather than a failed
//! catalog.
//!
//! # What "no machines" means
//!
//! An account with no document has not been read yet, which is a different
//! answer from an account that was read and offers nothing — the first
//! becomes machines in a few seconds and the second is a fact the user has
//! to act on. Nothing here collapses the two: a caller is told which
//! accounts are still pending, and [`crate::machines`] carries that all the
//! way out to the API.
//!
//! # Hardware the user owns is not cached
//!
//! A host account's catalog is a pure function of the `hosts` row the same
//! request already loaded — no I/O, nothing to move off the request path —
//! and its inputs change on the scale of a heartbeat. Serving it from a
//! six-hour cache would offer a machine that went offline hours ago, which
//! is the one thing `provisioning::catalog_of` refuses to do. So only
//! accounts whose catalog is a provider read are cached, and only those can
//! be pending.

use flyco_core::{MachineCatalogEntry, ProviderAccountId, UserId};
use serde::{Deserialize, Serialize};
use skyzen::sql;
use skyzen_services::{Db, Kv, Queue};

use crate::error::ApiError;
use crate::expiring;
use crate::provisioning_queue::{self, ProvisioningJob};

/// How long a region's entries are served before they are read again.
///
/// The same six hours the retail-prices cache uses, because the prices are
/// the part of an entry that moves: spot meters are republished monthly and
/// SKU availability changes slower still.
pub const TTL_SECONDS: u64 = flyco_provider::azure::pricing::CACHE_TTL_SECONDS;

/// How long a failed read is served before it is tried again.
///
/// Much shorter than [`TTL_SECONDS`], because a failure is usually
/// transient — a throttle, an expired token, a region the provider was
/// having a bad day in — and six hours of "this account offers nothing" for
/// a network blip the user cannot see would be indistinguishable from a
/// broken account.
pub const FAILURE_TTL_SECONDS: u64 = 15 * 60;

/// How long one asked-for refresh stops another being asked for.
///
/// The composer polls every five seconds while an account is pending and
/// the scheduled worker sweeps every minute; without this, one unread
/// account would put a message on the queue for every one of those. Ten
/// minutes is longer than any refresh takes and short enough that a refresh
/// whose message was lost is asked for again while the user is still
/// looking at the screen.
pub const REFRESH_CLAIM_SECONDS: u64 = 10 * 60;

/// KV key holding one account's catalog document.
#[must_use]
pub fn document_key(account: ProviderAccountId) -> String {
    format!("catalog:{account}")
}

/// KV key marking a refresh of this account as already asked for.
#[must_use]
fn claim_key(account: ProviderAccountId) -> String {
    format!("catalog-refresh:{account}")
}

/// The document layout a reader serves.
///
/// Bumped when a field is added that an older document cannot honestly
/// default — `MachineCatalogEntry::location` is the first: a document
/// written without it deserializes every entry's location as `None`,
/// which reads as "the provider cannot say where the region is" about
/// regions it can. A document under another version is served as absent
/// rather than as answers it never wrote, and the account goes down the
/// same refresh path a freshly linked one takes.
pub const DOCUMENT_VERSION: u8 = 1;

/// What one linked account can deploy, as the last read left it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogDocument {
    /// The [`DOCUMENT_VERSION`] this document was written under —
    /// `#[serde(default)]` so a document from before the field existed
    /// reads as version zero and is served as absent rather than parsed
    /// into answers it cannot contain.
    #[serde(default)]
    pub version: u8,
    /// When any part of this document was last written, in seconds since
    /// the Unix epoch.
    ///
    /// The document's own age, so a reader can say how stale the answer is
    /// without inspecting every region.
    pub read_at_unix: u64,
    /// One entry per region that has been read, in the order they were
    /// first written.
    pub regions: Vec<RegionCatalog>,
    /// Why the account as a whole could not be read, when the last attempt
    /// failed before it reached any region.
    ///
    /// Separate from a region's own failure: this is "the credential was
    /// refused" or "the subscription's policy could not be read", which is
    /// about the account rather than about a place in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

impl Default for CatalogDocument {
    /// An empty document claims the current layout: `record_region` builds
    /// one with `unwrap_or_default` to write into, and a default that
    /// claimed an older version would be served as absent on the next
    /// read and never land.
    fn default() -> Self {
        Self {
            version: DOCUMENT_VERSION,
            read_at_unix: 0,
            regions: Vec::new(),
            failure: None,
        }
    }
}

/// One region of an account's catalog, and when it was read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegionCatalog {
    /// The provider-native region this covers.
    pub region: String,
    /// When this region was read, in seconds since the Unix epoch.
    pub read_at_unix: u64,
    /// What the read produced.
    pub outcome: RegionOutcome,
}

/// What reading one region produced.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RegionOutcome {
    /// The machine types the account can deploy there.
    Offered {
        /// The entries, already stamped with the account they came from.
        entries: Vec<MachineCatalogEntry>,
    },
    /// The provider refused this region, and said this.
    Failed {
        /// What the provider said, for the log and for a later diagnosis.
        error: String,
    },
}

impl CatalogDocument {
    /// Every entry this document offers, across every region that answered.
    pub fn entries(&self) -> impl Iterator<Item = &MachineCatalogEntry> {
        self.regions
            .iter()
            .filter_map(|region| match &region.outcome {
                RegionOutcome::Offered { entries } => Some(entries.iter()),
                RegionOutcome::Failed { .. } => None,
            })
            .flatten()
    }

    /// Whether this document is due to be read again at `now_unix`.
    ///
    /// A failure ages out much faster than an answer, and an empty document
    /// — a fan-out that has been asked for but whose regions have not
    /// landed — is judged by the moment it was written, so a fan-out that
    /// went missing is asked for again rather than standing for ever.
    #[must_use]
    pub fn is_stale(&self, now_unix: u64) -> bool {
        if self.failure.is_some() {
            return now_unix.saturating_sub(self.read_at_unix) >= FAILURE_TTL_SECONDS;
        }
        let oldest = self
            .regions
            .iter()
            .map(|region| region.read_at_unix)
            .min()
            .unwrap_or(self.read_at_unix);
        now_unix.saturating_sub(oldest) >= TTL_SECONDS
    }
}

/// Reads one account's document, treating an expired one as absent.
///
/// # Errors
///
/// Returns [`ApiError::Kv`] if the store fails or the stored bytes are not
/// a catalog document.
pub async fn read(
    kv: &Kv,
    account: ProviderAccountId,
) -> Result<Option<CatalogDocument>, ApiError> {
    let document: Option<CatalogDocument> = expiring::get(kv, &document_key(account)).await?;
    // A document under another layout is absent, whatever it says: the
    // fields it never recorded deserialize as defaults, and a defaulted
    // answer is still an answer — every entry of a pre-location document
    // reads as unplaced, which is the geography-blind ordering the field
    // exists to replace.
    Ok(document.filter(|document| document.version == DOCUMENT_VERSION))
}

/// Writes one account's document back, with its life renewed.
async fn write(
    kv: &Kv,
    account: ProviderAccountId,
    document: &CatalogDocument,
) -> Result<(), ApiError> {
    expiring::put(kv, &document_key(account), document, TTL_SECONDS).await?;
    Ok(())
}

/// Records one region's outcome, leaving every other region alone.
///
/// The whole point of the per-region shape: a region that answered stays
/// served while the next message reads the one beside it.
///
/// # Errors
///
/// Returns [`ApiError::Kv`] if the store refuses the read or the write.
pub async fn record_region(
    kv: &Kv,
    account: ProviderAccountId,
    region: RegionCatalog,
) -> Result<(), ApiError> {
    let mut document = read(kv, account).await?.unwrap_or_default();
    document.read_at_unix = region.read_at_unix;
    // A successful read of any part of the account answers the question the
    // account-level failure was recording, so it is cleared.
    document.failure = None;
    match document
        .regions
        .iter_mut()
        .find(|held| held.region == region.region)
    {
        Some(held) => *held = region,
        None => document.regions.push(region),
    }
    write(kv, account, &document).await
}

/// Replaces the whole document with one read of the whole account.
///
/// For the providers whose catalog is a single call rather than a call per
/// region: what comes back *is* the account's whole answer, so anything the
/// document held before it is superseded rather than merged with.
///
/// # Errors
///
/// Returns [`ApiError::Kv`] if the store refuses the write.
pub async fn record_account(
    kv: &Kv,
    account: ProviderAccountId,
    entries: Vec<MachineCatalogEntry>,
    read_at_unix: u64,
) -> Result<(), ApiError> {
    let mut regions: Vec<RegionCatalog> = Vec::new();
    for entry in entries {
        match regions.iter_mut().find(|held| held.region == entry.region) {
            Some(RegionCatalog {
                outcome: RegionOutcome::Offered { entries },
                ..
            }) => entries.push(entry),
            // Nothing this function writes is a failure, so the arm below
            // is unreachable in practice; it is written as a push rather
            // than as an `unreachable!` because a panic here would take
            // down a queue consumer over a shape it can express honestly.
            Some(_) | None => regions.push(RegionCatalog {
                region: entry.region.clone(),
                read_at_unix,
                outcome: RegionOutcome::Offered {
                    entries: vec![entry],
                },
            }),
        }
    }

    write(
        kv,
        account,
        &CatalogDocument {
            version: DOCUMENT_VERSION,
            read_at_unix,
            regions,
            failure: None,
        },
    )
    .await
}

/// Records that the account itself could not be read.
///
/// The document is kept rather than deleted, because "read, and it refused"
/// is an answer: the account stops being *pending* and stops being asked
/// for every five seconds, and [`FAILURE_TTL_SECONDS`] is what brings the
/// question back.
///
/// # Errors
///
/// Returns [`ApiError::Kv`] if the store refuses the write.
pub async fn record_failure(
    kv: &Kv,
    account: ProviderAccountId,
    error: String,
    read_at_unix: u64,
) -> Result<(), ApiError> {
    write(
        kv,
        account,
        &CatalogDocument {
            version: DOCUMENT_VERSION,
            read_at_unix,
            regions: Vec::new(),
            failure: Some(error),
        },
    )
    .await
}

/// Asks for a refresh of one account, unless one was asked for recently.
///
/// Answers whether a job was enqueued, which is what the caller logs. The
/// claim is taken before the message is sent, because a message sent before
/// the claim would let a burst of pollers enqueue a burst of identical
/// refreshes. A send that then fails gives the claim back: otherwise every
/// poller for the next [`REFRESH_CLAIM_SECONDS`] would be told a refresh is
/// under way when nothing was ever queued.
///
/// # Errors
///
/// Returns [`ApiError`] if the store or the queue refuses.
pub async fn ask_for_refresh(
    kv: &Kv,
    queue: &Queue,
    user: UserId,
    account: ProviderAccountId,
) -> Result<bool, ApiError> {
    let key = claim_key(account);
    if expiring::get::<()>(kv, &key).await?.is_some() {
        return Ok(false);
    }
    expiring::put(kv, &key, &(), REFRESH_CLAIM_SECONDS).await?;
    if let Err(refused) =
        provisioning_queue::enqueue(queue, ProvisioningJob::RefreshCatalog { user, account }).await
    {
        expiring::take::<()>(kv, &key).await?;
        return Err(refused);
    }
    Ok(true)
}

/// One linked cloud account, as the scheduled sweep reads it.
#[derive(Debug, skyzen::FromRow)]
struct AccountRow {
    id: ProviderAccountId,
    user_id: UserId,
}

/// Asks for a refresh of every cloud account whose document is missing or
/// stale.
///
/// The scheduled leg, and the only thing that keeps a catalog current for a
/// user who is not looking at it. It rides the existing minute cron rather
/// than adding a second one, and it is cheap because [`ask_for_refresh`]
/// refuses to ask twice inside [`REFRESH_CLAIM_SECONDS`].
///
/// Host accounts are excluded in SQL: their catalog is never cached, so a
/// refresh would have nothing to write.
///
/// # Errors
///
/// Returns [`ApiError`] if the database, the store, or the queue fails.
pub async fn refresh_stale(db: &Db, kv: &Kv, queue: &Queue, at_unix: u64) -> Result<(), ApiError> {
    let host = flyco_core::CloudProviderKind::Host;
    let rows: Vec<AccountRow> = sql!(
        db,
        "SELECT id, user_id FROM provider_accounts \
         WHERE unlinked_at_unix IS NULL AND kind != {host}"
    )
    .fetch_all()
    .await?;

    for row in rows {
        let due = read(kv, row.id)
            .await?
            .is_none_or(|document| document.is_stale(at_unix));
        if due && ask_for_refresh(kv, queue, row.user_id, row.id).await? {
            tracing::info!(account = %row.id, "asked for a scheduled catalog refresh");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use flyco_core::{
        CloudProviderKind, MachineCatalogEntry, MachinePricing, OsFamily, ProviderAccountId,
        Runtime, Usd,
    };
    use skyzen_services::Kv;
    use skyzen_test::mock::InMemoryKv;

    use super::{
        CatalogDocument, DOCUMENT_VERSION, FAILURE_TTL_SECONDS, RegionCatalog, RegionOutcome,
        TTL_SECONDS, document_key, read, record_account, record_failure, record_region,
    };

    fn entry(region: &str, machine_type: &str) -> MachineCatalogEntry {
        MachineCatalogEntry {
            account: None,
            provider: CloudProviderKind::Azure,
            region: region.to_owned(),
            location: None,
            machine_type: machine_type.to_owned(),
            runtime: Runtime::Vm,
            free_grant: None,
            os: OsFamily::Linux,
            capacity: None,
            lineage: None,
            pricing: MachinePricing::Metered {
                on_demand_hourly: Usd::from_micros(1_000),
                spot_hourly: None,
                minimum: None,
                storage: flyco_core::StoragePricing::CapacityTiers { tiers: Vec::new() },
            },
        }
    }

    fn offered(region: &str, machine_type: &str, read_at_unix: u64) -> RegionCatalog {
        RegionCatalog {
            region: region.to_owned(),
            read_at_unix,
            outcome: RegionOutcome::Offered {
                entries: vec![entry(region, machine_type)],
            },
        }
    }

    fn store() -> Kv {
        Kv::new(InMemoryKv::new())
    }

    #[skyzen::test]
    async fn a_region_lands_beside_the_ones_already_read() {
        let kv = store();
        let account = ProviderAccountId::generate();

        record_region(&kv, account, offered("eastus", "Standard_D2als_v6", 100))
            .await
            .expect("first region");
        record_region(
            &kv,
            account,
            offered("westeurope", "Standard_D4als_v6", 200),
        )
        .await
        .expect("second region");

        let document = read(&kv, account).await.expect("read").expect("a document");
        assert_eq!(
            document
                .entries()
                .map(|entry| entry.machine_type.as_str())
                .collect::<Vec<_>>(),
            vec!["Standard_D2als_v6", "Standard_D4als_v6"],
            "a second region is merged in, not written over the first"
        );
        assert_eq!(document.read_at_unix, 200);
    }

    #[skyzen::test]
    async fn reading_a_region_again_replaces_that_regions_answer() {
        let kv = store();
        let account = ProviderAccountId::generate();

        record_region(&kv, account, offered("eastus", "Standard_D2als_v6", 100))
            .await
            .expect("first read");
        record_region(&kv, account, offered("eastus", "Standard_D8als_v6", 200))
            .await
            .expect("second read");

        let document = read(&kv, account).await.expect("read").expect("a document");
        assert_eq!(document.regions.len(), 1);
        assert_eq!(
            document
                .entries()
                .map(|entry| entry.machine_type.as_str())
                .collect::<Vec<_>>(),
            vec!["Standard_D8als_v6"]
        );
    }

    #[skyzen::test]
    async fn a_refused_region_is_a_hole_and_not_a_failed_catalog() {
        let kv = store();
        let account = ProviderAccountId::generate();

        record_region(&kv, account, offered("eastus", "Standard_D2als_v6", 100))
            .await
            .expect("a region that answered");
        record_region(
            &kv,
            account,
            RegionCatalog {
                region: "westeurope".to_owned(),
                read_at_unix: 100,
                outcome: RegionOutcome::Failed {
                    error: "throttled".to_owned(),
                },
            },
        )
        .await
        .expect("a region that refused");

        let document = read(&kv, account).await.expect("read").expect("a document");
        assert_eq!(
            document.entries().count(),
            1,
            "the good region still serves"
        );
    }

    #[skyzen::test]
    async fn a_whole_account_read_groups_its_entries_by_region() {
        let kv = store();
        let account = ProviderAccountId::generate();

        record_account(
            &kv,
            account,
            vec![
                entry("us-east-1", "t4g.large"),
                entry("eu-west-1", "t4g.xlarge"),
                entry("us-east-1", "m7g.large"),
            ],
            100,
        )
        .await
        .expect("record");

        let document = read(&kv, account).await.expect("read").expect("a document");
        assert_eq!(document.regions.len(), 2);
        assert_eq!(document.entries().count(), 3);
    }

    #[skyzen::test]
    async fn a_successful_region_clears_an_account_level_failure() {
        let kv = store();
        let account = ProviderAccountId::generate();

        record_failure(&kv, account, "the credential was refused".to_owned(), 100)
            .await
            .expect("record a failure");
        record_region(&kv, account, offered("eastus", "Standard_D2als_v6", 200))
            .await
            .expect("record a region");

        let document = read(&kv, account).await.expect("read").expect("a document");
        assert!(document.failure.is_none());
    }

    #[test]
    fn a_failure_is_retried_far_sooner_than_an_answer() {
        let answered = CatalogDocument {
            version: DOCUMENT_VERSION,
            read_at_unix: 0,
            regions: vec![offered("eastus", "Standard_D2als_v6", 0)],
            failure: None,
        };
        assert!(!answered.is_stale(TTL_SECONDS - 1));
        assert!(answered.is_stale(TTL_SECONDS));

        let refused = CatalogDocument {
            version: DOCUMENT_VERSION,
            read_at_unix: 0,
            regions: Vec::new(),
            failure: Some("the credential was refused".to_owned()),
        };
        assert!(!refused.is_stale(FAILURE_TTL_SECONDS - 1));
        assert!(refused.is_stale(FAILURE_TTL_SECONDS));
    }

    #[test]
    fn a_documents_age_is_its_oldest_region() {
        let document = CatalogDocument {
            version: DOCUMENT_VERSION,
            read_at_unix: TTL_SECONDS,
            regions: vec![
                offered("eastus", "Standard_D2als_v6", 0),
                offered("westeurope", "Standard_D4als_v6", TTL_SECONDS),
            ],
            failure: None,
        };
        assert!(
            document.is_stale(TTL_SECONDS),
            "one region read six hours ago makes the document due, however \
             recently its neighbour was written"
        );
    }

    #[skyzen::test]
    async fn a_document_from_before_this_layout_is_served_as_absent() {
        // What every account's row looks like until the first refresh
        // after a layout change: the same regions, written without the
        // field that version was added for. Served as written, each of
        // its entries would answer `location: None` — the ordering the
        // field exists to replace — so the reader serves absent instead
        // and the account refreshes.
        let kv = store();
        let account = ProviderAccountId::generate();
        let before = serde_json::json!({
            "read_at_unix": crate::clock::now_unix(),
            "regions": [{
                "region": "UsEast",
                "read_at_unix": crate::clock::now_unix(),
                "outcome": {"outcome": "offered", "entries": []},
            }],
        });
        crate::expiring::put(&kv, &document_key(account), &before, TTL_SECONDS)
            .await
            .expect("a pre-version document stores");

        assert!(read(&kv, account).await.expect("read").is_none());
    }
}
