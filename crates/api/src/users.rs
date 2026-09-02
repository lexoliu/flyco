//! The `users` table.
//!
//! One row per GitHub account, keyed by GitHub's immutable numeric id
//! because logins can be renamed and reused. The account's GitHub token is
//! sealed before it gets here and is never read back out on this path.

use flyco_core::{CurrentUser, SESSION_CAP_MAX, SESSION_CAP_MIN, UserId};
use skyzen::sql;
use skyzen_services::Db;

use crate::clock::now_unix;
use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::github::{GithubToken, GithubUser};

/// The columns every read on this path projects.
#[derive(Debug, skyzen::FromRow)]
struct IdentityRow {
    id: UserId,
    login: String,
    session_cap: u32,
}

impl From<IdentityRow> for CurrentUser {
    fn from(row: IdentityRow) -> Self {
        Self {
            id: row.id,
            login: row.login,
            session_cap: row.session_cap,
        }
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
    let row: IdentityRow = sql!(
        db,
        "INSERT INTO users (id, github_id, login, github_token_enc, created_at_unix) \
         VALUES ({UserId::generate()}, {account.id}, {account.login.clone()}, \
                 {sealed_token}, {now_unix()}) \
         ON CONFLICT (github_id) DO UPDATE SET \
         login = excluded.login, github_token_enc = excluded.github_token_enc \
         RETURNING id, login, session_cap"
    )
    .fetch_one()
    .await?;

    Ok(row.into())
}

/// Loads the identity behind a [`UserId`].
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails or the stored row is not a
/// valid identity.
pub async fn find(db: &Db, id: UserId) -> Result<Option<CurrentUser>, ApiError> {
    let row: Option<IdentityRow> = sql!(
        db,
        "SELECT id, login, session_cap FROM users WHERE id = {id}"
    )
    .fetch_optional()
    .await?;

    Ok(row.map(Into::into))
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

    sql!(db, "UPDATE users SET session_cap = {cap} WHERE id = {id}")
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
    Ok(
        sql!(db, "SELECT github_token_enc FROM users WHERE id = {id}")
            .fetch_scalar_optional()
            .await?,
    )
}

/// Opens a user's stored GitHub token for a call flyco makes on their
/// behalf.
///
/// The one place the seal is broken, because everything that acts as the
/// user needs exactly this: the repository picker, the branch picker, and
/// the provisioning queue building a machine's checkout.
///
/// # Errors
///
/// Returns [`ApiError::CorruptRecord`] if the row holds no token — a user
/// row is only ever written by a completed sign-in, which always stores one
/// — or a database or cryptography error otherwise.
pub async fn github_token(
    db: &Db,
    config: &ApiConfig,
    id: UserId,
) -> Result<GithubToken, ApiError> {
    let sealed = sealed_github_token(db, id)
        .await?
        .ok_or(ApiError::CorruptRecord(
            "the user has no stored GitHub token",
        ))?;
    Ok(GithubToken {
        access_token: config.token_cipher().open(&sealed)?,
    })
}
