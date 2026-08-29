//! A service-account key, and the access token it is exchanged for.
//!
//! Google's service accounts do not authenticate with a shared secret. The
//! key is an RSA private key, and proving possession of it means **minting a
//! JWT and signing it** (RS256), then handing that assertion to the token
//! endpoint under the `jwt-bearer` grant, which answers with an ordinary
//! `OAuth2` access token. So where Azure posts four form fields, this posts
//! two — one of which the driver had to compute.
//!
//! # Two clocks, and why
//!
//! The assertion carries `iat` and `exp` **claims**, which are wall-clock
//! times a Google server checks against its own clock — no amount of elapsed
//! time will do, and that is what [`WallClock`] is for.
//!
//! The *cache* is a different question, and it is keyed on elapsed time for
//! Azure's reason: wall-clock time can move backwards under an NTP
//! correction, and a cache that reads a backwards jump as "still fresh"
//! keeps presenting a token the service has already rejected. A 401
//! invalidates the cache regardless, because the service is the authority on
//! its own tokens and the clock is only an optimisation.
//!
//! # The signature needs no randomness
//!
//! PKCS#1 v1.5 is deterministic: the same key over the same bytes is the
//! same signature, with no RNG in the path. That is why this compiles on
//! wasm32 without a random source, and it is also what lets a test assert
//! the assertion byte for byte.

use core::fmt;

use base64::Engine as _;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey as _;
use rsa::signature::{SignatureEncoding as _, Signer as _};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::ProviderError;
use crate::clock::{MonotonicClock, WallClock};
use crate::http::{HttpRequest, HttpTransport, Method};

/// The grant a signed assertion is exchanged under.
pub const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

/// The scope every Compute Engine and Cloud Billing call is issued against.
///
/// `cloud-platform` rather than the narrower `compute`: the catalog also
/// reads the Cloud Billing price list, and a token scoped to Compute Engine
/// alone is refused there. It is the scope Google's own client libraries
/// default to.
pub const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// How long an assertion claims to be valid for, in seconds.
///
/// Google caps this at an hour and refuses anything longer outright, so it
/// is the maximum rather than a preference.
pub const ASSERTION_LIFETIME_SECONDS: u64 = 3_600;

/// Fraction of the stated lifetime a cached token is reused for.
pub const REFRESH_AT_PERCENT: u64 = 80;

/// The JOSE header of an RS256 assertion.
#[derive(Debug, Clone, Serialize)]
struct AssertionHeader<'a> {
    alg: &'static str,
    typ: &'static str,
    /// The key's own id, so Google can pick the right public half when a
    /// service account has several.
    #[serde(skip_serializing_if = "Option::is_none")]
    kid: Option<&'a str>,
}

/// The claims of an RS256 assertion.
#[derive(Debug, Clone, Serialize)]
struct AssertionClaims<'a> {
    iss: &'a str,
    scope: &'static str,
    aud: &'a str,
    iat: u64,
    exp: u64,
}

/// A Google service-account key, as the JSON document Google issues.
///
/// Parsed from the file rather than re-typed field by field: the document is
/// what a user downloads and pastes, and asking them to transcribe five
/// values out of it is five chances to get one wrong.
///
/// The private key is a credential; the hand-written [`fmt::Debug`] is what
/// keeps it out of a provisioning log.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct ServiceAccountKey {
    /// The project machines are provisioned into.
    pub project_id: String,
    /// The service account's address, which is the assertion's `iss`.
    pub client_email: String,
    /// PEM-encoded PKCS#8 RSA private key.
    pub private_key: String,
    /// Which of the account's keys this is.
    #[serde(default)]
    pub private_key_id: Option<String>,
    /// The token endpoint, which is the assertion's `aud`.
    ///
    /// Taken from the document rather than hardcoded: it is a field Google
    /// puts there, and the assertion is only valid for the audience it
    /// names, so reading it is both simpler and correct for a key issued
    /// against another endpoint.
    #[serde(default = "default_token_uri")]
    pub token_uri: String,
}

/// Where Google issues tokens, for a key that names no endpoint.
fn default_token_uri() -> String {
    "https://oauth2.googleapis.com/token".to_owned()
}

