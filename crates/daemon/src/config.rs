//! `flycod`'s on-disk configuration.
//!
//! One TOML file, deserialized with `deny_unknown_fields` throughout: a
//! typo in a session VM's config is a provisioning bug, and the daemon says
//! so at startup rather than running with a silently ignored setting.

use std::path::{Path, PathBuf};

use flyco_core::{HarnessKind, SessionId};
use serde::Deserialize;

use crate::harness::claude::protocol::{PermissionMode, SidecarAuth};
use crate::harness::claude::sidecar::SidecarConfig;

/// The configuration file could not be used.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("could not read the flycod config at {path}")]
    Read {
        /// The path that was tried.
        path: PathBuf,
        /// The underlying cause.
        #[source]
        source: std::io::Error,
    },
    /// The file is not valid TOML, or names something flycod does not know.
    #[error("the flycod config at {path} is invalid")]
    Parse {
        /// The path that was parsed.
        path: PathBuf,
        /// The underlying cause.
        #[source]
        source: toml::de::Error,
    },
}

/// An isolated Claude Code configuration tree.
///
/// Present on exactly the auth modes that inject credentials: a session VM
/// gets its own config directory and a stable project key, so nothing about
/// one session's Claude state can be seen or trampled by another.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Isolation {
    /// `CLAUDE_CONFIG_DIR` for the supervised CLI.
    pub config_dir: PathBuf,
    /// `CLAUDE_CODE_PROJECT_DIR_NAME` — the stable project key the Agent
    /// SDK files sessions and transcripts under.
    pub project_dir_name: String,
}

/// How the supervised `claude` CLI authenticates.
///
/// Credentials and isolation are one decision, not two: injecting a token
/// into the host's own `~/.claude` would rewrite a real person's login, and
/// an isolated config tree with no credentials has no way to authenticate.
/// Making [`Isolation`] a field of the credential-bearing variants means
/// neither mistake is expressible.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClaudeAuth {
    /// Use the host user's existing Claude login.
    ///
    /// No `CLAUDE_CONFIG_DIR`, no credential injection — the CLI reads the
    /// default `~/.claude`. This is how flyco is driven on a developer's
    /// own machine; a provisioned session VM never uses it.
    Inherit,
    /// A Claude subscription OAuth token in an isolated config tree.
    OauthToken {
        /// Value for `CLAUDE_CODE_OAUTH_TOKEN`.
        token: String,
        /// The config tree it applies to.
        isolation: Isolation,
    },
    /// An Anthropic API key in an isolated config tree.
    ApiKey {
        /// Value for `ANTHROPIC_API_KEY`.
        key: String,
        /// The config tree it applies to.
        isolation: Isolation,
    },
}

impl ClaudeAuth {
    /// The credentials half, as the sidecar protocol carries it.
    #[must_use]
    pub fn sidecar_auth(&self) -> SidecarAuth {
        match self {
            Self::Inherit => SidecarAuth::Inherit,
            Self::OauthToken { token, .. } => SidecarAuth::OauthToken {
                token: token.clone(),
            },
            Self::ApiKey { key, .. } => SidecarAuth::ApiKey { key: key.clone() },
        }
    }

    /// The isolated config tree, when this mode has one.
    #[must_use]
    pub const fn isolation(&self) -> Option<&Isolation> {
        match self {
            Self::Inherit => None,
            Self::OauthToken { isolation, .. } | Self::ApiKey { isolation, .. } => Some(isolation),
        }
    }
}

/// Settings specific to the Claude Code harness.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeConfig {
    /// Model override; omitted leaves the CLI's own default in place.
    #[serde(default)]
    pub model: Option<String>,
    /// Permission mode, spelled the way the Agent SDK spells it
    /// (`default`, `acceptEdits`, `bypassPermissions`, `plan`).
    pub permission_mode: PermissionMode,
    /// Credentials and config isolation.
    pub auth: ClaudeAuth,
}

/// Everything `flycod run` needs.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    /// The flyco session this daemon serves.
    pub session: SessionId,
    /// Which harness to drive.
    pub harness: HarnessKind,
    /// Directory the agent works in.
    pub workdir: PathBuf,
    /// Root of the local append-only transcript store.
    pub transcript_dir: PathBuf,
    /// Harness-native session id to resume, for cross-host History.
    #[serde(default)]
    pub resume_session_id: Option<String>,
    /// Claude Code settings.
    pub claude: ClaudeConfig,
    /// Where the Bun sidecar is materialized and how Bun is run.
    pub sidecar: SidecarConfig,
}

impl DaemonConfig {
    /// Reads and validates a config file.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if the file cannot be read, is not valid
    /// TOML, or names a field flycod does not know.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source,
        })
    }
}

/// A complete, valid configuration, printed by `flycod example-config`.
pub const EXAMPLE: &str = include_str!("../config.example.toml");

#[cfg(test)]
mod tests {
    use super::{ClaudeAuth, DaemonConfig, EXAMPLE};
    use crate::harness::claude::protocol::{PermissionMode, SidecarAuth};
    use flyco_core::HarnessKind;

    fn parse(text: &str) -> Result<DaemonConfig, toml::de::Error> {
        toml::from_str(text)
    }

    #[test]
    fn the_shipped_example_is_a_valid_config() {
        let config = parse(EXAMPLE).expect("the example config must parse");
        assert_eq!(config.harness, HarnessKind::ClaudeCode);
        assert!(matches!(config.claude.auth, ClaudeAuth::Inherit));
        assert_eq!(config.claude.permission_mode, PermissionMode::Default);
        assert_eq!(config.sidecar.bun, std::path::PathBuf::from("bun"));
    }

    #[test]
    fn inheriting_the_host_login_isolates_nothing() {
        let config = parse(EXAMPLE).expect("parse");
        assert!(config.claude.auth.isolation().is_none());
        assert!(matches!(
            config.claude.auth.sidecar_auth(),
            SidecarAuth::Inherit
        ));
    }

    #[test]
    fn injected_credentials_carry_their_own_config_tree() {
        let text = EXAMPLE.replace(
            "mode = \"inherit\"",
            "mode = \"oauth_token\"\ntoken = \"sk-ant-oat01-example\"\n\
             isolation = { config_dir = \"/var/lib/flyco/claude\", project_dir_name = \"flyco-session\" }",
        );
        let config = parse(&text).expect("parse");
        let isolation = config
            .claude
            .auth
            .isolation()
            .expect("credential modes are isolated");
        assert_eq!(isolation.project_dir_name, "flyco-session");
        assert!(matches!(
            config.claude.auth.sidecar_auth(),
            SidecarAuth::OauthToken { .. }
        ));
    }

    #[test]
    fn credentials_without_a_config_tree_do_not_parse() {
        let text = EXAMPLE.replace(
            "mode = \"inherit\"",
            "mode = \"api_key\"\nkey = \"sk-ant-example\"",
        );
        parse(&text).expect_err("an isolation-less credential mode is not expressible");
    }

    #[test]
    fn an_unknown_key_is_rejected_rather_than_ignored() {
        let text = format!("{EXAMPLE}\nreconnect_delay_ms = 500\n");
        let error = parse(&text).expect_err("unknown keys must fail the daemon at startup");
        assert!(
            error.to_string().contains("reconnect_delay_ms"),
            "the error should name the offending key: {error}"
        );
    }
}
