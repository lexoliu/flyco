//! byo-ssh: a Linux host the user already owns, reached over SSH and
//! sandboxed with Podman.
//!
//! This is the provider that needs no cloud account, which is why it exists
//! first: everything downstream of provisioning — the daemon pairing, the
//! relay, the budget ledger — can be exercised end to end against a laptop.
//!
//! # What "provisioning" means here
//!
//! There is no machine to create. The host is already running, so a session's
//! machine is a **Podman container** on it: created from the flyco image,
//! started, and handed the session's `flycod` configuration. Deallocating
//! stops the container and keeps its writable layer; starting puts it back;
//! destroying removes it. Podman is not optional — an agent with a shell on
//! somebody's real machine is exactly what the sandbox is for — so the
//! provider is Linux-only and says so.
//!
//! # The split: this module plans, `ssh` executes
//!
//! SSH is a TCP transport and the Cloudflare Worker has no sockets, so no
//! part of this can run in the control plane. The module is therefore in two
//! halves:
//!
//! * [`ByoSsh`] compiles on wasm32 and performs no I/O at all. It answers
//!   [`catalog`](ByoSsh::catalog) and turns a [`MachineOperation`] into a
//!   [`ContainerJob`] — a serializable description of the container work —
//!   which the Worker enqueues.
//! * [`SshExecutor`], behind the native `ssh` feature, is what implements
//!   [`CloudProvider`](crate::CloudProvider). It opens the connection, runs
//!   the rendered script, and reports the result.
//!
//! A Worker build contains no SSH client because the feature that provides
//! one is not enabled for it. The split is not a convention; it is what the
//! crate graph allows.
//!
//! # What byo-ssh does not support
//!
//! [`MachineOperation::Resize`] has no meaning on hardware the user owns:
//! the host has the cores it has. Planning one is
//! [`ProviderError::Unsupported`], never a silent no-op, because a caller
//! that believes a resize happened will bill and schedule against a machine
//! that did not change.

#[cfg(feature = "ssh")]
mod execute;

#[cfg(feature = "ssh")]
pub use execute::{CommandOutcome, CommandRunner, SshCommandRunner, SshExecutor, SshHost};

use flyco_core::MachineId;
use flyco_core::machine::{CloudProviderKind, MachineCatalogEntry, MachinePricing, OsFamily};
use serde::{Deserialize, Serialize};

use crate::{DaemonBootstrap, MachineOperation, ProviderError};

/// Driver name, as it appears in [`ProviderError::Unsupported`].
pub const PROVIDER: &str = "byo-ssh";

/// The container image a session runs in unless the user names another.
///
/// It must carry `flycod` and the harness CLIs, and its entrypoint must
/// decode [`CONFIG_ENV`] into a `flycod` configuration and run
/// `flycod run` against it.
pub const DEFAULT_IMAGE: &str = "ghcr.io/lexoliu/flyco-session:latest";

/// Environment variable the image reads the base64 `flycod` configuration
/// out of.
///
/// The configuration carries the session's daemon token, so it is handed
/// over on the container's stdin as an env-file rather than as a
/// command-line argument: an argument would be visible in the host's process
/// table for as long as `podman run` took to return.
pub const CONFIG_ENV: &str = "FLYCO_DAEMON_CONFIG";

/// Prefix every flyco container name carries, so a host shared with other
/// work stays legible.
const CONTAINER_PREFIX: &str = "flyco-";

/// The container name for one machine.
///
/// Derived from the machine id rather than allocated, so the name is
/// recoverable from the database without another column and two callers can
/// never disagree about it. A UUID is already a legal Podman name.
#[must_use]
pub fn container_name(machine: MachineId) -> String {
    let mut name = String::with_capacity(CONTAINER_PREFIX.len() + 36);
    name.push_str(CONTAINER_PREFIX);
    name.push_str(&machine.to_string());
    name
}

/// One unit of container lifecycle work, as the control plane enqueues it.
///
/// Serializable on purpose: this is a queue message, and the process that
/// executes it is not the process that planned it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "job", rename_all = "snake_case")]
pub enum ContainerJob {
    /// Create the container, start it, and hand it the daemon's
    /// configuration.
    Create {
        /// Container to create.
        container: String,
        /// Image to create it from.
        image: String,
        /// The machine this container is.
        machine: MachineId,
        /// What its `flycod` needs to phone home.
        ///
        /// Boxed because it is much the largest thing any variant of this
        /// enum carries — three credentials, a repository, a commit
        /// identity and the machine the session is on — and every other
        /// variant is a container name. Without the indirection each `Stop`
        /// on a queue would be padded out to the size of a `Create`. The
        /// JSON is unchanged: a `Box` serializes as what it holds.
        bootstrap: Box<DaemonBootstrap>,
    },
    /// Stop the container, keeping its writable layer.
    Stop {
        /// Container to stop.
        container: String,
    },
    /// Start a stopped container again.
    Start {
        /// Container to start.
        container: String,
    },
    /// Remove the container and its writable layer.
    Remove {
        /// Container to remove.
        container: String,
    },
}

