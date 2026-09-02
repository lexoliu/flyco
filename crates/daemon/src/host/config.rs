//! What an enrolled machine keeps on disk.
//!
//! One TOML file, written by `flycod host enroll` and read by `flycod host
//! run`, holding the four things the machine cannot re-derive: which host it
//! is, the token that proves it, the control plane it belongs to, and where
//! Podman keeps its containers.
//!
//! It is root-only, mode `0600`, because the host token is a live credential
//! for every session that will ever run here: anything that can read this
//! file can pretend to be this machine. That is also why the unit runs as
//! root and drops to the `flyco` user for each podman command, rather than
//! running as `flyco` and leaving the token where the container user could
//! read it.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use flyco_core::HostId;
use flyco_core::host::HOST_TOKEN_PREFIX;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt as _;
use url::Url;

/// Where the installer writes this file, and where `flycod host run` looks.
pub const DEFAULT_PATH: &str = "/etc/flyco/host.toml";

/// The user rootless Podman runs as, which the installer creates.
pub const DEFAULT_USER: &str = "flyco";

/// That user's home directory.
pub const DEFAULT_HOME: &str = "/home/flyco";

/// Where rootless Podman keeps containers and volumes for that user.
pub const DEFAULT_VOLUME_ROOT: &str = "/home/flyco/.local/share/containers";

/// Mode the file is created with and kept at: readable by root alone.
const MODE: u32 = 0o600;

/// The configuration could not be used.
#[derive(Debug, thiserror::Error)]
pub enum HostConfigError {
    /// The file could not be read.
    #[error("could not read the flyco host config at {path}")]
    Read {
        /// The path that was tried.
        path: PathBuf,
        /// The underlying cause.
        #[source]
        source: std::io::Error,
    },
    /// The file could not be written.
    #[error("could not write the flyco host config at {path}")]
    Write {
        /// The path that was tried.
        path: PathBuf,
        /// The underlying cause.
        #[source]
        source: std::io::Error,
    },
    /// The file is not valid TOML, or names something `flycod host` does not
    /// know.
    #[error("the flyco host config at {path} is invalid")]
    Parse {
        /// The path that was parsed.
        path: PathBuf,
        /// The underlying cause.
        #[source]
        source: toml::de::Error,
    },
    /// The configuration itself would not serialize, which is this binary's
    /// own bug rather than anything the machine did.
    #[error("the flyco host config could not be encoded")]
    Encode(#[source] toml::ser::Error),
    /// `host_token` is not a host token.
    #[error(
        "`host_token` must be a `{HOST_TOKEN_PREFIX}` token from `POST /v1/hosts/enroll`, \
         not a daemon token or an API key"
    )]
    NotAHostToken,
}

/// Where rootless Podman lives on this machine.
///
/// Configurable rather than compiled in because the installer is not the
/// only way a host is set up — a machine that already had a container user
/// keeps it — and because a wrong guess here is a daemon that runs every job
/// as the wrong user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PodmanConfig {
    /// The unprivileged user every container runs as.
    #[serde(default = "default_user")]
    pub user: String,
    /// That user's home directory, which is `HOME` for every podman command.
    #[serde(default = "default_home")]
    pub home: PathBuf,
    /// Where Podman keeps this machine's containers and volumes.
    ///
    /// The free space on the filesystem holding it is what
    /// [`HostFacts::disk_free_gib`](flyco_core::host::HostFacts::disk_free_gib)
    /// reports, and therefore what the control plane schedules against.
    #[serde(default = "default_volume_root")]
    pub volume_root: PathBuf,
}

fn default_user() -> String {
    DEFAULT_USER.to_owned()
}

fn default_home() -> PathBuf {
    PathBuf::from(DEFAULT_HOME)
}

fn default_volume_root() -> PathBuf {
    PathBuf::from(DEFAULT_VOLUME_ROOT)
}

impl Default for PodmanConfig {
    fn default() -> Self {
        Self {
            user: default_user(),
            home: default_home(),
            volume_root: default_volume_root(),
        }
    }
}

/// This machine's enrollment.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    /// Which host this machine is, as every route and every container job
    /// names it.
    pub host_id: HostId,
    /// The long-lived `fh_` token minted at enrollment.
    pub host_token: String,
    /// The control plane this machine belongs to.
    pub control_plane_url: Url,
    /// Whether that control plane has revoked this machine.
    ///
    /// Written when a `Revoked` frame arrives, and read at startup: a
    /// revoked host must stay stopped rather than reconnect every few
    /// seconds against a credential that will never work again. Removing a
    /// host is not undone by restarting its unit; it is undone by enrolling
    /// the machine again.
    #[serde(default)]
    pub revoked: bool,
    /// Where rootless Podman lives.
    #[serde(default)]
    pub podman: PodmanConfig,
}

/// Never prints the token: a configuration reaches a log line as often as
/// anything else in this daemon, and the token is the machine's identity.
impl core::fmt::Debug for HostConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HostConfig")
            .field("host_id", &self.host_id)
            .field("control_plane_url", &self.control_plane_url.as_str())
            .field("revoked", &self.revoked)
            .field("podman", &self.podman)
            .finish_non_exhaustive()
    }
}

