//! The credential check in front of every protected route.
//!
//! Flyco has exactly one credential channel: `Authorization: Bearer`. There
//! are no cookies, so there is no ambient authority and nothing that a
//! cross-site request can ride on. The token's prefix says which store owns
//! it — `fs_` for a browser session (KV), `fk_` for an API key (D1) —
//! so a lookup never has to probe both.
//!
//! [`RequireAuth`](crate::middleware::RequireAuth) runs this and injects the
//! resolved [`CurrentUser`] as request state.

use flyco_core::CurrentUser;
use skyzen::Request;
use skyzen::header::{AUTHORIZATION, HeaderMap};
use skyzen::middleware::auth::Authenticator;
use skyzen_services::{Db, Kv};

use crate::error::ApiError;
use crate::{api_keys, session, users};

/// Resolves flyco's two token kinds against KV and D1.
///
/// Stateless: the stores it needs are the ones skyzen already injected into
/// the request, so the authenticator itself carries nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct FlycoAuthenticator;

impl FlycoAuthenticator {
    /// Creates the authenticator.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Authenticator for FlycoAuthenticator {
    type User = CurrentUser;
    type Error = ApiError;

    async fn authenticate(&self, request: &Request) -> Result<Self::User, Self::Error> {
        let presented = bearer_token(request.headers()).ok_or(ApiError::MissingCredential)?;
        let db = request
            .extensions()
            .get::<Db>()
            .cloned()
            .ok_or(ApiError::ServiceMissing("main"))?;

        let user_id = if presented.starts_with(session::TOKEN_PREFIX) {
            let kv = request
                .extensions()
                .get::<Kv>()
                .cloned()
                .ok_or(ApiError::ServiceMissing("auth_kv"))?;
            session::resolve(&kv, presented)
                .await?
                .ok_or(ApiError::InvalidCredential)?
        } else if presented.starts_with(api_keys::TOKEN_PREFIX) {
            let owner = api_keys::find_by_token(&db, presented)
                .await?
                .ok_or(ApiError::InvalidCredential)?;
            api_keys::mark_used(&db, owner.key_id).await?;
            owner.user_id
        } else {
            return Err(ApiError::InvalidCredential);
        };

        users::find(&db, user_id)
            .await?
            .ok_or(ApiError::InvalidCredential)
    }
}

/// Extracts the credential from an `Authorization: Bearer …` header.
///
/// The scheme is matched case-insensitively per RFC 7235.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(char::is_whitespace)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }

    let token = token.trim_start();
    (!token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use skyzen::header::{AUTHORIZATION, HeaderMap, HeaderValue};

    use super::bearer_token;

    fn headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(value).expect("header value"),
        );
        headers
    }

    #[test]
    fn the_bearer_scheme_is_case_insensitive() {
        assert_eq!(bearer_token(&headers("bEaReR  fk_abc")), Some("fk_abc"));
    }

    #[test]
    fn other_authorization_schemes_are_ignored() {
        assert_eq!(bearer_token(&headers("Basic abc")), None);
        assert_eq!(bearer_token(&headers("Bearer   ")), None);
        assert_eq!(bearer_token(&HeaderMap::new()), None);
    }
}
