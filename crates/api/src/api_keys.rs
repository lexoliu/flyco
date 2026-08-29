//! The `api_keys` table.
//!
//! API keys are what non-browser clients authenticate with. The plaintext
//! key exists exactly once, in the response that mints it; the row keeps
//! only its SHA-256, so the table is worthless to whoever steals it.

use flyco_core::{ApiKeyId, ApiKeySummary, CreatedApiKey, UserId};
use skyzen_services::Db;

use crate::clock::now_unix;
use crate::crypto::{prefixed_token, token_hash};
use crate::error::ApiError;

/// Marks a REST API key, so a leaked key is recognisable to secret scanners.
pub const TOKEN_PREFIX: &str = "fk_";

/// A key matched by its hash, and the user it authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyOwner {
    /// The key that matched.
    pub key_id: ApiKeyId,
    /// The user it belongs to.
    pub user_id: UserId,
}

#[derive(Debug, skyzen::FromRow)]
struct OwnerRow {
    id: ApiKeyId,
    user_id: UserId,
}

impl From<OwnerRow> for KeyOwner {
    fn from(row: OwnerRow) -> Self {
        Self {
            key_id: row.id,
            user_id: row.user_id,
        }
    }
}

#[derive(Debug, skyzen::FromRow)]
struct SummaryRow {
    id: ApiKeyId,
    label: String,
    created_at_unix: u64,
    last_used_unix: Option<u64>,
}

impl From<SummaryRow> for ApiKeySummary {
    fn from(row: SummaryRow) -> Self {
        Self {
            id: row.id,
            label: row.label,
            created_at_unix: row.created_at_unix,
            last_used_unix: row.last_used_unix,
        }
    }
}

/// Mints a key for `user_id` and stores its hash.
///
/// The returned [`CreatedApiKey::token`] is the only copy that will ever
/// exist.
///
/// # Errors
///
/// Returns [`ApiError`] if entropy is unavailable or the insert fails.
pub async fn create(db: &Db, user_id: UserId, label: String) -> Result<CreatedApiKey, ApiError> {
    let token = prefixed_token(TOKEN_PREFIX)?;
    let id = ApiKeyId::generate();
    let created_at_unix = now_unix();

    db.query(
        "INSERT INTO api_keys (id, user_id, token_hash, label, created_at_unix, last_used_unix) \
         VALUES (?, ?, ?, ?, ?, NULL)",
    )
    .bind(id)
    .bind(user_id)
    .bind(token_hash(&token))
    .bind(label.clone())
    .bind(created_at_unix)
    .execute()
    .await?;

    Ok(CreatedApiKey {
        id,
        label,
        token,
        created_at_unix,
    })
}

/// Lists a user's keys, oldest first.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails or a stored row is malformed.
pub async fn list(db: &Db, user_id: UserId) -> Result<Vec<ApiKeySummary>, ApiError> {
    let rows: Vec<SummaryRow> = db
        .query(
            "SELECT id, label, created_at_unix, last_used_unix FROM api_keys \
             WHERE user_id = ? ORDER BY created_at_unix, id",
        )
        .bind(user_id)
        .fetch_all()
        .await?;

    Ok(rows.into_iter().map(Into::into).collect())
}

/// Revokes one of `user_id`'s keys.
///
/// # Errors
///
/// Returns [`ApiError::ApiKeyNotFound`] if the key does not exist or belongs
/// to somebody else — the two cases are deliberately indistinguishable.
pub async fn revoke(db: &Db, user_id: UserId, key_id: ApiKeyId) -> Result<(), ApiError> {
    let result = db
        .query("DELETE FROM api_keys WHERE id = ? AND user_id = ?")
        .bind(key_id)
        .bind(user_id)
        .execute()
        .await?;

    if result.rows_written == 0 {
        return Err(ApiError::ApiKeyNotFound);
    }
    Ok(())
}

/// Finds the key whose hash matches a presented credential.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails or the stored row is malformed.
pub async fn find_by_token(db: &Db, presented: &str) -> Result<Option<KeyOwner>, ApiError> {
    let row: Option<OwnerRow> = db
        .query("SELECT id, user_id FROM api_keys WHERE token_hash = ?")
        .bind(token_hash(presented))
        .fetch_optional()
        .await?;

    Ok(row.map(Into::into))
}

/// Stamps a key as used, so a stale credential is visible in the key list.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn mark_used(db: &Db, key_id: ApiKeyId) -> Result<(), ApiError> {
    db.query("UPDATE api_keys SET last_used_unix = ? WHERE id = ?")
        .bind(now_unix())
        .bind(key_id)
        .execute()
        .await?;
    Ok(())
}
