//! The cloud-config every provisioned machine boots with.
//!
//! One document, shared by every driver that boots a real virtual machine:
//! Azure hands it over as `osProfile.customData`, EC2 as `UserData`, and
//! Compute Engine as the `user-data` metadata key. All three want the same
//! thing — write the `flycod` configuration, install the daemon, start it —
//! and all three want it base64-encoded, so [`render`] answers with the
//! encoding rather than with the text.
//!
//! The daemon configuration travels inside a `write_files` entry as base64
//! (`encoding: b64`), which is what keeps the session's daemon token from
//! appearing as a legible substring of a provisioning request.

use askama::Template;
use base64::Engine as _;

use crate::ProviderError;

/// Where the `flycod` configuration lands on a provisioned machine.
pub const CONFIG_PATH: &str = "/etc/flycod/config.toml";

/// Where a machine fetches `flycod` from on first boot, unless the caller
/// names somewhere else.
pub const DEFAULT_FLYCOD_INSTALLER_URL: &str = "https://flyco.dev/install/flycod.sh";

/// The cloud-config a machine boots with.
#[derive(Debug, Template)]
#[template(path = "cloud_init.yml", escape = "none")]
struct CloudInit {
    config_path: String,
    config_base64: String,
    installer_url: String,
}

/// Quotes a value as a YAML single-quoted scalar.
///
/// Single quotes because nothing inside one is an escape except a doubled
/// quote, which makes the encoding total: any byte sequence round-trips.
fn yaml_quoted(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for character in value.chars() {
        if character == '\'' {
            quoted.push('\'');
        }
        quoted.push(character);
    }
    quoted.push('\'');
    quoted
}

/// Base64 of the cloud-config that installs `flycod` and hands it
/// `daemon_config`.
///
/// # Errors
///
/// Returns [`ProviderError::Malformed`] if the template does not render,
/// which would mean this module is broken rather than anything the caller
/// did.
pub fn render(daemon_config: &str, installer_url: &str) -> Result<String, ProviderError> {
    let engine = base64::engine::general_purpose::STANDARD;
    let rendered = CloudInit {
        config_path: yaml_quoted(CONFIG_PATH),
        config_base64: engine.encode(daemon_config),
        installer_url: installer_url.to_owned(),
    }
    .render()
    .map_err(|_| ProviderError::Malformed("the cloud-init template did not render"))?;

    Ok(engine.encode(rendered))
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;

    use super::{CONFIG_PATH, render, yaml_quoted};

    fn decoded(encoded: &str) -> String {
        String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .expect("the document is base64"),
        )
        .expect("the document is UTF-8")
    }

    #[test]
    fn the_configuration_travels_base64_rather_than_as_readable_text() {
        let encoded = render("daemon_token = \"fd_live\"\n", "https://flyco.dev/i.sh")
            .expect("the template renders");
        let cloud_config = decoded(&encoded);

        assert!(cloud_config.starts_with("#cloud-config"));
        assert!(cloud_config.contains(CONFIG_PATH));
        assert!(cloud_config.contains("encoding: b64"));
        assert!(
            !cloud_config.contains("fd_live"),
            "the daemon token must not be legible in the cloud-config"
        );

        let inner = cloud_config
            .lines()
            .find_map(|line| line.trim().strip_prefix("content: "))
            .expect("the cloud-config writes the daemon configuration");
        assert!(decoded(inner).contains("fd_live"));
    }

    #[test]
    fn a_quoted_scalar_escapes_by_doubling() {
        assert_eq!(yaml_quoted("it's"), "'it''s'");
    }
}
