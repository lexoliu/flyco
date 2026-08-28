//! The `users` table.
//!
//! One row per GitHub account, keyed by GitHub's immutable numeric id
//! because logins can be renamed and reused. The account's GitHub token is
//! sealed before it gets here and is never read back out on this path.

use flyco_core::{CurrentUser, UserId};
use serde::Deserialize;
use skyzen_services::Db;

use crate::clock::now_unix;
use crate::error::ApiError;
use crate::github::GithubUser;

/// The columns every read on this path projects.
#[derive(Debug, Deserialize)]
struct IdentityRow {
    id: String,
    login: String,
}

impl TryFrom<IdentityRow> for CurrentUser {
    type Error = ApiError;

    fn try_from(row: IdentityRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row
                .id
                .parse::<UserId>()
                .map_err(|_| ApiError::CorruptRecord("users.id is not a UUID"))?,
            login: row.login,
        })
    }
}

/// Records a completed GitHub sign-in, creating the account on first sight.
///
/// A returning user keeps their flyco [`UserId`] — everything else in the
/// product hangs off it — while their login and sealed token are refreshed
/// to what GitHub just told us.
///
/// # Errors
///
/// Returns [`ApiError`] if the database rejects the write or the row it
/// returns is not the one the schema promises.
pub async fn upsert_from_github(
    db: &Db,
    account: &GithubUser,
    sealed_token: &str,
) -> Result<CurrentUser, ApiError> {
    let row: IdentityRow = db
        .query(
            "INSERT INTO users (id, github_id, login, github_token_enc, created_at_unix) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT (github_id) DO UPDATE SET \
             login = excluded.login, github_token_enc = excluded.github_token_enc \
             RETURNING id, login",
        )
        .bind(UserId::generate().to_string())
        .bind(account.id)
        .bind(account.login.clone())
        .bind(sealed_token.to_owned())
        .bind(i64::try_from(now_unix()).unwrap_or(i64::MAX))
        .fetch_one()
        .await?;

    row.try_into()
}

/// Loads the identity behind a [`UserId`].
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails or the stored row is not a
/// valid identity.
pub async fn find(db: &Db, id: UserId) -> Result<Option<CurrentUser>, ApiError> {
    let row: Option<IdentityRow> = db
        .query("SELECT id, login FROM users WHERE id = ?")
        .bind(id.to_string())
        .fetch_optional()
        .await?;

    row.map(TryInto::try_into).transpose()
}

/// Reads back the sealed GitHub token stored for a user.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn sealed_github_token(db: &Db, id: UserId) -> Result<Option<String>, ApiError> {
    #[derive(Debug, Deserialize)]
    struct TokenRow {
        github_token_enc: String,
    }

    let row: Option<TokenRow> = db
        .query("SELECT github_token_enc FROM users WHERE id = ?")
        .bind(id.to_string())
        .fetch_optional()
        .await?;

    Ok(row.map(|row| row.github_token_enc))
}
