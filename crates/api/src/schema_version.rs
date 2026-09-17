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

use skyzen::durable::DurableObjectError;
use skyzen_services::DurableDb;

/// What an activation remembers about its object's schema: the highest
/// version it has already verified.
///
/// The cell lives on the `#[durable_object]` struct, which skyzen rebuilds
/// from `Default` around every event — the objects keep every durable fact
/// in storage and opt out of the state blob with `PERSIST = false` — so
/// the memo is per activation: it is what lets the poll steps of a held
/// stream, and every call a handler makes after its first, answer without
/// a statement. `fetch` mounts a clone on every route as `State<Cache>`;
/// the `Arc` is what makes a handler's [`Cache::record`] land in the same
/// cell the next call in the activation reads.
///
/// Nothing is written to the blob because the objects that hold this went
/// through a release as unit structs, which read back from `null` alone: a
/// blob carrying a version would make a rollback of the Worker fail every
/// object that had served a request, and the durable answer is already in
/// the `schema_meta` table, one read away.
#[derive(Debug, Clone)]
pub struct Cache(Arc<AtomicI64>);

/// What the cell holds before the first check lands. `i64::MIN` can never
/// name a real schema version, so "nothing verified" needs no second word.
const UNVERIFIED: i64 = i64::MIN;

impl Cache {
    /// The version this activation has already verified, when one has been.
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

/// The version table's own DDL. `CHECK (id = 0)` keeps it one row the
/// same way the presence tables are kept one row.
const META: &str = "CREATE TABLE IF NOT EXISTS schema_meta (\
     id      INTEGER PRIMARY KEY CHECK (id = 0), \
     version INTEGER NOT NULL)";

/// The one read a warm activation costs.
const READ: &str = "SELECT version FROM schema_meta WHERE id = 0";

/// Runs `statements` once per version bump, then records `expected` as
/// the object's schema version.
///
/// `cache` is the activation's own remembered answer — the [`Cache`]
/// field on its `#[durable_object]` struct, handed to the request as
/// `State<Cache>`. A cache already holding a version at or above
/// `expected` returns without touching storage, which is what makes the
/// check once per activation rather than once per call.
///
/// An activation that remembers nothing reads the version row first, and
/// that read is the whole cost of every activation past an object's
/// first: the table is only created when the read fails, which is what
/// an object that has never been checked — or one built before the
/// version lived in a table — answers. A stored version at or above
/// `expected` is recorded and nothing else runs; a stored version short
/// of it, or no row, runs the statements — each one idempotent (`IF NOT
/// EXISTS`, `INSERT OR IGNORE`) — and lands the bump.
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
    let version = match read_version(db).await {
        Ok(version) => version,
        Err(error) => {
            // The table is not there to read: the object's first check.
            // Anything else fails the read that follows the create, and
            // that failure is the one reported.
            tracing::debug!(%error, "no schema version to read; creating the version table");
            db.query(META)
                .execute()
                .await
                .map_err(|error| DurableObjectError::Runtime(error.to_string()))?;
            read_version(db)
                .await
                .map_err(|error| DurableObjectError::Runtime(error.to_string()))?
        }
    };
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

/// The version the table records, `None` for a table with no row yet.
async fn read_version(db: &DurableDb) -> Result<Option<i64>, skyzen_services::DurableDbError> {
    db.query(READ).fetch_scalar_optional().await
}
