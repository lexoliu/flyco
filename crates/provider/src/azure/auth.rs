//! Client-credentials `OAuth2` against Microsoft Entra ID.
//!
//! One `POST` to `login.microsoftonline.com/{tenant}/oauth2/v2.0/token` with
//! exactly four form fields, for the resource scope
//! `https://management.azure.com/.default`. The `.default` form is not
//! optional — client credentials has no way to name individual permissions —
//! and no refresh token is issued, so a stale token is re-minted from the
//! client secret rather than exchanged.
//!
//! # Why the cache is keyed on elapsed time
//!
//! The response states `expires_in` in seconds, not an absolute time, and
//! the JWT's own `exp` would need a parser the driver has no other use for.
//! So the cache remembers *when it asked*, on a [`MonotonicClock`], and
//! refreshes at [`REFRESH_AT_PERCENT`] of the stated lifetime. Wall-clock
//! time is deliberately not consulted: it can move backwards under an NTP
//! correction, and a cache that reads a backwards jump as "still fresh"
//! keeps presenting a token the service has already rejected.
//!
//! A 401 invalidates the cache regardless, because the service is the
//! authority on its own tokens and the clock is only an optimisation.

use core::fmt;

use serde::Deserialize;

use crate::ProviderError;
use crate::clock::MonotonicClock;
use crate::http::{HttpRequest, HttpResponse, HttpTransport, Method};

/// Microsoft Entra ID's public authority.
pub const AUTHORITY: &str = "https://login.microsoftonline.com";

/// The resource scope every Azure Resource Manager call is issued against.
pub const SCOPE: &str = "https://management.azure.com/.default";

/// The only grant a service principal can use here.
pub const GRANT_TYPE: &str = "client_credentials";

/// Fraction of the stated lifetime a cached token is reused for.
///
/// Twenty per cent of an hour is twelve minutes of headroom, which is more
/// than enough for the longest provisioning sequence to finish on the token
/// it started with.
pub const REFRESH_AT_PERCENT: u64 = 80;

/// An Azure service principal, and the subscription it acts on.
///
/// The client secret is a credential; the [`fmt::Debug`] below is what keeps
/// it out of a provisioning log.
#[derive(Clone, PartialEq, Eq)]
pub struct ServicePrincipal {
    /// Directory (tenant) the service principal belongs to.
    pub tenant_id: String,
    /// Application (client) id.
    pub client_id: String,
    /// Client secret issued for that application.
    pub client_secret: String,
    /// Subscription machines are provisioned into.
    pub subscription_id: String,
}

impl fmt::Debug for ServicePrincipal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServicePrincipal")
            .field("tenant_id", &self.tenant_id)
            .field("client_id", &self.client_id)
            .field("subscription_id", &self.subscription_id)
            .finish_non_exhaustive()
    }
}

impl ServicePrincipal {
    /// The token endpoint for this principal's tenant.
    #[must_use]
    pub fn token_url(&self) -> String {
        let mut url = String::with_capacity(AUTHORITY.len() + self.tenant_id.len() + 24);
        url.push_str(AUTHORITY);
        url.push('/');
        url.push_str(&self.tenant_id);
        url.push_str("/oauth2/v2.0/token");
        url
    }
}

/// What the token endpoint answers with.
#[derive(Debug, Clone, Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

/// A token, and the elapsed reading past which it stops being reused.
#[derive(Clone)]
struct CachedToken {
    value: String,
    refresh_after: u64,
}

/// Holds the current access token and re-mints it when it is due.
///
/// Mutating methods take `&mut self`, which is why
/// [`CloudProvider`](crate::CloudProvider) does too: replacing a cached
/// credential is a write, and saying so with the borrow checker costs
/// nothing where a lock would cost a lock.
#[derive(Clone)]
pub struct TokenCache {
    principal: ServicePrincipal,
    cached: Option<CachedToken>,
}

impl fmt::Debug for TokenCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenCache")
            .field("principal", &self.principal)
            .field("holds_token", &self.cached.is_some())
            .finish_non_exhaustive()
    }
}

