//! Deployment configuration, read once when the router is built.
//!
//! On the Worker these values come from the Cloudflare `env` object — the
//! non-secret ones from `[cloudflare.vars]` in `Skyzen.toml`, the secret
//! ones from `wrangler secret put`. On native they come from the process
//! environment. A missing or malformed *required* value is a startup
//! failure, never a default.
//!
//! GitHub webhook verification and Web Push are product capabilities, so both
//! signing identities are required at startup like the OAuth identity.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use url::Url;
use web_push_native::p256::ecdsa::SigningKey;

use crate::crypto::{KEY_LEN, TokenCipher};
use crate::provider_oauth::{AZURE_CALLBACK_PATH, GCP_CALLBACK_PATH};

/// Names of the bindings the control plane reads.
///
/// They are identical on both platforms so a `.dev.vars` file and a deployed
/// Worker are configured the same way.
pub mod var {
    /// GitHub OAuth app client id. Public; lives in `[cloudflare.vars]`.
    pub const GITHUB_CLIENT_ID: &str = "FLYCO_GITHUB_CLIENT_ID";
    /// GitHub OAuth app client secret. Secret; `wrangler secret put`.
    pub const GITHUB_CLIENT_SECRET: &str = "FLYCO_GITHUB_CLIENT_SECRET";
    /// Claude Code OAuth client id. Public; lives in `[cloudflare.vars]`.
    ///
    /// Anthropic issues no client secret for this flow — it is a public
    /// client with PKCE — so the id is deployment configuration rather than
    /// a secret, and a deployment presenting its own registered client sets
    /// this one variable.
    pub const CLAUDE_OAUTH_CLIENT_ID: &str = "FLYCO_CLAUDE_OAUTH_CLIENT_ID";
    /// Codex OAuth client id. Public; lives in `[cloudflare.vars]`.
    ///
    /// `OpenAI` issues no client secret for the device flow either — it is
    /// the same kind of public client — so this is deployment
    /// configuration rather than a secret, and a deployment presenting its
    /// own registered client sets this one variable.
    pub const CODEX_OAUTH_CLIENT_ID: &str = "FLYCO_CODEX_OAUTH_CLIENT_ID";
    /// Microsoft OAuth application client id. Public; lives in
    /// `[cloudflare.vars]`.
    ///
    /// Unlike the harness clients this one is a *confidential* client:
    /// "Sign in with Microsoft" is a redirect flow with a callback flyco
    /// serves, so Microsoft issues a client secret and the id alone is not
    /// enough to redeem anything.
    pub const AZURE_OAUTH_CLIENT_ID: &str = "FLYCO_AZURE_OAUTH_CLIENT_ID";
    /// Microsoft OAuth application client secret. Secret;
    /// `wrangler secret put`.
    pub const AZURE_OAUTH_CLIENT_SECRET: &str = "FLYCO_AZURE_OAUTH_CLIENT_SECRET";
    /// Google OAuth client id. Public; lives in `[cloudflare.vars]`.
    pub const GOOGLE_OAUTH_CLIENT_ID: &str = "FLYCO_GOOGLE_OAUTH_CLIENT_ID";
    /// Google OAuth client secret. Secret; `wrangler secret put`.
    pub const GOOGLE_OAUTH_CLIENT_SECRET: &str = "FLYCO_GOOGLE_OAUTH_CLIENT_SECRET";
    /// Absolute URL GitHub redirects back to. Public.
    pub const REDIRECT_URI: &str = "FLYCO_REDIRECT_URI";
    /// AES-256 key for sealing third-party tokens, hex-encoded. Secret.
    pub const ENCRYPTION_KEY: &str = "FLYCO_ENCRYPTION_KEY";
    /// VAPID P-256 private key as raw base64url, unpadded. Secret.
    pub const VAPID_PRIVATE_KEY: &str = "FLYCO_VAPID_PRIVATE_KEY";
    /// Contact URI placed in every VAPID JWT, normally `mailto:`.
    pub const VAPID_SUBJECT: &str = "FLYCO_VAPID_SUBJECT";
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
    /// KV binding holding sessions, OAuth attempts and the catalog cache.
    ///
    /// Named here for the handlers that open it themselves — the queue
    /// consumer and the scheduled worker — because neither is a request and
    /// neither gets `#[skyzen::main]`'s service wiring.
    pub const AUTH_KV: &str = "AUTH_KV";
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
    /// The VAPID private key is not a raw P-256 private key.
    #[error("configuration `{0}` is not a base64url P-256 private key")]
    NotAVapidKey(&'static str),
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
    claude_oauth_client_id: String,
    codex_oauth_client_id: String,
    azure_oauth_client_id: String,
    azure_oauth_client_secret: String,
    google_oauth_client_id: String,
    google_oauth_client_secret: String,
    redirect_uri: Url,
    azure_oauth_redirect_uri: Url,
    gcp_oauth_redirect_uri: Url,
    encryption_key: [u8; KEY_LEN],
    vapid: VapidConfig,
    github_webhook_secret: String,
}

/// The application-server identity used to encrypt and sign Web Push.
#[derive(Clone)]
pub struct VapidConfig {
    signing_key: SigningKey,
    public_key: String,
    subject: Url,
}

impl core::fmt::Debug for VapidConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VapidConfig")
            .field("public_key", &self.public_key)
            .field("subject", &self.subject.as_str())
            .finish_non_exhaustive()
    }
}

