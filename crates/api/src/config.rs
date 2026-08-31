//! Deployment configuration, read once when the router is built.
//!
//! On the Worker these values come from the Cloudflare `env` object — the
//! non-secret ones from `[cloudflare.vars]` in `Skyzen.toml`, the secret
//! ones from `wrangler secret put`. On native they come from the process
//! environment. A missing or malformed *required* value is a startup
//! failure, never a default.
//!
//! The VAPID public key is optional because it advertises whether this build
//! can subscribe a browser. The GitHub webhook secret is required: accepting
//! CI deliveries is a product capability and cannot be left unusable at
//! runtime.

use url::Url;

use flyco_core::HarnessKind;

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
    /// Per-harness OAuth client, one set per vendor.
    ///
    /// Optional as a group: a deployment links a harness account only if it
    /// holds a registered OAuth client for that vendor. Flyco does not ship
    /// endpoints or client ids of its own — inventing them would be a guess
    /// about somebody else's service — so an unconfigured harness reports
    /// itself unlinkable rather than sending a browser somewhere hopeful.
    pub const CLAUDE_OAUTH: HarnessOauthVars = HarnessOauthVars {
        authorize_url: "FLYCO_CLAUDE_OAUTH_AUTHORIZE_URL",
        token_url: "FLYCO_CLAUDE_OAUTH_TOKEN_URL",
        client_id: "FLYCO_CLAUDE_OAUTH_CLIENT_ID",
        client_secret: "FLYCO_CLAUDE_OAUTH_CLIENT_SECRET",
        scope: "FLYCO_CLAUDE_OAUTH_SCOPE",
    };

    /// The same set for Codex.
    pub const CODEX_OAUTH: HarnessOauthVars = HarnessOauthVars {
        authorize_url: "FLYCO_CODEX_OAUTH_AUTHORIZE_URL",
        token_url: "FLYCO_CODEX_OAUTH_TOKEN_URL",
        client_id: "FLYCO_CODEX_OAUTH_CLIENT_ID",
        client_secret: "FLYCO_CODEX_OAUTH_CLIENT_SECRET",
        scope: "FLYCO_CODEX_OAUTH_SCOPE",
    };

    /// The five variable names one harness's OAuth client is read from.
    #[derive(Debug, Clone, Copy)]
    pub struct HarnessOauthVars {
        /// Where the browser is sent to approve.
        pub authorize_url: &'static str,
        /// Where the authorization code is exchanged.
        pub token_url: &'static str,
        /// The registered client id.
        pub client_id: &'static str,
        /// The registered client secret.
        pub client_secret: &'static str,
        /// Space-separated scopes to request.
        pub scope: &'static str,
    }

    /// VAPID public key browsers subscribe against, base64url unpadded.
    ///
    /// Optional: a deployment that never sends a push notification needs no
    /// key pair, and refusing to start without one would make web push a
    /// requirement rather than a feature.
    pub const VAPID_PUBLIC_KEY: &str = "FLYCO_VAPID_PUBLIC_KEY";
    /// Shared secret GitHub signs webhook deliveries with. Secret;
    /// `wrangler secret put`.
    ///
    /// Required: a deployment with no secret has nothing to verify a delivery
    /// against, so it must fail during startup rather than leave a product
    /// route running in an unusable state.
    pub const GITHUB_WEBHOOK_SECRET: &str = "FLYCO_GITHUB_WEBHOOK_SECRET";
}

/// Cloudflare binding names the queue consumer resolves for itself.
///
/// `#[skyzen::main]` wires the manifest's services into the router, but a
/// `#[skyzen::queue]` handler is not a request and receives no wiring at all
/// — the platform hands it the raw environment and it opens what it needs.
/// These names therefore mirror `[cloudflare.database.main]` and
/// `[cloudflare.service.provisioning]` in `Skyzen.toml`, and a deployment
/// where they disagree fails loudly on its first job rather than quietly.
pub mod binding {
    /// D1 binding holding the control plane's one database.
    pub const DATABASE: &str = "DB";
    /// Queue binding the provisioning jobs are produced to and consumed
    /// from.
    pub const PROVISIONING: &str = "PROVISIONING";
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
    github_webhook_secret: String,
    claude_oauth: Option<HarnessOauthClient>,
    codex_oauth: Option<HarnessOauthClient>,
}

/// One vendor's registered OAuth client.
///
/// Every field is deployment configuration. Flyco ships no client of its own
/// for either vendor: both require registration, and inventing endpoints or
/// a client id would be a guess about somebody else's service.
#[derive(Clone)]
pub struct HarnessOauthClient {
    /// Where the browser is sent to approve.
    pub authorize_url: Url,
    /// Where the authorization code is exchanged.
    pub token_url: Url,
    /// The registered client id.
    pub client_id: String,
    /// The registered client secret.
    pub client_secret: String,
    /// Space-separated scopes to request.
    pub scope: String,
}