impl fmt::Debug for ServiceAccountKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceAccountKey")
            .field("project_id", &self.project_id)
            .field("client_email", &self.client_email)
            .field("private_key_id", &self.private_key_id)
            .field("token_uri", &self.token_uri)
            .finish_non_exhaustive()
    }
}

impl ServiceAccountKey {
    /// Reads a key out of the JSON document Google issues.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Malformed`] if the document is not a service
    /// account key — which is a credential to fix rather than a failure to
    /// retry.
    pub fn parse(document: &str) -> Result<Self, ProviderError> {
        serde_json::from_str(document).map_err(|_| {
            ProviderError::Malformed(
                "this is not a Google service-account key: it needs project_id, client_email \
                 and private_key",
            )
        })
    }

    /// The signed assertion that proves possession of the private key.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Malformed`] if the private key is not a
    /// PEM-encoded PKCS#8 RSA key, or if the claims do not serialize.
    pub fn assertion(&self, now_unix: u64) -> Result<String, ProviderError> {
        let encoder = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header = encoder.encode(
            serde_json::to_vec(&AssertionHeader {
                alg: "RS256",
                typ: "JWT",
                kid: self.private_key_id.as_deref(),
            })
            .map_err(|_| ProviderError::Malformed("a JWT header did not serialize"))?,
        );
        let claims = encoder.encode(
            serde_json::to_vec(&AssertionClaims {
                iss: &self.client_email,
                scope: SCOPE,
                aud: &self.token_uri,
                iat: now_unix,
                exp: now_unix.saturating_add(ASSERTION_LIFETIME_SECONDS),
            })
            .map_err(|_| ProviderError::Malformed("JWT claims did not serialize"))?,
        );

        let signing_input = format!("{header}.{claims}");
        let key = rsa::RsaPrivateKey::from_pkcs8_pem(&self.private_key).map_err(|_| {
            ProviderError::Malformed(
                "the service-account key's private_key is not a PEM-encoded PKCS#8 RSA key",
            )
        })?;
        // PKCS#1 v1.5 signs without an RNG, which is what keeps this
        // deterministic — and therefore assertable — and what lets it
        // compile on a target with no random source.
        let signature = SigningKey::<Sha256>::new(key).sign(signing_input.as_bytes());

        Ok(format!(
            "{signing_input}.{}",
            encoder.encode(signature.to_bytes())
        ))
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
#[derive(Clone)]
pub struct TokenCache {
    key: ServiceAccountKey,
    cached: Option<CachedToken>,
}

impl fmt::Debug for TokenCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenCache")
            .field("key", &self.key)
            .field("holds_token", &self.cached.is_some())
            .finish_non_exhaustive()
    }
}

impl TokenCache {
    /// An empty cache for one service account.
    #[must_use]
    pub const fn new(key: ServiceAccountKey) -> Self {
        Self { key, cached: None }
    }

    /// The service account this cache mints for.
    #[must_use]
    pub const fn key(&self) -> &ServiceAccountKey {
        &self.key
    }

    /// Forgets the current token, so the next call mints a fresh one.
    pub fn invalidate(&mut self) {
        self.cached = None;
    }