impl TokenCache {
    /// An empty cache for one principal.
    #[must_use]
    pub const fn new(principal: ServicePrincipal) -> Self {
        Self {
            principal,
            cached: None,
        }
    }

    /// The principal this cache mints for.
    #[must_use]
    pub const fn principal(&self) -> &ServicePrincipal {
        &self.principal
    }

    /// Forgets the current token, so the next call mints a fresh one.
    ///
    /// Called on any 401: the service is the authority on whether a token
    /// still works, and it has just said this one does not.
    pub fn invalidate(&mut self) {
        self.cached = None;
    }

    /// The bearer token to present, minting one if the cache is cold or due.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Rejected`] if Entra ID refuses the client
    /// credentials, or a transport error if the request never completed.
    #[expect(
        clippy::future_not_send,
        reason = "`Send`-ness follows from the concrete transport: the deployed one is a \
                  unit struct and the Worker is single-threaded, while the test transport \
                  is deliberately not `Sync`. Bounding `T: Sync` here would forbid the \
                  recorded transport the whole driver is tested against."
    )]
    pub async fn access_token<T: HttpTransport, C: MonotonicClock>(
        &mut self,
        transport: &T,
        clock: &C,
    ) -> Result<String, ProviderError> {
        let now = clock.elapsed_seconds();
        if let Some(cached) = &self.cached
            && now < cached.refresh_after
        {
            return Ok(cached.value.clone());
        }

        let request = HttpRequest::new(Method::Post, self.principal.token_url()).form_body(&[
            ("grant_type", GRANT_TYPE),
            ("client_id", &self.principal.client_id),
            ("client_secret", &self.principal.client_secret),
            ("scope", SCOPE),
        ]);

        let response = transport.send(request).await?;
        if !response.is_success() {
            return Err(ProviderError::Rejected(refusal(&response)));
        }

        let token: TokenResponse = response.json()?;
        let lifetime = token.expires_in.saturating_mul(REFRESH_AT_PERCENT) / 100;
        self.cached = Some(CachedToken {
            value: token.access_token.clone(),
            refresh_after: now.saturating_add(lifetime),
        });

        tracing::debug!(
            tenant = %self.principal.tenant_id,
            reuse_for_seconds = lifetime,
            "minted an Azure management token"
        );
        Ok(token.access_token)
    }
}

#[cfg(test)]
mod tests {
    use super::{GRANT_TYPE, SCOPE, ServicePrincipal, TokenCache};
    use crate::clock::ManualClock;
    use crate::http::{HttpResponse, Method};
    use crate::testing::RecordedTransport;

    const TENANT: &str = "f9dd8f4f-3b8b-4768-aba7-bbd379e0736b";

    fn principal() -> ServicePrincipal {
        ServicePrincipal {
            tenant_id: TENANT.to_owned(),
            client_id: "app-id".to_owned(),
            client_secret: "app-secret".to_owned(),
            subscription_id: "e47d07d8-2715-4909-aa56-1bfde801bdf0".to_owned(),
        }
    }

    fn token(value: &str, expires_in: u64) -> HttpResponse {
        HttpResponse::new(
            200,
            serde_json::to_vec(&serde_json::json!({
                "token_type": "Bearer",
                "expires_in": expires_in,
                "ext_expires_in": expires_in,
                "access_token": value,
            }))
            .expect("serialize"),
        )
    }

    #[test]
    fn the_token_endpoint_is_the_tenants_v2_endpoint() {
        assert_eq!(
            principal().token_url(),
            format!("https://login.microsoftonline.com/{TENANT}/oauth2/v2.0/token")
        );
    }

