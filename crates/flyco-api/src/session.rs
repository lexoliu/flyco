//! Browser sessions: opaque tokens in KV, handed to the browser in a cookie.
//!
//! The token is random and meaningless on its own; KV maps its SHA-256 to
//! the user it stands for. Nothing is signed, so revoking a session is a
//! single delete rather than a key rotation.

use cookie::{Cookie, SameSite, time::Duration};
use flyco_core::UserId;
use serde::{Deserialize, Serialize};
use skyzen_services::Kv;

use crate::crypto::{random_token, token_hash};
use crate::error::ApiError;
use crate::expiring;

/// Cookie the browser presents on every subsequent request.
pub const COOKIE_NAME: &str = "flyco_session";

/// How long a session stays valid without being refreshed.
pub const TTL_SECONDS: u64 = 30 * 24 * 60 * 60;

/// What a session token stands for.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Session {
    user_id: UserId,
}

fn kv_key(token: &str) -> String {
    let mut key = String::with_capacity(11 + 64);
    key.push_str("auth:token:");
    key.push_str(&token_hash(token));
    key
}

/// Mints a session for `user_id` and records it in KV.
///
/// The plaintext token is returned to the caller once, to be placed in the
/// cookie built by [`cookie`]; only its hash is ever stored.
///
/// # Errors
///
/// Returns [`ApiError`] if entropy is unavailable or KV rejects the write.
pub async fn issue(kv: &Kv, user_id: UserId) -> Result<String, ApiError> {
    let token = random_token()?;
    expiring::put(kv, &kv_key(&token), &Session { user_id }, TTL_SECONDS).await?;
    Ok(token)
}

/// Resolves a presented session token to the user it belongs to.
///
/// # Errors
///
/// Returns [`ApiError`] if KV fails; an unknown or expired token is `Ok(None)`.
pub async fn resolve(kv: &Kv, token: &str) -> Result<Option<UserId>, ApiError> {
    Ok(expiring::get::<Session>(kv, &kv_key(token))
        .await?
        .map(|session| session.user_id))
}

/// Builds the `Set-Cookie` value that carries `token` to the browser.
///
/// `HttpOnly` keeps it out of reach of page scripts, `Secure` keeps it off
/// plaintext transports, and `SameSite=Lax` still allows the top-level
/// navigation GitHub performs when it redirects back.
#[must_use]
pub fn cookie(token: String) -> Cookie<'static> {
    Cookie::build((COOKIE_NAME, token))
        .path("/")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(Duration::seconds(
            i64::try_from(TTL_SECONDS).unwrap_or(i64::MAX),
        ))
        .build()
}
