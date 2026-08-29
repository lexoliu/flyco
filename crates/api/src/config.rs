//! Deployment configuration, read once when the router is built.
//!
//! On the Worker these values come from the Cloudflare `env` object — the
//! non-secret ones from `[cloudflare.vars]` in `Skyzen.toml`, the secret
//! ones from `wrangler secret put`. On native they come from the process
//! environment. A missing or malformed *required* value is a startup
//! failure, never a default.
//!
//! Two bindings are optional, and each absence disables a capability rather
//! than weakening one: with no VAPID key this deployment sends no push, and
//! with no webhook secret it accepts no GitHub deliveries.

use url::Url;

use crate::crypto::{KEY_LEN, TokenCipher};

/// Names of the bindings the control plane reads.
///
/// They are identical on both platforms so a `.dev.vars` file and a deployed
/// Worker are configured the same way.
pub mod var {
    /// GitHub OAuth app client id. Public; lives in `[cloudflare.vars]`.
    pub const GITHUB_CLIENT_ID: &str = "FLYCO_GITHUB_CLIENT_ID";
    /// GitHub OAuth app client secret. Secret; `wrangler secret put`.
    pub const GITHUB_CLIENT_SECRET: &str = "FLYCO_GITHUB_CLIENT_SECRET";
    /// Absolute URL GitHub redirects back to. Public.
    pub const REDIRECT_URI: &str = "FLYCO_REDIRECT_URI";
    /// AES-256 key for sealing third-party tokens, hex-encoded. Secret.
    pub const ENCRYPTION_KEY: &str = "FLYCO_ENCRYPTION_KEY";
    /// VAPID public key browsers subscribe against, base64url unpadded.
    ///
    /// Optional: a deployment that never sends a push notification needs no
    /// key pair, and refusing to start without one would make web push a
    /// requirement rather than a feature.
    pub const VAPID_PUBLIC_KEY: &str = "FLYCO_VAPID_PUBLIC_KEY";
    /// Shared secret GitHub signs webhook deliveries with. Secret;
    /// `wrangler secret put`.
    ///
    /// Optional, and its absence is a decision rather than an oversight:
    /// a deployment with no secret has nothing to verify a delivery
    /// against, so it accepts no webhooks at all and says so. Defaulting
    /// to "no signature required" would turn the one thing standing
    /// between a forgery and a live session into a missing environment
    /// variable.
    pub const GITHUB_WEBHOOK_SECRET: &str = "FLYCO_GITHUB_WEBHOOK_SECRET";
}

/// Why the control plane refused to start.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A required binding is absent, empty, or not a string.
    #[error("required configuration `{0}` is missing")]
    Missing(&'static str),
    /// `FLYCO_REDIRECT_URI` is not an absolute URL.
    #[error("configuration `{name}` is not an absolute URL: {source}")]
    NotAUrl {
        /// The offending binding.
        name: &'static str,
        /// What the URL parser objected to.
        source: url::ParseError,
    },
    /// `FLYCO_ENCRYPTION_KEY` is not hex, or is not 32 bytes long.
    #[error("configuration `{0}` must be exactly {KEY_LEN} bytes of lowercase hex")]
    NotAKey(&'static str),
    /// The Cloudflare `env` object was not reachable during startup.
    #[error("the Cloudflare Workers environment is not available")]
    NoEnvironment,
}

/// Everything the control plane needs from its deployment environment.
///
/// Constructed once by [`from_environment`](Self::from_environment) and shared
/// with handlers through skyzen's `State`.
#[derive(Clone)]
pub struct ApiConfig {
    github_client_id: String,
    github_client_secret: String,
    redirect_uri: Url,
    encryption_key: [u8; KEY_LEN],
    vapid_public_key: Option<String>,
    github_webhook_secret: Option<String>,
}

impl core::fmt::Debug for ApiConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ApiConfig")
            .field("github_client_id", &self.github_client_id)
            .field("redirect_uri", &self.redirect_uri.as_str())
            .finish_non_exhaustive()
    }
}

impl ApiConfig {
    /// Assembles a configuration from already-resolved values.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if the redirect URI is not absolute or the
    /// encryption key is not 32 hex-encoded bytes.
    pub fn new(
        github_client_id: String,
        github_client_secret: String,
        redirect_uri: &str,
        encryption_key_hex: &str,
    ) -> Result<Self, ConfigError> {
        let redirect_uri = Url::parse(redirect_uri).map_err(|source| ConfigError::NotAUrl {
            name: var::REDIRECT_URI,
            source,
        })?;

        let mut encryption_key = [0_u8; KEY_LEN];
        hex::decode_to_slice(encryption_key_hex, &mut encryption_key)
            .map_err(|_| ConfigError::NotAKey(var::ENCRYPTION_KEY))?;

        Ok(Self {
            vapid_public_key: None,
            github_webhook_secret: None,
            github_client_id,
            github_client_secret,
            redirect_uri,
            encryption_key,
        })
    }

