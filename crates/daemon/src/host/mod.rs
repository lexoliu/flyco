//! `flycod host`: the daemon on a machine the user owns.
//!
//! The same binary as the session daemon, in its other role. A session VM
//! runs `flycod run` and drives a coding harness; a machine somebody owns
//! runs `flycod host run` and drives *containers*, each of which has a
//! `flycod run` inside it. Nothing about a session changes because it landed
//! here — only where the machine came from (docs/host-enrollment.md).
//!
//! # Two commands
//!
//! * [`enroll`] registers this machine: it measures itself, spends the
//!   single-use `fh_` enrollment token the user pasted, and writes
//!   `/etc/flyco/host.toml` with the long-lived token that comes back. It
//!   runs once, from the installer, and exits.
//! * [`run`] is the unit: one outbound WebSocket to this machine's room,
//!   container jobs in, results out, until the control plane revokes the
//!   machine.
//!
//! # Why the machine dials
//!
//! The control plane is a Cloudflare Worker and has no TCP sockets, so it
//! can never open a connection to somebody's hardware. Everything here is
//! outbound: the enrollment is a `POST`, the relay is a socket this process
//! opens, and a job result travels back up both.
//!
//! # What is in each module
//!
//! * [`config`] — `/etc/flyco/host.toml`: which host this is, its token,
//!   its control plane, and where Podman keeps things. Root-only.
//! * [`facts`] — what the machine says about itself, measured rather than
//!   configured.
//! * [`podman`] — running one [`ContainerJob`](flyco_provider::host::ContainerJob)
//!   under rootless Podman, from the script the control plane's own planner
//!   rendered.
//! * [`relay`] — the socket, its reconnects, and the two halves of a job
//!   result.
//! * [`rest`] — enrollment, and the durable half of a job result.

pub mod config;
pub mod facts;
pub mod podman;
pub mod relay;
pub mod rest;

use std::path::{Path, PathBuf};

use flyco_core::host::EnrollHost;
use url::Url;

pub use config::{HostConfig, HostConfigError, PodmanConfig};
pub use facts::FactsError;
pub use podman::{LocalExecutor, PodmanError, ProcessRunner, Rootless};
pub use relay::{HostEndpoint, HostRelay, Stopped};
pub use rest::HttpHostApi;

use crate::control::rest::ControlApiError;
use crate::control::wire::WireError;

