//! Uncommitted checkout snapshots in object storage.
//!
//! An automatic archive releases the disk, so anything the agent had not
//! committed has to live here or it is gone. Manual archives never write
//! one: the user confirmed discarding the work.

use skyzen_services::Storage;

use crate::error::ApiError;

/// Prefix every workdir snapshot lives under.
const ROOT: &str = "workdirs";

/// Media type of a stored `git diff --cached --binary` patch.
pub const CONTENT_TYPE: &str = "application/octet-stream";

fn key(session: flyco_core::SessionId) -> String {
    format!("{ROOT}/{session}/uncommitted.patch")
}

/// Stores (or replaces) the uncommitted diff of one session.
///
/// # Errors
///
/// Returns [`ApiError::Storage`] if the store fails.
pub async fn put(
    storage: &Storage,
    session: flyco_core::SessionId,
    patch: Vec<u8>,
) -> Result<(), ApiError> {
    storage.put(&key(session), patch).await?;
    tracing::info!(%session, "stored an uncommitted workdir snapshot");
    Ok(())
}

/// Reads the uncommitted diff of one session, if an automatic archive
/// stored one.
///
/// # Errors
///
/// Returns [`ApiError::Storage`] if the store fails.
pub async fn get(
    storage: &Storage,
    session: flyco_core::SessionId,
) -> Result<Option<Vec<u8>>, ApiError> {
    match storage.get(&key(session)).await? {
        Some(object) => Ok(Some(object.body)),
        None => Ok(None),
    }
}