    /// Reads the configuration from the deployment environment.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] on the first binding that is absent or
    /// unusable — the control plane never falls back to a default.
    pub fn from_environment() -> Result<Self, ConfigError> {
        let mut config = Self::new(
            read_var(var::GITHUB_CLIENT_ID)?,
            read_var(var::GITHUB_CLIENT_SECRET)?,
            &read_var(var::REDIRECT_URI)?,
            &read_var(var::ENCRYPTION_KEY)?,
        )?;
        config.vapid_public_key = read_var(var::VAPID_PUBLIC_KEY).ok();
        config.github_webhook_secret = read_var(var::GITHUB_WEBHOOK_SECRET).ok();
        Ok(config)
    }

    /// The VAPID public key browsers subscribe against, when this
    /// deployment has one.
    #[must_use]
    pub fn vapid_public_key(&self) -> Option<&str> {
        self.vapid_public_key.as_deref()
    }

    /// Replaces the VAPID public key, for tests and for callers that resolve
    /// configuration themselves.
    #[must_use]
    pub fn with_vapid_public_key(mut self, key: impl Into<String>) -> Self {
        self.vapid_public_key = Some(key.into());
        self
    }

    /// The secret GitHub signs this deployment's webhook deliveries with.
    ///
    /// `None` means this deployment accepts no webhooks: there is nothing
    /// to check a signature against, and an unverified body is never read.
    #[must_use]
    pub fn github_webhook_secret(&self) -> Option<&str> {
        self.github_webhook_secret.as_deref()
    }

    /// Replaces the GitHub webhook secret, for tests and for callers that
    /// resolve configuration themselves.
    #[must_use]
    pub fn with_github_webhook_secret(mut self, secret: impl Into<String>) -> Self {
        self.github_webhook_secret = Some(secret.into());
        self
    }

    /// GitHub OAuth app client id.
    #[must_use]
    pub fn github_client_id(&self) -> &str {
        &self.github_client_id
    }

    /// GitHub OAuth app client secret.
    #[must_use]
    pub fn github_client_secret(&self) -> &str {
        &self.github_client_secret
    }

    /// Absolute URL GitHub redirects the browser back to.
    #[must_use]
    pub const fn redirect_uri(&self) -> &Url {
        &self.redirect_uri
    }

    /// Cipher that seals third-party tokens before they reach D1.
    #[must_use]
    pub const fn token_cipher(&self) -> TokenCipher {
        TokenCipher::new(self.encryption_key)
    }
}

#[cfg(target_arch = "wasm32")]
fn read_var(name: &'static str) -> Result<String, ConfigError> {
    let env = skyzen::runtime::wasm::current_env().ok_or(ConfigError::NoEnvironment)?;
    let value =
        skyzen_cloudflare::required_secret(&env, name).map_err(|_| ConfigError::Missing(name))?;
    reject_empty(name, value)
}

#[cfg(not(target_arch = "wasm32"))]
fn read_var(name: &'static str) -> Result<String, ConfigError> {
    let value = std::env::var(name).map_err(|_| ConfigError::Missing(name))?;
    reject_empty(name, value)
}

fn reject_empty(name: &'static str, value: String) -> Result<String, ConfigError> {
    if value.trim().is_empty() {
        return Err(ConfigError::Missing(name));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::{ApiConfig, ConfigError, var};

    use crate::testing::{CLIENT_SECRET, ENCRYPTION_KEY_HEX, test_config};

    const KEY_HEX: &str = ENCRYPTION_KEY_HEX;

    #[test]
    fn a_short_encryption_key_is_rejected() {
        let error = ApiConfig::new(
            "id".to_owned(),
            "secret".to_owned(),
            "https://flyco.test/cb",
            "00112233",
        )
        .expect_err("a 4-byte key must be rejected");
        assert!(matches!(error, ConfigError::NotAKey(var::ENCRYPTION_KEY)));
    }

    #[test]
    fn a_relative_redirect_uri_is_rejected() {
        let error = ApiConfig::new(
            "id".to_owned(),
            "secret".to_owned(),
            "/v1/auth/github/callback",
            KEY_HEX,
        )
        .expect_err("a relative redirect URI must be rejected");
        assert!(matches!(error, ConfigError::NotAUrl { .. }));
    }

    #[test]
    fn the_debug_rendering_never_shows_a_secret() {
        let rendered = format!(
            "{:?}",
            test_config().with_github_webhook_secret("a-webhook-secret")
        );
        assert!(!rendered.contains(CLIENT_SECRET));
        assert!(!rendered.contains(KEY_HEX));
        assert!(!rendered.contains("a-webhook-secret"));
    }

    #[test]
    fn a_deployment_without_a_webhook_secret_accepts_no_webhooks() {
        // The absence of a secret is the whole refusal: there is nothing to
        // verify a delivery against, and `webhooks` reads that as "this
        // deployment accepts none" rather than "verification is optional".
        assert!(test_config().github_webhook_secret().is_none());
    }
}