/// Anything that stops `flycod host`.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    /// The machine's configuration could not be read or written.
    #[error(transparent)]
    Config(#[from] HostConfigError),
    /// The machine could not describe itself.
    #[error(transparent)]
    Facts(#[from] FactsError),
    /// A call to the control plane failed.
    #[error(transparent)]
    ControlApi(#[from] ControlApiError),
    /// The relay could not be held.
    #[error(transparent)]
    Wire(#[from] WireError),
    /// Podman could not be reached as the container user.
    #[error(transparent)]
    Podman(#[from] PodmanError),
    /// This machine has been removed from the control plane.
    ///
    /// A refusal rather than a reconnect loop: the token in the file will
    /// never authenticate again, and what fixes it is enrolling the machine
    /// afresh, not restarting the unit.
    #[error(
        "this machine's enrollment was revoked ({path}). \
         Enrol it again to put it back in service"
    )]
    Revoked {
        /// The configuration that says so.
        path: PathBuf,
    },
}

/// What `flycod host enroll` was asked to do.
#[derive(Debug, Clone)]
pub struct Enrollment {
    /// The single-use `fh_` token the user pasted.
    pub token: String,
    /// The control plane that minted it.
    pub control_plane: Url,
    /// Where rootless Podman keeps this machine's containers and volumes.
    pub volume_root: PathBuf,
    /// Where to write the configuration.
    pub config: PathBuf,
}

/// Registers this machine and writes its configuration.
///
/// Measured before it is registered, and registered before anything is
/// written: a machine with no Podman is refused by [`facts::gather`] with
/// the command that installs one, and an enrollment the control plane
/// refuses leaves nothing behind on the machine.
///
/// # Errors
///
/// Returns [`HostError`] if the machine cannot describe itself, the control
/// plane refuses the token, or the configuration cannot be written.
pub async fn enroll(enrollment: Enrollment) -> Result<HostConfig, HostError> {
    let facts = facts::gather(&enrollment.volume_root).await?;
    tracing::info!(
        hostname = %facts.hostname,
        architecture = ?facts.architecture,
        vcpus = facts.vcpus,
        podman = %facts.podman_version,
        "enrolling this machine"
    );

    let control_plane = with_base_path(enrollment.control_plane);
    let enrolled = rest::enroll(
        &control_plane,
        &EnrollHost {
            token: enrollment.token,
            facts,
        },
    )
    .await?;

    let config = HostConfig {
        host_id: enrolled.host_id,
        host_token: enrolled.host_token,
        control_plane_url: control_plane,
        revoked: false,
        podman: PodmanConfig {
            volume_root: enrollment.volume_root,
            ..PodmanConfig::default()
        },
    };
    config.save(&enrollment.config).await?;
    tracing::info!(
        host = %config.host_id,
        path = %enrollment.config.display(),
        "this machine is enrolled"
    );
    Ok(config)
}

/// Holds this machine's relay until the control plane revokes it.
///
/// Returns when the machine is revoked, having recorded the revocation in
/// the configuration so a restarted unit refuses rather than reconnecting.
///
/// # Errors
///
/// Returns [`HostError`] if the configuration cannot be read, this machine
/// has already been revoked, the container user does not exist, or the
/// control plane speaks a relay this daemon cannot read.
pub async fn run(path: &Path) -> Result<(), HostError> {
    let mut config = HostConfig::load(path).await?;
    if config.revoked {
        return Err(HostError::Revoked {
            path: path.to_path_buf(),
        });
    }

    let rootless = Rootless::resolve(&config.podman, &ProcessRunner).await?;
    let facts = facts::gather(&config.podman.volume_root).await?;
    tracing::info!(
        host = %config.host_id,
        control_plane = %config.control_plane_url,
        hostname = %facts.hostname,
        "flycod host is up"
    );

    let stopped = relay::run(HostRelay {
        endpoint: HostEndpoint::from_base(
            &config.control_plane_url,
            config.host_id,
            config.host_token.clone(),
        )?,
        facts,
        jobs: std::sync::Arc::new(LocalExecutor::new(rootless, ProcessRunner)),
        api: HttpHostApi::new(
            config.control_plane_url.clone(),
            config.host_id,
            config.host_token.clone(),
        ),
    })
    .await?;

    match stopped {
        Stopped::Revoked => {
            config.revoked = true;
            config.save(path).await?;
            tracing::warn!(
                path = %path.display(),
                "this machine's enrollment is revoked; enrol it again to put it back in service"
            );
        }
    }
    Ok(())
}

/// A base URL every route resolves under.
///
/// `https://dev.flyco.dev` and `https://dev.flyco.dev/` address the same
/// deployment, but only the second one joins `v1/hosts/enroll` onto the end
/// rather than over the last segment. The installer derives the origin from
/// the URL the installer itself was served from, so which of the two it
/// passes is not something to leave to chance.
fn with_base_path(mut url: Url) -> Url {
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    url
}

#[cfg(test)]
mod tests {
    use flyco_core::HostId;

    use super::{Enrollment, HostConfig, HostError, enroll, run, with_base_path};

    /// A private directory under the test process's own temporary root.
    fn tempdir() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("flycod-enroll-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create a temporary directory");
        path
    }

    #[test]
    fn an_origin_without_a_trailing_slash_still_addresses_every_route() {
        for given in [
            "https://dev.flyco.dev",
            "https://dev.flyco.dev/",
            "http://127.0.0.1:8787",
        ] {
            let base = with_base_path(given.parse().expect("a URL"));
            assert!(base.path().ends_with('/'), "{base}");
            assert!(
                base.join("v1/hosts/enroll")
                    .expect("join")
                    .path()
                    .ends_with("/v1/hosts/enroll"),
                "{base}"
            );
        }
    }

    #[tokio::test]
    async fn a_machine_that_cannot_measure_itself_writes_no_configuration() {
        let directory = tempdir();
        let config = directory.join("host.toml");

        let error = enroll(Enrollment {
            token: "fh_an-enrollment-token".to_owned(),
            control_plane: "https://dev.flyco.dev/".parse().expect("a URL"),
            // Nothing is mounted here, so the machine cannot measure the
            // free space Podman would have.
            volume_root: directory.join("not-a-directory"),
            config: config.clone(),
        })
        .await
        .expect_err("a machine that cannot describe itself does not enrol");

        assert!(matches!(error, HostError::Facts(_)), "{error:?}");
        assert!(
            !config.exists(),
            "a refused enrollment leaves nothing behind"
        );
    }

    #[tokio::test]
    async fn a_revoked_machine_refuses_to_run_rather_than_reconnecting() {
        let directory = tempdir();
        let path = directory.join("host.toml");
        HostConfig {
            host_id: HostId::generate(),
            host_token: "fh_a-revoked-token".to_owned(),
            control_plane_url: "https://dev.flyco.dev/".parse().expect("a URL"),
            revoked: true,
            podman: super::PodmanConfig::default(),
        }
        .save(&path)
        .await
        .expect("save");

        let error = run(&path)
            .await
            .expect_err("a revoked machine stays stopped");

        assert!(matches!(error, HostError::Revoked { .. }), "{error:?}");
        assert!(
            error.to_string().contains("Enrol it again"),
            "the operator is told what fixes it: {error}"
        );
    }
}
