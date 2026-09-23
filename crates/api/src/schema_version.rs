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

use skyzen::durable::DurableObjectError;
use skyzen_services::DurableDb;

/// The version table's own DDL. `CHECK (id = 0)` keeps it one row the
/// same way the presence tables are kept one row.
const META: &str = "CREATE TABLE IF NOT EXISTS schema_meta (\
     id      INTEGER PRIMARY KEY CHECK (id = 0), \
     version INTEGER NOT NULL)";

/// Runs `ddl` once per version bump, then records `expected` as the
/// object's schema version.
///
/// The meta table's `CREATE` runs on every call — it is what makes the
/// version read safe — and the version row is the skip: an object already
/// at `expected` answers after one create and one select rather than
/// re-running every statement it was built with. An object built before
/// the version lived in a table has no row, so its DDL — every statement
/// `IF NOT EXISTS` — runs once more and the row lands.
///
/// # Errors
///
/// Returns [`DurableObjectError`] if the version table or any `ddl`
/// statement fails.
pub async fn ensure(
    db: &DurableDb,
    expected: i64,
    ddl: &[&'static str],
) -> Result<(), DurableObjectError> {
    db.query(META)
        .execute()
        .await
        .map_err(|error| DurableObjectError::Runtime(error.to_string()))?;
    let version: Option<i64> = db
        .query("SELECT version FROM schema_meta WHERE id = 0")
        .fetch_scalar_optional()
        .await
        .map_err(|error| DurableObjectError::Runtime(error.to_string()))?;
    if version.is_some_and(|version| version >= expected) {
        return Ok(());
    }
    for statement in ddl {
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
    Ok(())
}
