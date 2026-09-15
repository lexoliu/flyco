//! The `users` table.
//!
//! One row per GitHub account, keyed by GitHub's immutable numeric id
//! because logins can be renamed and reused. The account's GitHub grant is
//! sealed before it gets here and is never read back out on this path.

use flyco_core::{CurrentUser, SESSION_CAP_MAX, SESSION_CAP_MIN, UserId};
use skyzen::sql;
use skyzen_services::Db;

use crate::clock::now_unix;
use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::github::{GithubError, GithubGrant, GithubOauth, GithubToken, GithubUser};
use crate::harness_accounts::REFRESH_WINDOW_SECONDS;

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

/// The stored halves of a user's GitHub grant.
#[derive(Debug, skyzen::FromRow)]
struct GrantRow {
    token_enc: String,
    refresh_token_enc: Option<String>,
    token_expires_at_unix: Option<u64>,
}

/// Records a completed GitHub sign-in, creating the account on first sight.
///
/// A returning user keeps their flyco [`UserId`] — everything else in the
/// product hangs off it — while their login and sealed grant are refreshed
/// to what GitHub just told us. When the OAuth app expires user tokens the
/// grant carries the refresh credential it is renewed with; when it does
/// not, those columns stay `NULL` and the grant is used until revoked.
///
/// # Errors
///
/// Returns [`ApiError`] if the database rejects the write or the row it
/// returns is not the one the schema promises.
pub async fn upsert_from_github(
    db: &Db,
    config: &ApiConfig,
    account: &GithubUser,
    grant: &GithubGrant,
) -> Result<CurrentUser, ApiError> {
    let cipher = config.token_cipher();
    let sealed_token = cipher.seal(&grant.token.access_token)?;
    let sealed_refresh = grant
        .refresh_token
        .as_deref()
        .map(|refresh| cipher.seal(refresh))
        .transpose()?;
    let expires_at = grant.expires_at_unix;
    let row: IdentityRow = sql!(
        db,
        "INSERT INTO users (id, github_id, login, github_token_enc, \
             github_refresh_token_enc, github_token_expires_at_unix, created_at_unix) \
         VALUES ({UserId::generate()}, {account.id}, {account.login.clone()}, \
                 {sealed_token}, {sealed_refresh}, {expires_at}, {now_unix()}) \
         ON CONFLICT (github_id) DO UPDATE SET \
         login = excluded.login, \
         github_token_enc = excluded.github_token_enc, \
         github_refresh_token_enc = excluded.github_refresh_token_enc, \
         github_token_expires_at_unix = excluded.github_token_expires_at_unix \
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

/// Reads the stored grant row for a user.
///
/// A row is only ever written by a completed sign-in, which always stores
/// the token half — `None` here means the user id named no row, which is
/// corrupt rather than absent.
async fn grant_row(db: &Db, id: UserId) -> Result<Option<GrantRow>, ApiError> {
    Ok(sql!(
        db,
        "SELECT github_token_enc AS token_enc, \
         github_refresh_token_enc AS refresh_token_enc, \
         github_token_expires_at_unix AS token_expires_at_unix \
         FROM users WHERE id = {id}"
    )
    .fetch_optional()
    .await?)
}

/// Opens a stored grant's access token.
fn access_token(config: &ApiConfig, row: &GrantRow) -> Result<GithubToken, ApiError> {
    Ok(GithubToken {
        access_token: config.token_cipher().open(&row.token_enc)?,
    })
}

/// Whether the stored grant is close enough to its end to renew first.
///
/// A grant GitHub never expires — and every row written before flyco kept
/// refresh credentials — carries no `expires_at`, so it answers false and
/// is used until GitHub revokes it.
fn due_for_renewal(row: &GrantRow) -> bool {
    row.token_expires_at_unix
        .is_some_and(|at| at <= now_unix().saturating_add(REFRESH_WINDOW_SECONDS))
}

/// Opens a user's stored GitHub grant for a call flyco makes on their
/// behalf.
///
/// The one place the seal is broken, because everything that acts as the
/// user needs exactly this: the repository picker, the branch picker, and
/// the provisioning queue building a machine's checkout.
///
/// A grant whose access token is nearing its end is renewed here rather
/// than handed out to fail: GitHub rotates the pair on every redemption, so
/// the write is fenced on the sealed token this read began with — a racing
/// renewal that landed first is the grant GitHub honors next, and its row
/// is re-read instead of overwritten. A refresh GitHub rejects outright
/// means the grant is dead, which is reported as
/// [`ApiError::GithubTokenRevoked`] — the one honest answer, since only the
/// user can re-authorize it.
///
/// # Errors
///
/// Returns [`ApiError::CorruptRecord`] if the row holds no token — a user
/// row is only ever written by a completed sign-in, which always stores one
/// — [`ApiError::GithubTokenRevoked`] if the grant can no longer be
/// renewed, or a database or cryptography error otherwise.
pub async fn github_token(
    db: &Db,
    config: &ApiConfig,
    github: &impl GithubOauth,
    id: UserId,
) -> Result<GithubToken, ApiError> {
    let row = grant_row(db, id).await?.ok_or(ApiError::CorruptRecord(
        "the user has no stored GitHub token",
    ))?;
    if !due_for_renewal(&row) {
        return access_token(config, &row);
    }

    let Some(sealed_refresh) = row.refresh_token_enc.clone() else {
        // An expiring grant with no renewal credential cannot come from a
        // completed sign-in — the two columns are written together — but a
        // token that has not quite ended is still GitHub's to honor, so it
        // is handed out and its call's own answer stands.
        if row.token_expires_at_unix.is_some_and(|at| at <= now_unix()) {
            return Err(ApiError::GithubTokenRevoked);
        }
        return access_token(config, &row);
    };

    let refresh_token = config.token_cipher().open(&sealed_refresh)?;
    let grant = match github
        .refresh(
            config.github_client_id(),
            config.github_client_secret(),
            &refresh_token,
        )
        .await
    {
        Ok(grant) => grant,
        Err(GithubError::Rejected { .. }) => {
            // A refresh token is good exactly once: a refusal on a row that
            // has since changed means a racing renewal already redeemed it
            // and wrote its pair — that pair is the answer.
            let current = grant_row(db, id).await?.ok_or(ApiError::CorruptRecord(
                "the user has no stored GitHub token",
            ))?;
            return if current.token_enc == row.token_enc {
                Err(ApiError::GithubTokenRevoked)
            } else {
                access_token(config, &current)
            };
        }
        Err(error) => return Err(error.into()),
    };

    let cipher = config.token_cipher();
    let sealed_token = cipher.seal(&grant.token.access_token)?;
    let sealed_refresh = grant
        .refresh_token
        .as_deref()
        .map(|refresh| cipher.seal(refresh))
        .transpose()?;
    let expires_at = grant.expires_at_unix;
    let written = sql!(
        db,
        "UPDATE users SET \
         github_token_enc = {sealed_token}, \
         github_refresh_token_enc = {sealed_refresh}, \
         github_token_expires_at_unix = {expires_at} \
         WHERE id = {id} AND github_token_enc = {row.token_enc}"
    )
    .execute()
    .await?
    .rows_written;
    if written == 0 {
        // The row moved under the renewal: another request refreshed the
        // same grant first, and what it stored is the pair GitHub honors
        // next. Ours stays valid for this call, but the stored one is what
        // later reads — and the next renewal — must see.
        let current = grant_row(db, id).await?.ok_or(ApiError::CorruptRecord(
            "the user has no stored GitHub token",
        ))?;
        return access_token(config, &current);
    }

    tracing::info!(user = %id, "renewed a GitHub grant before handing it out");
    Ok(grant.token)
}