impl core::fmt::Debug for HarnessOauthClient {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HarnessOauthClient")
            .field("authorize_url", &self.authorize_url.as_str())
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
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
        github_webhook_secret: String,
    ) -> Result<Self, ConfigError> {
        let github_webhook_secret =
            reject_empty(var::GITHUB_WEBHOOK_SECRET, github_webhook_secret)?;
        let redirect_uri = Url::parse(redirect_uri).map_err(|source| ConfigError::NotAUrl {
            name: var::REDIRECT_URI,
            source,
        })?;

        let mut encryption_key = [0_u8; KEY_LEN];
        hex::decode_to_slice(encryption_key_hex, &mut encryption_key)
            .map_err(|_| ConfigError::NotAKey(var::ENCRYPTION_KEY))?;

        Ok(Self {
            vapid_public_key: None,
            claude_oauth: None,
            codex_oauth: None,
            github_webhook_secret,
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
        Self::read_with(read_var)
    }

    /// Reads the configuration out of a Cloudflare environment handed in
    /// explicitly.
    ///
    /// [`from_environment`](Self::from_environment) reads the ambient one,
    /// which the runtime only publishes while the router is being built. A
    /// queue consumer is not a request and runs outside that window, so it
    /// passes the environment the platform gave it.
    ///
    /// # Errors
    ///
    /// The same as [`from_environment`](Self::from_environment).
    #[cfg(target_arch = "wasm32")]
    pub fn from_worker_env(env: &skyzen::runtime::wasm::Env) -> Result<Self, ConfigError> {
        Self::read_with(|name| {
            let value = skyzen_cloudflare::required_secret(env, name)
                .map_err(|_| ConfigError::Missing(name))?;
            reject_empty(name, value)
        })
    }

    /// Assembles the configuration from a source of named values.
    ///
    /// One reading order for every environment, so a binding that is
    /// required in one place cannot become optional in another.
    fn read_with(
        read: impl Fn(&'static str) -> Result<String, ConfigError>,
    ) -> Result<Self, ConfigError> {
        let mut config = Self::new(
            read(var::GITHUB_CLIENT_ID)?,
            read(var::GITHUB_CLIENT_SECRET)?,
            &read(var::REDIRECT_URI)?,
            &read(var::ENCRYPTION_KEY)?,
            read(var::GITHUB_WEBHOOK_SECRET)?,
        )?;
        config.vapid_public_key = read(var::VAPID_PUBLIC_KEY).ok();
        config.claude_oauth = read_harness_oauth(&read, var::CLAUDE_OAUTH);
        config.codex_oauth = read_harness_oauth(&read, var::CODEX_OAUTH);
        Ok(config)
    }

    /// The OAuth client registered for one harness, when this deployment
    /// holds one.
    #[must_use]
    pub const fn harness_oauth(&self, harness: HarnessKind) -> Option<&HarnessOauthClient> {
        match harness {
            HarnessKind::ClaudeCode => self.claude_oauth.as_ref(),
            HarnessKind::Codex => self.codex_oauth.as_ref(),
        }
    }

    /// Replaces one harness's OAuth client, for tests and for callers that
    /// resolve configuration themselves.
    #[must_use]
    pub fn with_harness_oauth(mut self, harness: HarnessKind, client: HarnessOauthClient) -> Self {
        match harness {
            HarnessKind::ClaudeCode => self.claude_oauth = Some(client),
            HarnessKind::Codex => self.codex_oauth = Some(client),
        }
        self
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
    #[must_use]
    pub fn github_webhook_secret(&self) -> &str {
        &self.github_webhook_secret
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

    /// The base URL a session's daemon phones home to.
    ///
    /// Derived from the OAuth redirect URI's origin rather than configured
    /// separately: the two are the same deployment by construction, and a
    /// second variable would be one more thing that can be set to the wrong
    /// host — which for this one means every machine flyco provisions boots
    /// pointing at somewhere else.
    #[must_use]
    pub fn control_plane_url(&self) -> String {
        let mut base = self.redirect_uri.clone();
        base.set_path("/");
        base.set_query(None);
        base.set_fragment(None);
        base.to_string()
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

/// Reads one harness's OAuth client, or nothing if any part is missing.
///
/// All or nothing on purpose: a half-configured client would fail at the
/// vendor's door with an error the user cannot act on, where an absent one
/// says plainly that this deployment cannot link that harness.
fn read_harness_oauth(
    read: &impl Fn(&'static str) -> Result<String, ConfigError>,
    vars: var::HarnessOauthVars,
) -> Option<HarnessOauthClient> {
    Some(HarnessOauthClient {
        authorize_url: read(vars.authorize_url).ok()?.parse().ok()?,
        token_url: read(vars.token_url).ok()?.parse().ok()?,
        client_id: read(vars.client_id).ok()?,
        client_secret: read(vars.client_secret).ok()?,
        scope: read(vars.scope).ok()?,
    })
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
            "webhook-secret".to_owned(),
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
            "webhook-secret".to_owned(),
        )
        .expect_err("a relative redirect URI must be rejected");
        assert!(matches!(error, ConfigError::NotAUrl { .. }));
    }

    #[test]
    fn an_empty_webhook_secret_is_rejected() {
        let error = ApiConfig::new(
            "id".to_owned(),
            "secret".to_owned(),
            "https://flyco.test/cb",
            KEY_HEX,
            String::new(),
        )
        .expect_err("an empty webhook secret must be rejected at startup");
        assert!(matches!(
            error,
            ConfigError::Missing(var::GITHUB_WEBHOOK_SECRET)
        ));
    }

    #[test]
    fn the_debug_rendering_never_shows_a_secret() {
        let rendered = format!("{:?}", test_config());
        assert!(!rendered.contains(CLIENT_SECRET));
        assert!(!rendered.contains(KEY_HEX));
        assert!(!rendered.contains(super::super::testing::GITHUB_WEBHOOK_SECRET));
    }
}