    /// The bearer token to present, minting one if the cache is cold or due.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Rejected`] if Google refuses the assertion,
    /// [`ProviderError::Malformed`] if the key cannot sign one, or a
    /// transport error if the request never completed.
    #[expect(
        clippy::future_not_send,
        reason = "`Send`-ness follows from the concrete transport: the deployed one is a \
                  unit struct and the Worker is single-threaded, while the test transport \
                  is deliberately not `Sync`. Bounding `T: Sync` here would forbid the \
                  recorded transport the whole driver is tested against."
    )]
    pub async fn access_token<T: HttpTransport, C: MonotonicClock, W: WallClock>(
        &mut self,
        transport: &T,
        clock: &C,
        wall_clock: &W,
    ) -> Result<String, ProviderError> {
        let elapsed = clock.elapsed_seconds();
        if let Some(cached) = &self.cached
            && elapsed < cached.refresh_after
        {
            return Ok(cached.value.clone());
        }

        let assertion = self.key.assertion(wall_clock.unix_seconds())?;
        let request = HttpRequest::new(Method::Post, self.key.token_uri.clone())
            .form_body(&[("grant_type", GRANT_TYPE), ("assertion", &assertion)]);

        let response = transport.send(request).await?;
        if !response.is_success() {
            // The body of a token failure names what is wrong with the
            // assertion — a clock skew, a revoked key, a disabled account —
            // and is the only way to tell them apart without guessing.
            return Err(ProviderError::Rejected(format!(
                "Google refused the service account (HTTP {}): {}",
                response.status,
                response.body_text()
            )));
        }

        let token: TokenResponse = response.json()?;
        let lifetime = token.expires_in.saturating_mul(REFRESH_AT_PERCENT) / 100;
        self.cached = Some(CachedToken {
            value: token.access_token.clone(),
            refresh_after: elapsed.saturating_add(lifetime),
        });

        tracing::debug!(
            service_account = %self.key.client_email,
            reuse_for_seconds = lifetime,
            "minted a Google access token"
        );
        Ok(token.access_token)
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;

    use super::{GRANT_TYPE, SCOPE, ServiceAccountKey, TokenCache};
    use crate::clock::{ManualClock, ManualWallClock};
    use crate::http::{HttpResponse, Method};
    use crate::testing::RecordedTransport;

    /// 2026-08-29T12:00:00Z.
    const SIGNED_AT: u64 = 1_788_004_800;

    const KEY: &str = include_str!("../../fixtures/gcp/service_account.json");

    fn key() -> ServiceAccountKey {
        ServiceAccountKey::parse(KEY).expect("the key fixture parses")
    }

    fn token(value: &str, expires_in: u64) -> HttpResponse {
        HttpResponse::new(
            200,
            serde_json::to_vec(&serde_json::json!({
                "access_token": value,
                "expires_in": expires_in,
                "token_type": "Bearer",
            }))
            .expect("serialize"),
        )
    }

    fn part(assertion: &str, index: usize) -> serde_json::Value {
        let encoded = assertion
            .split('.')
            .nth(index)
            .expect("an assertion has three parts");
        serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(encoded)
                .expect("each part is base64url"),
        )
        .expect("the part is JSON")
    }

    #[test]
    fn a_key_is_read_out_of_the_document_google_issues() {
        let key = key();
        assert_eq!(key.project_id, "flyco-sessions");
        assert_eq!(
            key.client_email,
            "flyco-provisioner@flyco-sessions.iam.gserviceaccount.com"
        );
        // Read from the document, because the assertion is only valid for
        // the audience it names.
        assert_eq!(key.token_uri, "https://oauth2.googleapis.com/token");
    }

    #[test]
    fn something_that_is_not_a_service_account_key_is_refused() {
        ServiceAccountKey::parse("{}").expect_err("a credential to fix, not a failure to retry");
        ServiceAccountKey::parse("not json at all").expect_err("neither is this");
    }

    #[test]
    fn an_assertion_claims_the_account_the_scope_and_an_hour() {
        let assertion = key().assertion(SIGNED_AT).expect("sign");

        let header = part(&assertion, 0);
        assert_eq!(header["alg"], "RS256");
        assert_eq!(header["typ"], "JWT");
        // The key id, so Google picks the right public half when the account
        // has several.
        assert_eq!(header["kid"], "9f1c2b3a4d5e6f708192a3b4c5d6e7f809a1b2c3");

        let claims = part(&assertion, 1);
        assert_eq!(
            claims["iss"],
            "flyco-provisioner@flyco-sessions.iam.gserviceaccount.com"
        );
        assert_eq!(claims["aud"], "https://oauth2.googleapis.com/token");
        assert_eq!(claims["scope"], SCOPE);
        assert_eq!(claims["iat"], SIGNED_AT);
        // Google caps an assertion at an hour and refuses anything longer.
        assert_eq!(claims["exp"], SIGNED_AT + 3_600);
    }

    #[test]
    fn the_signature_is_deterministic_and_covers_the_claims() {
        // PKCS#1 v1.5 uses no RNG, which is what makes this assertable at
        // all — and what lets it run on a target with no random source.
        let first = key().assertion(SIGNED_AT).expect("sign");
        assert_eq!(first, key().assertion(SIGNED_AT).expect("sign"));

        let later = key().assertion(SIGNED_AT + 1).expect("sign");
        assert_ne!(
            first.rsplit('.').next(),
            later.rsplit('.').next(),
            "the claims are signed, so a different `iat` signs differently"
        );

        // An RSA-2048 signature is 256 bytes, base64url-encoded.
        let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(first.rsplit('.').next().expect("a signature"))
            .expect("the signature is base64url");
        assert_eq!(signature.len(), 256);
    }

    #[test]
    fn a_key_that_is_not_a_pkcs8_pem_cannot_sign() {
        let mut key = key();
        key.private_key =
            "-----BEGIN PRIVATE KEY-----\nnope\n-----END PRIVATE KEY-----\n".to_owned();
        key.assertion(SIGNED_AT)
            .expect_err("an unusable key must fail where it can be named");
    }

    #[tokio::test]
    async fn a_token_request_carries_the_grant_and_the_assertion_and_nothing_else() {
        let transport = RecordedTransport::new(vec![token("ya29.first", 3_599)]);
        let mut cache = TokenCache::new(key());

        cache
            .access_token(
                &transport,
                &ManualClock::new(),
                &ManualWallClock::at(SIGNED_AT),
            )
            .await
            .expect("mint a token");

        let request = transport.request(0);
        assert_eq!(request.method, Method::Post);
        assert_eq!(request.url, "https://oauth2.googleapis.com/token");

        let body = request.body_text().expect("UTF-8");
        let fields: Vec<(String, String)> = url::form_urlencoded::parse(body.as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0], ("grant_type".to_owned(), GRANT_TYPE.to_owned()));
        assert_eq!(fields[1].0, "assertion");
        assert_eq!(
            fields[1].1,
            key().assertion(SIGNED_AT).expect("sign"),
            "the assertion on the wire is the one the key signs"
        );
    }

    #[tokio::test]
    async fn a_live_token_is_reused_and_a_due_one_is_re_minted() {
        let transport = RecordedTransport::new(vec![
            token("ya29.first", 3_600),
            token("ya29.second", 3_600),
        ]);
        let clock = ManualClock::new();
        let wall_clock = ManualWallClock::at(SIGNED_AT);
        let mut cache = TokenCache::new(key());

        assert_eq!(
            cache
                .access_token(&transport, &clock, &wall_clock)
                .await
                .expect("mint"),
            "ya29.first"
        );

        // Still inside 80% of an hour: no second request.
        clock.advance(2_879);
        assert_eq!(
            cache
                .access_token(&transport, &clock, &wall_clock)
                .await
                .expect("reuse"),
            "ya29.first"
        );
        assert_eq!(transport.request_count(), 1);

        clock.advance(1);
        assert_eq!(
            cache
                .access_token(&transport, &clock, &wall_clock)
                .await
                .expect("re-mint"),
            "ya29.second"
        );
    }

    #[tokio::test]
    async fn invalidating_the_cache_forces_a_fresh_token_before_it_is_due() {
        let transport = RecordedTransport::new(vec![
            token("ya29.first", 3_600),
            token("ya29.second", 3_600),
        ]);
        let clock = ManualClock::new();
        let wall_clock = ManualWallClock::at(SIGNED_AT);
        let mut cache = TokenCache::new(key());

        cache
            .access_token(&transport, &clock, &wall_clock)
            .await
            .expect("mint");
        cache.invalidate();

        assert_eq!(
            cache
                .access_token(&transport, &clock, &wall_clock)
                .await
                .expect("re-mint"),
            "ya29.second"
        );
        assert_eq!(transport.request_count(), 2);
    }

    #[tokio::test]
    async fn a_refused_service_account_is_reported_rather_than_cached() {
        let transport = RecordedTransport::new(vec![HttpResponse::new(
            400,
            br#"{"error":"invalid_grant","error_description":"Invalid JWT Signature."}"#.to_vec(),
        )]);
        let mut cache = TokenCache::new(key());

        let error = cache
            .access_token(
                &transport,
                &ManualClock::new(),
                &ManualWallClock::at(SIGNED_AT),
            )
            .await
            .expect_err("a rejected assertion is an error");
        assert!(error.to_string().contains("Invalid JWT Signature"));
    }

    #[test]
    fn a_key_never_debug_prints_its_private_half() {
        let rendered = format!("{:?}", key());
        assert!(!rendered.contains("BEGIN PRIVATE KEY"));
        assert!(!format!("{:?}", TokenCache::new(key())).contains("BEGIN PRIVATE KEY"));
        assert!(rendered.contains("flyco-sessions"));
    }
}