impl ContainerJob {
    /// The container this job acts on.
    #[must_use]
    pub fn container(&self) -> &str {
        match self {
            Self::Create { container, .. }
            | Self::Stop { container }
            | Self::Start { container }
            | Self::Remove { container } => container,
        }
    }
}

/// A registered host, and what flyco knows how to do with it.
///
/// Holds no credential: the private key belongs to the executor that opens
/// the connection, and this half never opens one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByoSsh {
    address: String,
    image: String,
}

impl ByoSsh {
    /// Describes a registered host reachable at `address`, running
    /// [`DEFAULT_IMAGE`].
    #[must_use]
    pub fn new(address: impl Into<String>) -> Self {
        Self {
            address: address.into(),
            image: DEFAULT_IMAGE.to_owned(),
        }
    }

    /// Runs sessions from another image.
    #[must_use]
    pub fn with_image(mut self, image: impl Into<String>) -> Self {
        self.image = image.into();
        self
    }

    /// The host's address, which is also the machine type it offers.
    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
    }

    /// The one machine this provider offers: the host itself.
    ///
    /// It carries no price, because the user already owns and already pays
    /// for the hardware, and no size, because flyco does not learn what the
    /// host has until a daemon runs on it and says so. Both are absent in
    /// the type rather than filled in with a zero, which a budget or a
    /// scheduler would take at face value.
    #[must_use]
    pub fn catalog(&self) -> Vec<MachineCatalogEntry> {
        vec![MachineCatalogEntry {
            // Stamped by the control plane, which knows the row.
            account: None,
            provider: CloudProviderKind::ByoSsh,
            region: self.address.clone(),
            machine_type: self.address.clone(),
            os: OsFamily::Linux,
            capacity: None,
            // For the same reason the capacity is absent: flyco has not
            // looked at this machine, and a family it invented would be a
            // claim curation would then act on.
            lineage: None,
            pricing: MachinePricing::UserOwned,
        }]
    }

    /// Turns an operation into the container work that satisfies it.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Unsupported`] for a resize, which hardware
    /// the user owns cannot do, and for a spec that names a machine type
    /// this host does not answer to.
    pub fn plan(&self, operation: &MachineOperation) -> Result<ContainerJob, ProviderError> {
        match operation {
            MachineOperation::Provision(request) => {
                if request.spec.machine_type != self.address {
                    return Err(ProviderError::Unsupported {
                        provider: PROVIDER,
                        operation: "provisioning another host",
                        reason: "a registered host offers exactly one machine type — itself",
                    });
                }
                Ok(ContainerJob::Create {
                    container: container_name(request.machine),
                    image: self.image.clone(),
                    machine: request.machine,
                    bootstrap: Box::new(request.bootstrap.clone()),
                })
            }
            MachineOperation::Resize { .. } => Err(ProviderError::Unsupported {
                provider: PROVIDER,
                operation: "resize",
                reason: "a registered host has the hardware it has; \
                         start a session on a cloud provider to change machine size",
            }),
            MachineOperation::Deallocate { machine } => Ok(ContainerJob::Stop {
                container: container_name(machine.id),
            }),
            MachineOperation::Start { machine } => Ok(ContainerJob::Start {
                container: container_name(machine.id),
            }),
            MachineOperation::Destroy { machine } => Ok(ContainerJob::Remove {
                container: container_name(machine.id),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use flyco_core::machine::{CloudProviderKind, MachinePricing, MachineSpec, MachineState};
    use flyco_core::{MachineId, PermissionMode, SessionId};

    use super::{ByoSsh, ContainerJob, DEFAULT_IMAGE, PROVIDER, container_name};
    use crate::{
        CapacityMode, ClaudeCredential, DaemonBootstrap, HarnessCredential, Machine,
        MachineOperation, ProviderError, ProvisionRequest,
    };

    const HOST: &str = "build.lexo.cool";

    fn bootstrap() -> DaemonBootstrap {
        DaemonBootstrap {
            session: SessionId::generate(),
            control_plane_url: "https://flyco.dev/".to_owned(),
            daemon_token: "fd_token".to_owned(),
            permission_mode: PermissionMode::Default,
            auth: HarnessCredential::ClaudeCode(ClaudeCredential::Inherit),
            repo: crate::testing::checkout(),
            machine_origin: flyco_core::MachineOrigin::Auto,
            machine: crate::testing::session_machine(),
            resume_session_id: None,
        }
    }

    fn machine(id: MachineId) -> Machine {
        Machine {
            id,
            native_id: container_name(id),
            region: HOST.to_owned(),
            state: MachineState::Running,
            capacity_mode: CapacityMode::OnDemand,
            address: Some(HOST.to_owned()),
        }
    }

    fn provision(machine_type: &str) -> MachineOperation {
        MachineOperation::Provision(Box::new(ProvisionRequest {
            machine: MachineId::generate(),
            spec: MachineSpec {
                provider: CloudProviderKind::ByoSsh,
                machine_type: machine_type.to_owned(),
                region: HOST.to_owned(),
                spot: false,
                disk_gib: 0,
            },
            bootstrap: bootstrap(),
        }))
    }

    #[test]
    fn the_catalog_is_the_host_itself_and_carries_no_price() {
        let catalog = ByoSsh::new(HOST).catalog();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].machine_type, HOST);
        assert_eq!(catalog[0].pricing, MachinePricing::UserOwned);
        assert_eq!(catalog[0].pricing.hourly(false), None);
        assert!(
            catalog[0].capacity.is_none(),
            "flyco does not know the host's size until a daemon reports it"
        );
    }

    #[test]
    fn provisioning_plans_a_container_from_the_configured_image() {
        let host = ByoSsh::new(HOST).with_image("localhost/flyco-session:dev");
        let operation = provision(HOST);
        let job = host.plan(&operation).expect("plan");

        let ContainerJob::Create {
            container,
            image,
            machine,
            ..
        } = job
        else {
            panic!("provisioning plans a create");
        };
        assert_eq!(image, "localhost/flyco-session:dev");
        assert_eq!(container, container_name(machine));
        assert!(container.starts_with("flyco-"));
    }

    #[test]
    fn the_default_image_is_used_when_none_is_named() {
        let job = ByoSsh::new(HOST).plan(&provision(HOST)).expect("plan");
        assert!(matches!(job, ContainerJob::Create { image, .. } if image == DEFAULT_IMAGE));
    }

    #[test]
    fn a_resize_is_refused_rather_than_ignored() {
        let error = ByoSsh::new(HOST)
            .plan(&MachineOperation::Resize {
                machine: machine(MachineId::generate()),
                machine_type: "bigger".to_owned(),
            })
            .expect_err("a registered host cannot resize");

        assert!(matches!(
            error,
            ProviderError::Unsupported {
                provider: PROVIDER,
                operation: "resize",
                ..
            }
        ));
    }

    #[test]
    fn provisioning_a_machine_type_this_host_does_not_offer_is_refused() {
        ByoSsh::new(HOST)
            .plan(&provision("some.other.host"))
            .expect_err("a host offers exactly one machine type");
    }

    #[test]
    fn the_lifecycle_operations_address_the_machines_own_container() {
        let host = ByoSsh::new(HOST);
        let id = MachineId::generate();
        let expected = container_name(id);

        for (operation, job) in [
            (
                MachineOperation::Deallocate {
                    machine: machine(id),
                },
                ContainerJob::Stop {
                    container: expected.clone(),
                },
            ),
            (
                MachineOperation::Start {
                    machine: machine(id),
                },
                ContainerJob::Start {
                    container: expected.clone(),
                },
            ),
            (
                MachineOperation::Destroy {
                    machine: machine(id),
                },
                ContainerJob::Remove {
                    container: expected,
                },
            ),
        ] {
            assert_eq!(host.plan(&operation).expect("plan"), job);
        }
    }

    #[test]
    fn a_job_round_trips_through_the_queue_without_leaking_the_token() {
        let job = ByoSsh::new(HOST).plan(&provision(HOST)).expect("plan");
        let encoded = serde_json::to_string(&job).expect("serialize");
        let back: ContainerJob = serde_json::from_str(&encoded).expect("deserialize");

        assert_eq!(back, job);
        assert!(
            !format!("{job:?}").contains("fd_token"),
            "a queued job must not print its daemon token"
        );
    }
}
