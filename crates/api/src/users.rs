//! The `users` table.
//!
//! One row per GitHub account, keyed by GitHub's immutable numeric id
//! because logins can be renamed and reused. The account's GitHub token is
//! sealed before it gets here and is never read back out on this path.

use flyco_core::{CurrentUser, SESSION_CAP_MAX, SESSION_CAP_MIN, UserId};
use skyzen_services::Db;

use crate::clock::now_unix;
use crate::error::ApiError;
use crate::github::GithubUser;
use crate::sql::{from_column, to_column};

/// The columns every read on this path projects.
#[derive(Debug, skyzen::FromRow)]
struct IdentityRow {
    id: String,
    login: String,
    session_cap: i64,
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
            session_cap: u32::try_from(from_column(row.session_cap, "users.session_cap")?)
                .map_err(|_| ApiError::CorruptRecord("users.session_cap is implausibly large"))?,
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
             RETURNING id, login, session_cap",
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
        .query("SELECT id, login, session_cap FROM users WHERE id = ?")
        .bind(id.to_string())
        .fetch_optional()
        .await?;

    row.map(TryInto::try_into).transpose()
}

/// Sets how many sessions a user may hold at once.
///
/// # Errors
///
/// Returns [`ApiError::InvalidSessionCap`] if `cap` is outside
/// [`SESSION_CAP_MIN`]..=[`SESSION_CAP_MAX`], or a database error otherwise.
pub async fn set_session_cap(db: &Db, id: UserId, cap: u32) -> Result<(), ApiError> {
    if !(SESSION_CAP_MIN..=SESSION_CAP_MAX).contains(&cap) {
        return Err(ApiError::InvalidSessionCap {
            min: SESSION_CAP_MIN,
            max: SESSION_CAP_MAX,
        });
    }

    db.query("UPDATE users SET session_cap = ? WHERE id = ?")
        .bind(to_column(u64::from(cap)))
        .bind(id.to_string())
        .execute()
        .await?;

    Ok(())
}

/// Reads back the sealed GitHub token stored for a user.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn sealed_github_token(db: &Db, id: UserId) -> Result<Option<String>, ApiError> {
    #[derive(Debug, skyzen::FromRow)]
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