impl core::fmt::Debug for ApiConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ApiConfig")
            .field("github_client_id", &self.github_client_id)
            .field("claude_oauth_client_id", &self.claude_oauth_client_id)
            .field("codex_oauth_client_id", &self.codex_oauth_client_id)
            .field("azure_oauth_client_id", &self.azure_oauth_client_id)
            .field("google_oauth_client_id", &self.google_oauth_client_id)
            .field("redirect_uri", &self.redirect_uri.as_str())
            .finish_non_exhaustive()
    }
}

/// The deployment's bindings as strings, before any of them is validated.
///
/// One field per name in [`var`]. A struct rather than eight positional
/// arguments, because every one of them is a string: a swapped pair would
/// type-check and fail only when a deployment tried to sign somebody in.
pub struct ApiSettings {
    /// [`var::GITHUB_CLIENT_ID`].
    pub github_client_id: String,
    /// [`var::GITHUB_CLIENT_SECRET`].
    pub github_client_secret: String,
    /// [`var::CLAUDE_OAUTH_CLIENT_ID`].
    pub claude_oauth_client_id: String,
    /// [`var::CODEX_OAUTH_CLIENT_ID`].
    pub codex_oauth_client_id: String,
    /// [`var::AZURE_OAUTH_CLIENT_ID`].
    pub azure_oauth_client_id: String,
    /// [`var::AZURE_OAUTH_CLIENT_SECRET`].
    pub azure_oauth_client_secret: String,
    /// [`var::GOOGLE_OAUTH_CLIENT_ID`].
    pub google_oauth_client_id: String,
    /// [`var::GOOGLE_OAUTH_CLIENT_SECRET`].
    pub google_oauth_client_secret: String,
    /// [`var::REDIRECT_URI`].
    pub redirect_uri: String,
    /// [`var::ENCRYPTION_KEY`], hex-encoded.
    pub encryption_key_hex: String,
    /// [`var::VAPID_PRIVATE_KEY`], raw base64url.
    pub vapid_private_key: String,
    /// [`var::VAPID_SUBJECT`].
    pub vapid_subject: String,
    /// [`var::GITHUB_WEBHOOK_SECRET`].
    pub github_webhook_secret: String,
}