impl HostConfig {
    /// Reads and validates the configuration at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`HostConfigError`] if the file cannot be read, is not the
    /// TOML this daemon defines, or carries something that is not a host
    /// token.
    pub async fn load(path: &Path) -> Result<Self, HostConfigError> {
        let text =
            tokio::fs::read_to_string(path)
                .await
                .map_err(|source| HostConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                })?;
        let config: Self = toml::from_str(&text).map_err(|source| HostConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        if !config.host_token.starts_with(HOST_TOKEN_PREFIX) {
            return Err(HostConfigError::NotAHostToken);
        }
        Ok(config)
    }

    /// Writes the configuration to `path`, root-only.
    ///
    /// The directory is created if it is not there, mode `0700`, and the
    /// file is created — or re-permissioned, if it already existed — mode
    /// `0600`, so an enrollment that lands on a machine with a
    /// world-readable `/etc/flyco` does not leave the token world-readable
    /// with it.
    ///
    /// # Errors
    ///
    /// Returns [`HostConfigError`] if the configuration will not serialize
    /// or the file cannot be written.
    pub async fn save(&self, path: &Path) -> Result<(), HostConfigError> {
        let text = toml::to_string_pretty(self).map_err(HostConfigError::Encode)?;
        let write = |source| HostConfigError::Write {
            path: path.to_path_buf(),
            source,
        };

        if let Some(directory) = path.parent() {
            tokio::fs::create_dir_all(directory).await.map_err(write)?;
            tokio::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
                .await
                .map_err(write)?;
        }

        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(MODE)
            .open(path)
            .await
            .map_err(write)?;
        file.write_all(text.as_bytes()).await.map_err(write)?;
        file.flush().await.map_err(write)?;
        // `mode` above applies only when the file is created, so a file that
        // was already there is re-permissioned rather than trusted.
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(MODE))
            .await
            .map_err(write)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use flyco_core::HostId;

    use super::{DEFAULT_HOME, DEFAULT_USER, DEFAULT_VOLUME_ROOT, HostConfig, HostConfigError};

    /// What the installer's enrollment leaves behind: the three fields it
    /// writes, and nothing else.
    const ENROLLED: &str = include_str!("../../fixtures/host/enrolled.toml");

    /// The same file with one key misspelled.
    const MISSPELLED: &str = include_str!("../../fixtures/host/misspelled.toml");

    fn config() -> HostConfig {
        HostConfig {
            host_id: HostId::generate(),
            host_token: "fh_a-live-host-token".to_owned(),
            control_plane_url: "https://dev.flyco.dev/".parse().expect("a URL"),
            revoked: false,
            podman: super::PodmanConfig::default(),
        }
    }

    #[tokio::test]
    async fn a_saved_configuration_reads_back_as_itself() {
        let directory = tempdir();
        let path = directory.join("host.toml");
        let written = config();

        written.save(&path).await.expect("save");
        let read = HostConfig::load(&path).await.expect("load");

        assert_eq!(read, written);
    }

    #[tokio::test]
    async fn the_token_is_readable_by_root_alone() {
        let directory = tempdir();
        let path = directory.join("host.toml");

        config().save(&path).await.expect("save");

        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the host token must not be readable");
        let directory_mode = std::fs::metadata(&directory)
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(directory_mode & 0o777, 0o700);
    }

    #[tokio::test]
    async fn saving_over_a_world_readable_file_takes_its_permissions_away() {
        let directory = tempdir();
        let path = directory.join("host.toml");
        std::fs::write(&path, "").expect("seed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");

        config().save(&path).await.expect("save");

        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn a_revocation_survives_a_restart() {
        let directory = tempdir();
        let path = directory.join("host.toml");
        let mut revoked = config();
        revoked.revoked = true;

        revoked.save(&path).await.expect("save");

        assert!(HostConfig::load(&path).await.expect("load").revoked);
    }

    #[tokio::test]
    async fn a_configuration_that_names_no_host_token_is_refused() {
        let directory = tempdir();
        let path = directory.join("host.toml");
        let mut wrong = config();
        wrong.host_token = "fd_a-daemon-token".to_owned();
        wrong.save(&path).await.expect("save");

        assert!(matches!(
            HostConfig::load(&path).await,
            Err(HostConfigError::NotAHostToken)
        ));
    }

    #[tokio::test]
    async fn podman_defaults_to_the_layout_the_installer_creates() {
        let directory = tempdir();
        let path = directory.join("host.toml");
        std::fs::write(&path, ENROLLED).expect("seed");

        let config = HostConfig::load(&path).await.expect("load");

        assert_eq!(config.podman.user, DEFAULT_USER);
        assert_eq!(config.podman.home.to_str(), Some(DEFAULT_HOME));
        assert_eq!(
            config.podman.volume_root.to_str(),
            Some(DEFAULT_VOLUME_ROOT)
        );
        assert!(!config.revoked);
    }

    #[tokio::test]
    async fn a_typo_is_refused_rather_than_ignored() {
        let directory = tempdir();
        let path = directory.join("host.toml");
        std::fs::write(&path, MISSPELLED).expect("seed");

        assert!(matches!(
            HostConfig::load(&path).await,
            Err(HostConfigError::Parse { .. })
        ));
    }

    /// A private directory under the test process's own temporary root.
    fn tempdir() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("flycod-host-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create a temporary directory");
        path
    }
}
