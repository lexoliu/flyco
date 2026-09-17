//! Where a durable object keeps the version of the schema its tables were
//! built under — the durable answer to "is the schema already there".
//!
//! `PRAGMA user_version` used to be this store: reading it cost one
//! storage read where running every `CREATE` blind costs one per
//! statement. Cloudflare's SQLite authorizer admits only a fixed list of
//! pragmas and `user_version` stopped being one of them — reading or
//! writing it began answering `SQLITE_AUTH`, and every object that
//! checked its schema through it went down at once. The version now
//! lives in an ordinary table no authorizer decision can take away.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use skyzen::durable::DurableObjectError;
use skyzen_services::DurableDb;

/// What an object remembers about its own schema: the highest version it
/// has already verified.
///
/// The cell lives on the `#[durable_object]` struct itself — the value
/// skyzen keeps with the instance and round-trips through the object's
/// own storage — so a remembered version survives the event that produced
/// it, and survives eviction exactly as long as the tables it describes
/// do. `fetch` mounts a clone on every route as `State<Cache>`; the `Arc`
/// is what makes a handler's [`Cache::record`] land in the object's own
/// cell, where the next state write carries it.
///
/// Serialized as the bare version or `null` — the same shape the unit
/// structs this field joined already wrote, so an object built before the
/// cell existed reads back empty rather than failing.
#[derive(Debug, Clone)]
pub struct Cache(Arc<AtomicI64>);

/// What the cell holds before the first check lands. `i64::MIN` can never
/// name a real schema version, so "nothing verified" needs no second word.
const UNVERIFIED: i64 = i64::MIN;

impl Cache {
    /// The version this object has already verified, when one has been.
    fn verified(&self) -> Option<i64> {
        let version = self.0.load(Ordering::Relaxed);
        (version != UNVERIFIED).then_some(version)
    }

    /// Marks `version` verified: a call asking for it or anything lower
    /// answers from the cell alone.
    fn record(&self, version: i64) {
        self.0.fetch_max(version, Ordering::Relaxed);
    }
}

impl Default for Cache {
    fn default() -> Self {
        Self(Arc::new(AtomicI64::new(UNVERIFIED)))
    }
}

impl Serialize for Cache {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.verified().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Cache {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let version = Option::<i64>::deserialize(deserializer)?;
        Ok(Self(Arc::new(AtomicI64::new(
            version.unwrap_or(UNVERIFIED),
        ))))
    }
}

/// The version table's own DDL. `CHECK (id = 0)` keeps it one row the
/// same way the presence tables are kept one row.
const META: &str = "CREATE TABLE IF NOT EXISTS schema_meta (\
     id      INTEGER PRIMARY KEY CHECK (id = 0), \
     version INTEGER NOT NULL)";

/// Runs `statements` once per version bump, then records `expected` as
/// the object's schema version.
///
/// `cache` is the object's own remembered answer — the [`Cache`] field on
/// its `#[durable_object]` struct, handed to the request as
/// `State<Cache>`. A cache already holding a version at or above
/// `expected` returns without touching storage, which is what makes the
/// check once per object rather than once per call. A bumped `expected`
/// outruns the cached version, so the meta table's `CREATE` and the
/// version read run again — the read being what tells a build that is
/// genuinely new to this object from one that merely lost its cell — and
/// only a stored version still short of `expected` re-runs the statements
/// and lands the bump.
///
/// An object built before the version lived in a table has no row, so its
/// statements — each one idempotent (`IF NOT EXISTS`, `INSERT OR IGNORE`)
/// — run once more and the row lands.
///
/// # Errors
///
/// Returns [`DurableObjectError`] if the version table or any of
/// `statements` fails.
pub async fn ensure(
    db: &DurableDb,
    cache: &Cache,
    expected: i64,
    statements: &[&'static str],
) -> Result<(), DurableObjectError> {
    if cache.verified().is_some_and(|version| version >= expected) {
        return Ok(());
    }
    db.query(META)
        .execute()
        .await
        .map_err(|error| DurableObjectError::Runtime(error.to_string()))?;
    let version: Option<i64> = db
        .query("SELECT version FROM schema_meta WHERE id = 0")
        .fetch_scalar_optional()
        .await
        .map_err(|error| DurableObjectError::Runtime(error.to_string()))?;
    if let Some(version) = version
        && version >= expected
    {
        cache.record(version);
        return Ok(());
    }
    for statement in statements {
        db.query(statement)
            .execute()
            .await
            .map_err(|error| DurableObjectError::Runtime(error.to_string()))?;
    }
    db.query(&format!(
        "INSERT INTO schema_meta (id, version) VALUES (0, {expected}) \
         ON CONFLICT (id) DO UPDATE SET version = excluded.version"
    ))
    .execute()
    .await
    .map_err(|error| DurableObjectError::Runtime(error.to_string()))?;
    cache.record(expected);
    Ok(())
}
