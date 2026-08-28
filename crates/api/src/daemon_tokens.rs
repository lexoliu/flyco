//! Daemon credentials: the `fd_` token that pairs one `flycod` with one
//! session.
//!
//! A daemon token is not a user credential and does not resolve to a
//! [`CurrentUser`](flyco_core::CurrentUser). It authenticates *one session's
//! daemon* against the daemon-scoped routes of *that* session, and nothing
//! else: the lookup is keyed by the session id in the request path, so a
//! token minted for session A never matches while session B is being
//! addressed. That is the whole authorization model, and it is one column
//! comparison rather than a scope list.
//!
//! Like every other flyco credential, only the SHA-256 is stored. Minting
//! replaces whatever was there, so re-pairing a session revokes the token
//! the previous daemon holds.

use flyco_core::{DAEMON_TOKEN_PREFIX, DaemonToken, SessionId, UserId};
use serde::Deserialize;
use skyzen_services::Db;

use crate::crypto::{prefixed_token, token_hash};
use crate::error::ApiError;

#[derive(Debug, Deserialize)]
struct TokenHashRow {
    daemon_token_hash: Option<String>,
}

/// Mints a daemon token for one of `user`'s sessions, replacing any token
/// the session already had.
///
/// The returned [`DaemonToken::token`] is the only copy that will ever
/// exist.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is not the
/// caller's, or [`ApiError`] if entropy is unavailable or the write fails.
pub async fn issue(db: &Db, user: UserId, session: SessionId) -> Result<DaemonToken, ApiError> {
    let token = prefixed_token(DAEMON_TOKEN_PREFIX)?;

    let result = db
        .query("UPDATE sessions SET daemon_token_hash = ? WHERE id = ? AND user_id = ?")
        .bind(token_hash(&token))
        .bind(session.to_string())
        .bind(user.to_string())
        .execute()
        .await?;

    if result.rows_written == 0 {
        return Err(ApiError::SessionNotFound);
    }
    Ok(DaemonToken { session, token })
}

/// Whether `presented` is the live daemon token of `session`.
///
/// A session with no token paired yet answers `false`, exactly as a wrong
/// token does: an unpaired session must not be distinguishable from a
/// mispaired one.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn authenticates(db: &Db, session: SessionId, presented: &str) -> Result<bool, ApiError> {
    if !presented.starts_with(DAEMON_TOKEN_PREFIX) {
        return Ok(false);
    }

    let row: Option<TokenHashRow> = db
        .query("SELECT daemon_token_hash FROM sessions WHERE id = ?")
        .bind(session.to_string())
        .fetch_optional()
        .await?;

    Ok(row
        .and_then(|row| row.daemon_token_hash)
        .is_some_and(|stored| stored == token_hash(presented)))
}

#[cfg(test)]
mod tests {
    use flyco_core::{DAEMON_TOKEN_PREFIX, SessionId};
    use skyzen_services::Db;

    use super::{authenticates, issue};
    use crate::testing::{migrate, seed_other_user, seed_session, seed_user};

    #[skyzen::test]
    async fn a_minted_token_authenticates_only_its_own_session(db: Db) {
        migrate(&db).await;
        let user = seed_user(&db).await;
        let first = seed_session(&db, &user).await;
        let second = seed_session(&db, &user).await;

        let token = issue(&db, user.id, first).await.expect("mint");
        assert!(token.token.starts_with(DAEMON_TOKEN_PREFIX));

        assert!(
            authenticates(&db, first, &token.token)
                .await
                .expect("check first")
        );
        assert!(
            !authenticates(&db, second, &token.token)
                .await
                .expect("check second"),
            "a token minted for one session must not open another"
        );
    }

    #[skyzen::test]
    async fn minting_again_revokes_the_previous_token(db: Db) {
        migrate(&db).await;
        let user = seed_user(&db).await;
        let session = seed_session(&db, &user).await;

        let first = issue(&db, user.id, session).await.expect("mint");
        let second = issue(&db, user.id, session).await.expect("re-mint");

        assert!(
            !authenticates(&db, session, &first.token)
                .await
                .expect("check first")
        );
        assert!(
            authenticates(&db, session, &second.token)
                .await
                .expect("check second")
        );
    }

    #[skyzen::test]
    async fn an_unpaired_or_unknown_session_authenticates_nothing(db: Db) {
        migrate(&db).await;
        let user = seed_user(&db).await;
        let session = seed_session(&db, &user).await;

        assert!(
            !authenticates(&db, session, "fd_anything")
                .await
                .expect("check unpaired")
        );
        assert!(
            !authenticates(&db, SessionId::generate(), "fd_anything")
                .await
                .expect("check unknown")
        );
    }

    #[skyzen::test]
    async fn a_token_of_another_kind_is_never_a_daemon_token(db: Db) {
        migrate(&db).await;
        let user = seed_user(&db).await;
        let session = seed_session(&db, &user).await;
        let token = issue(&db, user.id, session).await.expect("mint");

        // Same bytes, wrong prefix: the prefix is part of what is hashed, so
        // this cannot match, and the check refuses it before touching D1.
        let disguised = token.token.replace(DAEMON_TOKEN_PREFIX, "fk_");
        assert!(
            !authenticates(&db, session, &disguised)
                .await
                .expect("check disguised")
        );
    }

    #[skyzen::test]
    async fn a_session_that_is_not_the_callers_cannot_be_paired(db: Db) {
        migrate(&db).await;
        let owner = seed_user(&db).await;
        let stranger = seed_other_user(&db).await;
        let session = seed_session(&db, &owner).await;

        issue(&db, stranger.id, session)
            .await
            .expect_err("a stranger may not mint a daemon token for this session");
    }
}