    #[tokio::test]
    async fn a_token_request_carries_exactly_the_four_client_credentials_fields() {
        let transport = RecordedTransport::new(vec![token("first", 3_599)]);
        let clock = ManualClock::new();
        let mut cache = TokenCache::new(principal());

        cache
            .access_token(&transport, &clock)
            .await
            .expect("mint a token");

        let request = transport.request(0);
        assert_eq!(request.method, Method::Post);
        assert_eq!(request.url, principal().token_url());
        assert_eq!(
            request.headers,
            vec![(
                "content-type".to_owned(),
                "application/x-www-form-urlencoded".to_owned()
            )]
        );

        let body = request.body_text().expect("UTF-8");
        let fields: Vec<(String, String)> = url::form_urlencoded::parse(body.as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        assert_eq!(
            fields,
            vec![
                ("grant_type".to_owned(), GRANT_TYPE.to_owned()),
                ("client_id".to_owned(), "app-id".to_owned()),
                ("client_secret".to_owned(), "app-secret".to_owned()),
                ("scope".to_owned(), SCOPE.to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn a_live_token_is_reused_and_a_due_one_is_re_minted() {
        let transport = RecordedTransport::new(vec![token("first", 3_600), token("second", 3_600)]);
        let clock = ManualClock::new();
        let mut cache = TokenCache::new(principal());

        assert_eq!(
            cache.access_token(&transport, &clock).await.expect("mint"),
            "first"
        );

        // Still inside 80% of an hour: no second request.
        clock.advance(2_879);
        assert_eq!(
            cache.access_token(&transport, &clock).await.expect("reuse"),
            "first"
        );
        assert_eq!(transport.request_count(), 1);

        // One second past 80% of 3600s, so the token is due.
        clock.advance(1);
        assert_eq!(
            cache
                .access_token(&transport, &clock)
                .await
                .expect("re-mint"),
            "second"
        );
        assert_eq!(transport.request_count(), 2);
    }

    #[tokio::test]
    async fn invalidating_the_cache_forces_a_fresh_token_before_it_is_due() {
        let transport = RecordedTransport::new(vec![token("first", 3_600), token("second", 3_600)]);
        let clock = ManualClock::new();
        let mut cache = TokenCache::new(principal());

        cache.access_token(&transport, &clock).await.expect("mint");
        cache.invalidate();

        assert_eq!(
            cache
                .access_token(&transport, &clock)
                .await
                .expect("re-mint"),
            "second"
        );
        assert_eq!(transport.request_count(), 2);
    }

    #[tokio::test]
    async fn a_refused_service_principal_is_reported_rather_than_cached() {
        let transport = RecordedTransport::new(vec![HttpResponse::new(
            401,
            br#"{"error":"invalid_client","error_description":"AADSTS7000215"}"#.to_vec(),
        )]);
        let clock = ManualClock::new();
        let mut cache = TokenCache::new(principal());

        let error = cache
            .access_token(&transport, &clock)
            .await
            .expect_err("a rejected principal is an error");
        assert!(error.to_string().contains("AADSTS7000215"));
    }

    #[test]
    fn a_principal_never_debug_prints_its_client_secret() {
        assert!(!format!("{:?}", principal()).contains("app-secret"));
        assert!(!format!("{:?}", TokenCache::new(principal())).contains("app-secret"));
    }
}

/// The body Entra ID sends with a refused token request.
///
/// Only the two fields a person can act on. The description also carries
/// a trace id, a correlation id and a timestamp for Microsoft support,
/// which are noise to the user and are cut off.
#[derive(serde::Deserialize)]
struct TokenRefusal {
    error: String,
    error_description: String,
}

/// What to tell the user when Entra ID refused the principal.
///
/// "Wrong secret" and "no such application" are different sentences from
/// Microsoft, so the sentence is surfaced rather than the status; a body
/// that is not Microsoft's refusal document is shown as it came.
fn refusal(response: &HttpResponse) -> String {
    match response.json::<TokenRefusal>() {
        Ok(TokenRefusal {
            error,
            error_description,
        }) => {
            let sentence = error_description
                .split(" Trace ID:")
                .next()
                .unwrap_or(&error_description)
                .trim();
            format!("Entra ID refused the service principal ({error}): {sentence}")
        }
        Err(_) => format!(
            "Entra ID refused the service principal (HTTP {}): {}",
            response.status,
            response.body_text()
        ),
    }
}
