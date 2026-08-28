//! The credential check in front of every protected route.
//!
//! Two credentials are accepted: an `Authorization: Bearer fk_…` API key and
//! the `flyco_session` cookie the browser holds. An explicit `Authorization`
//! header wins — a caller who presents a bad key is refused rather than
//! silently falling through to whatever cookie their browser happened to
//! carry. Skyzen's [`AuthMiddleware`](skyzen::middleware::auth::AuthMiddleware)
//! runs this and injects the resolved [`CurrentUser`] as request state.

use flyco_core::CurrentUser;
use skyzen::header::{AUTHORIZATION, COOKIE};
use skyzen::middleware::auth::Authenticator;
use skyzen::{Request, header::HeaderMap};
use skyzen_services::{Db, Kv};

use crate::error::ApiError;
use crate::{api_keys, session, users};

/// Resolves flyco's two credential shapes against KV and D1.
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
        let kv = request
            .extensions()
            .get::<Kv>()
            .cloned()
            .ok_or(ApiError::ServiceMissing("auth_kv"))?;
        let db = request
            .extensions()
            .get::<Db>()
            .cloned()
            .ok_or(ApiError::ServiceMissing("main"))?;

        let bearer = bearer_token(request.headers()).map(ToOwned::to_owned);
        let cookie = session_cookie(request.headers());

        let user_id = if let Some(presented) = bearer {
            let Some(owner) = api_keys::find_by_token(&db, &presented).await? else {
                return Err(ApiError::Unauthenticated);
            };
            api_keys::mark_used(&db, owner.key_id).await?;
            owner.user_id
        } else if let Some(token) = cookie {
            session::resolve(&kv, &token)
                .await?
                .ok_or(ApiError::Unauthenticated)?
        } else {
            return Err(ApiError::Unauthenticated);
        };

        users::find(&db, user_id)
            .await?
            .ok_or(ApiError::Unauthenticated)
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

/// Extracts the flyco session token from the `Cookie` header.
fn session_cookie(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(COOKIE)?.to_str().ok()?;
    cookie::Cookie::split_parse_encoded(value)
        .filter_map(Result::ok)
        .find(|cookie| cookie.name() == session::COOKIE_NAME)
        .map(|cookie| cookie.value().to_owned())
}

#[cfg(test)]
mod tests {
    use skyzen::header::{AUTHORIZATION, COOKIE, HeaderMap, HeaderValue};

    use super::{bearer_token, session_cookie};

    fn headers(name: skyzen::header::HeaderName, value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(name, HeaderValue::from_str(value).expect("header value"));
        headers
    }

    #[test]
    fn the_bearer_scheme_is_case_insensitive() {
        assert_eq!(
            bearer_token(&headers(AUTHORIZATION, "bEaReR  fk_abc")),
            Some("fk_abc")
        );
    }

    #[test]
    fn other_authorization_schemes_are_ignored() {
        assert_eq!(bearer_token(&headers(AUTHORIZATION, "Basic abc")), None);
        assert_eq!(bearer_token(&headers(AUTHORIZATION, "Bearer   ")), None);
        assert_eq!(bearer_token(&HeaderMap::new()), None);
    }

    #[test]
    fn the_session_cookie_is_picked_out_of_the_header() {
        let jar = headers(COOKIE, "theme=dark; flyco_session=abc%2Fdef; other=1");
        assert_eq!(session_cookie(&jar).as_deref(), Some("abc/def"));
    }

    #[test]
    fn an_unrelated_cookie_header_yields_nothing() {
        assert_eq!(session_cookie(&headers(COOKIE, "theme=dark")), None);
        assert_eq!(session_cookie(&HeaderMap::new()), None);
    }
}