impl core::fmt::Debug for ApiSettings {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ApiSettings")
            .field("github_client_id", &self.github_client_id)
            .field("claude_oauth_client_id", &self.claude_oauth_client_id)
            .field("codex_oauth_client_id", &self.codex_oauth_client_id)
            .field("azure_oauth_client_id", &self.azure_oauth_client_id)
            .field("google_oauth_client_id", &self.google_oauth_client_id)
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

impl ApiConfig {
    /// Assembles a configuration from already-resolved values.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if a required value is empty, the redirect
    /// URI is not absolute, or the encryption key is not 32 hex-encoded
    /// bytes.
    pub fn new(settings: ApiSettings) -> Result<Self, ConfigError> {
        let github_webhook_secret =
            reject_empty(var::GITHUB_WEBHOOK_SECRET, settings.github_webhook_secret)?;
        let claude_oauth_client_id =
            reject_empty(var::CLAUDE_OAUTH_CLIENT_ID, settings.claude_oauth_client_id)?;
        let codex_oauth_client_id =
            reject_empty(var::CODEX_OAUTH_CLIENT_ID, settings.codex_oauth_client_id)?;
        let azure_oauth_client_id =
            reject_empty(var::AZURE_OAUTH_CLIENT_ID, settings.azure_oauth_client_id)?;
        let azure_oauth_client_secret = reject_empty(
            var::AZURE_OAUTH_CLIENT_SECRET,
            settings.azure_oauth_client_secret,
        )?;
        let google_oauth_client_id =
            reject_empty(var::GOOGLE_OAUTH_CLIENT_ID, settings.google_oauth_client_id)?;
        let google_oauth_client_secret = reject_empty(
            var::GOOGLE_OAUTH_CLIENT_SECRET,
            settings.google_oauth_client_secret,
        )?;
        let redirect_uri =
            Url::parse(&settings.redirect_uri).map_err(|source| ConfigError::NotAUrl {
                name: var::REDIRECT_URI,
                source,
            })?;
        // Derived from the one configured origin rather than configured
        // twice: a callback URI that named a different host would be a
        // sign-in that never comes back, and there is nothing a second
        // variable could say that this one does not.
        let azure_oauth_redirect_uri = callback_uri(&redirect_uri, AZURE_CALLBACK_PATH);
        let gcp_oauth_redirect_uri = callback_uri(&redirect_uri, GCP_CALLBACK_PATH);

        let mut encryption_key = [0_u8; KEY_LEN];
        hex::decode_to_slice(&settings.encryption_key_hex, &mut encryption_key)
            .map_err(|_| ConfigError::NotAKey(var::ENCRYPTION_KEY))?;

        let private_key = URL_SAFE_NO_PAD
            .decode(&settings.vapid_private_key)
            .map_err(|_| ConfigError::NotAVapidKey(var::VAPID_PRIVATE_KEY))?;
        let signing_key = SigningKey::from_slice(&private_key)
            .map_err(|_| ConfigError::NotAVapidKey(var::VAPID_PRIVATE_KEY))?;
        let subject =
            Url::parse(&settings.vapid_subject).map_err(|source| ConfigError::NotAUrl {
                name: var::VAPID_SUBJECT,
                source,
            })?;

        Ok(Self {
            vapid: VapidConfig {
                public_key: URL_SAFE_NO_PAD
                    .encode(signing_key.verifying_key().to_encoded_point(false)),
                signing_key,
                subject,
            },
            github_webhook_secret,
            github_client_id: settings.github_client_id,
            github_client_secret: settings.github_client_secret,
            claude_oauth_client_id,
            codex_oauth_client_id,
            azure_oauth_client_id,
            azure_oauth_client_secret,
            google_oauth_client_id,
            google_oauth_client_secret,
            redirect_uri,
            azure_oauth_redirect_uri,
            gcp_oauth_redirect_uri,
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
        Self::new(ApiSettings {
            github_client_id: read(var::GITHUB_CLIENT_ID)?,
            github_client_secret: read(var::GITHUB_CLIENT_SECRET)?,
            claude_oauth_client_id: read(var::CLAUDE_OAUTH_CLIENT_ID)?,
            codex_oauth_client_id: read(var::CODEX_OAUTH_CLIENT_ID)?,
            azure_oauth_client_id: read(var::AZURE_OAUTH_CLIENT_ID)?,
            azure_oauth_client_secret: read(var::AZURE_OAUTH_CLIENT_SECRET)?,
            google_oauth_client_id: read(var::GOOGLE_OAUTH_CLIENT_ID)?,
            google_oauth_client_secret: read(var::GOOGLE_OAUTH_CLIENT_SECRET)?,
            redirect_uri: read(var::REDIRECT_URI)?,
            encryption_key_hex: read(var::ENCRYPTION_KEY)?,
            vapid_private_key: read(var::VAPID_PRIVATE_KEY)?,
            vapid_subject: read(var::VAPID_SUBJECT)?,
            github_webhook_secret: read(var::GITHUB_WEBHOOK_SECRET)?,
        })
    }

    /// The VAPID application-server identity.
    #[must_use]
    pub const fn vapid(&self) -> &VapidConfig {
        &self.vapid
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

    /// Claude Code OAuth client id this deployment presents to Anthropic.
    #[must_use]
    pub fn claude_oauth_client_id(&self) -> &str {
        &self.claude_oauth_client_id
    }

    /// Codex OAuth client id this deployment presents to `OpenAI`.
    #[must_use]
    pub fn codex_oauth_client_id(&self) -> &str {
        &self.codex_oauth_client_id
    }

    /// Microsoft OAuth client id this deployment presents.
    #[must_use]
    pub fn azure_oauth_client_id(&self) -> &str {
        &self.azure_oauth_client_id
    }

    /// Microsoft OAuth client secret this deployment presents.
    #[must_use]
    pub fn azure_oauth_client_secret(&self) -> &str {
        &self.azure_oauth_client_secret
    }

    /// Google OAuth client id this deployment presents.
    #[must_use]
    pub fn google_oauth_client_id(&self) -> &str {
        &self.google_oauth_client_id
    }

    /// Google OAuth client secret this deployment presents.
    #[must_use]
    pub fn google_oauth_client_secret(&self) -> &str {
        &self.google_oauth_client_secret
    }

    /// Absolute URL Microsoft redirects the browser back to.
    #[must_use]
    pub const fn azure_oauth_redirect_uri(&self) -> &Url {
        &self.azure_oauth_redirect_uri
    }

    /// Absolute URL Google redirects the browser back to.
    #[must_use]
    pub const fn gcp_oauth_redirect_uri(&self) -> &Url {
        &self.gcp_oauth_redirect_uri
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

impl VapidConfig {
    /// P-256 key used to sign one push-service audience.
    #[must_use]
    pub const fn signing_key(&self) -> &SigningKey {
        &self.signing_key
    }

    /// Uncompressed P-256 public key browsers subscribe against.
    #[must_use]
    pub fn public_key(&self) -> &str {
        &self.public_key
    }

    /// Contact URI placed in the VAPID JWT.
    #[must_use]
    pub const fn subject(&self) -> &Url {
        &self.subject
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

/// One of this deployment's own callback URIs, on the configured origin.
///
/// # Panics
///
/// Panics if `path` is not rooted, which would mean one of this crate's own
/// route constants was edited into a relative path.
fn callback_uri(origin: &Url, path: &str) -> Url {
    origin
        .join(path)
        .expect("a rooted path always resolves against an absolute redirect URI")
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

    use crate::testing::{CLIENT_SECRET, ENCRYPTION_KEY_HEX, test_config, test_settings};

    const KEY_HEX: &str = ENCRYPTION_KEY_HEX;

    #[test]
    fn a_short_encryption_key_is_rejected() {
        let error = ApiConfig::new(super::ApiSettings {
            encryption_key_hex: "00112233".to_owned(),
            ..test_settings()
        })
        .expect_err("a 4-byte key must be rejected");
        assert!(matches!(error, ConfigError::NotAKey(var::ENCRYPTION_KEY)));
    }

    #[test]
    fn a_relative_redirect_uri_is_rejected() {
        let error = ApiConfig::new(super::ApiSettings {
            redirect_uri: "/v1/auth/github/callback".to_owned(),
            ..test_settings()
        })
        .expect_err("a relative redirect URI must be rejected");
        assert!(matches!(error, ConfigError::NotAUrl { .. }));
    }

    #[test]
    fn an_empty_webhook_secret_is_rejected() {
        let error = ApiConfig::new(super::ApiSettings {
            github_webhook_secret: String::new(),
            ..test_settings()
        })
        .expect_err("an empty webhook secret must be rejected at startup");
        assert!(matches!(
            error,
            ConfigError::Missing(var::GITHUB_WEBHOOK_SECRET)
        ));
    }

    #[test]
    fn an_empty_claude_client_id_is_rejected() {
        let error = ApiConfig::new(super::ApiSettings {
            claude_oauth_client_id: String::new(),
            ..test_settings()
        })
        .expect_err("a deployment cannot run the Claude flow without a client id");
        assert!(matches!(
            error,
            ConfigError::Missing(var::CLAUDE_OAUTH_CLIENT_ID)
        ));
    }

    #[test]
    fn an_empty_codex_client_id_is_rejected() {
        let error = ApiConfig::new(super::ApiSettings {
            codex_oauth_client_id: String::new(),
            ..test_settings()
        })
        .expect_err("a deployment cannot run the Codex flow without a client id");
        assert!(matches!(
            error,
            ConfigError::Missing(var::CODEX_OAUTH_CLIENT_ID)
        ));
    }

    #[test]
    fn an_empty_microsoft_client_secret_is_rejected() {
        let error = ApiConfig::new(super::ApiSettings {
            azure_oauth_client_secret: String::new(),
            ..test_settings()
        })
        .expect_err("a deployment cannot run the Microsoft flow without a client secret");
        assert!(matches!(
            error,
            ConfigError::Missing(var::AZURE_OAUTH_CLIENT_SECRET)
        ));
    }

    #[test]
    fn an_empty_google_client_id_is_rejected() {
        let error = ApiConfig::new(super::ApiSettings {
            google_oauth_client_id: String::new(),
            ..test_settings()
        })
        .expect_err("a deployment cannot run the Google flow without a client id");
        assert!(matches!(
            error,
            ConfigError::Missing(var::GOOGLE_OAUTH_CLIENT_ID)
        ));
    }

    #[test]
    fn the_cloud_callbacks_share_the_configured_origin() {
        let config = test_config();
        let origin = config.redirect_uri().origin();
        assert_eq!(config.azure_oauth_redirect_uri().origin(), origin);
        assert_eq!(
            config.azure_oauth_redirect_uri().path(),
            "/v1/providers/azure/oauth/callback"
        );
        assert_eq!(config.gcp_oauth_redirect_uri().origin(), origin);
        assert_eq!(
            config.gcp_oauth_redirect_uri().path(),
            "/v1/providers/gcp/oauth/callback"
        );
    }

    #[test]
    fn the_debug_rendering_never_shows_a_secret() {
        let rendered = format!("{:?}", test_config());
        assert!(!rendered.contains(CLIENT_SECRET));
        assert!(!rendered.contains(KEY_HEX));
        assert!(!rendered.contains(super::super::testing::GITHUB_WEBHOOK_SECRET));
    }
}
