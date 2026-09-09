//! Hosts: a Linux machine the user owns, running sessions as Podman
//! containers.
//!
//! There is no machine to create here. The host is already running and
//! already enrolled, so a session's machine is a **container** on it:
//! created from the flyco image with a volume for the checkout, started, and
//! handed the session's `flycod` configuration. Deallocating stops the
//! container and keeps its volume; starting puts it back; destroying removes
//! the container and, when the disk is not being kept, the volume with it.
//! Podman is not optional — an agent with a shell on somebody's real machine
//! is exactly what the sandbox is for.
//!
//! # This module plans; the host executes
//!
//! The control plane cannot reach the machine at all: a Cloudflare Worker
//! has no sockets, which is why a host is enrolled rather than dialled
//! (docs/host-enrollment.md). So nothing here performs anything.
//!
//! * [`Host`] compiles on wasm32 and does no I/O. It answers
//!   [`catalog`](Host::catalog) from the facts the machine reported, and
//!   turns a [`MachineOperation`] into a [`ContainerJob`].
//! * [`ControlToHost`] carries that job down the host's own outbound
//!   WebSocket, and [`HostToControl`] carries back what came of it.
//! * [`script::render`] turns a job into the shell the host runs. It lives
//!   here, beside the job it renders, so the quoting that keeps a container
//!   name from becoming another command is tested where it is written — and
//!   `flycod host` on the machine runs exactly what this produces.
//!
//! # Why the wire enums are here and not in `flyco_core`
//!
//! Every other wire protocol flyco speaks is in [`flyco_core::wire`]. This
//! one cannot be: a [`ControlToHost::Run`] carries a [`ContainerJob`], which
//! carries a [`DaemonBootstrap`], and both of those live in this crate.
//! Moving them to `flyco_core` would take the whole provider vocabulary —
//! harness credentials, repository checkouts, git identities — with them.
//! What *is* in `flyco_core` is everything the REST API also speaks:
//! [`flyco_core::host`] holds the facts, the states and the enrollment
//! documents.
//!
//! # What a host does not support
//!
//! [`MachineOperation::Resize`] has no meaning on hardware the user owns:
//! the machine has the cores it has. Planning one is
//! [`ProviderError::Unsupported`], never a silent no-op, because a caller
//! that believes a resize happened will bill and schedule against a machine
//! that did not change.

pub mod script;
mod wire;

pub use script::render;
pub use wire::{ControlToHost, HostToControl};

use flyco_core::host::HostFacts;
use flyco_core::machine::{
    CloudProviderKind, MachineCatalogEntry, MachinePricing, OsFamily, Runtime,
};
use flyco_core::{HostId, MachineId};
use serde::{Deserialize, Serialize};

use crate::{DaemonBootstrap, MachineOperation, ProviderError};

/// Driver name, as it appears in [`ProviderError::Unsupported`].
pub const PROVIDER: &str = "host";

/// The container image a session runs in unless the user names another.
///
/// It must carry `flycod` and the harness CLIs, and its entrypoint must
/// decode [`CONFIG_ENV`] into a `flycod` configuration and run
/// `flycod run` against it.
pub const DEFAULT_IMAGE: &str = flyco_core::release::SESSION_IMAGE_LATEST;

/// Environment variable the image reads the base64 `flycod` configuration
/// out of.
///
/// The configuration carries the session's daemon token, so it is handed
/// over on the container's stdin as an env-file rather than as a
/// command-line argument: an argument would be visible in the host's process
/// table for as long as `podman run` took to return.
pub const CONFIG_ENV: &str = "FLYCO_DAEMON_CONFIG";

/// The value [`CONFIG_ENV`] carries: base64 of the rendered `flycod`
/// configuration.
///
/// Beside the variable rather than in each driver that sets it, because it
/// is one half of one contract: the image decodes exactly this, and the four
/// runtimes that start that image — a machine the user owns, Azure Container
/// Apps, Cloud Run and ECS on Fargate — must not be able to disagree about
/// the encoding.
///
/// # Errors
///
/// Returns [`ProviderError::Malformed`] if the configuration does not
/// render, which would mean [`crate::flycod`] is broken rather than anything
/// the caller did.
pub fn encoded_config(bootstrap: &DaemonBootstrap) -> Result<String, ProviderError> {
    use base64::Engine as _;

    let config = crate::flycod::render(bootstrap)
        .map_err(|_| ProviderError::Malformed("the flycod configuration did not render"))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(config))
}

/// Prefix every flyco container and volume name carries, so a host shared
/// with other work stays legible.
const RESOURCE_PREFIX: &str = "flyco-";

/// Suffix distinguishing a machine's volume from its container.
const VOLUME_SUFFIX: &str = "-work";

/// The container name for one machine.
///
/// Derived from the machine id rather than allocated, so the name is
/// recoverable from the database without another column and two callers can
/// never disagree about it. A UUID is already a legal Podman name.
#[must_use]
pub fn container_name(machine: MachineId) -> String {
    let mut name = String::with_capacity(RESOURCE_PREFIX.len() + 36);
    name.push_str(RESOURCE_PREFIX);
    name.push_str(&machine.to_string());
    name
}

/// The volume name holding one machine's checkout.
///
/// Derived from the same id for the same reason, and separate from the
/// container because the two have different lifetimes: stopping a session
/// keeps the volume, and so does destroying a machine whose disk is being
/// kept for the archive snapshot.
#[must_use]
pub fn volume_name(machine: MachineId) -> String {
    let mut name = container_name(machine);
    name.push_str(VOLUME_SUFFIX);
    name
}

/// The machine a flyco container or volume name identifies.
///
/// The inverse of [`container_name`] and [`volume_name`], and the reason
/// neither of them allocates: the machine an operation acts on is
/// recoverable from the name it produced, so a host answering a job it was
/// sent does not need the control plane to have told it the id twice.
/// `None` for a name flyco did not derive — a container the user runs on
/// their own machine, or one a later version of this table names differently.
#[must_use]
pub fn machine_named(name: &str) -> Option<MachineId> {
    let id = name.strip_prefix(RESOURCE_PREFIX)?;
    id.strip_suffix(VOLUME_SUFFIX).unwrap_or(id).parse().ok()
}

/// One unit of container lifecycle work, as the control plane sends it.
///
/// Serializable on purpose: this is what travels down a host's socket, and
/// the process that executes it is not the process that planned it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "job", rename_all = "snake_case")]
pub enum ContainerJob {
    /// Create the volume and the container, start it, and hand it the
    /// daemon's configuration.
    Create {
        /// Container to create.
        container: String,
        /// Volume to mount as the session's working directory, created if
        /// it does not exist. A session put back onto its host after a
        /// restart finds the checkout it left.
        volume: String,
        /// Image to create it from.
        image: String,
        /// The machine this container is.
        machine: MachineId,
        /// What its `flycod` needs to phone home.
        ///
        /// Boxed because it is much the largest thing any variant of this
        /// enum carries — three credentials, a repository, a commit
        /// identity and the machine the session is on — and every other
        /// variant is a name or two. Without the indirection each `Stop`
        /// on a socket would be padded out to the size of a `Create`. The
        /// JSON is unchanged: a `Box` serializes as what it holds.
        bootstrap: Box<DaemonBootstrap>,
    },
    /// Stop the container, keeping its volume.
    Stop {
        /// Container to stop.
        container: String,
    },
    /// Start a stopped container again, on the volume it kept.
    Start {
        /// Container to start.
        container: String,
    },
    /// Remove the container, and the volume unless the disk is being kept.
    Remove {
        /// Container to remove.
        container: String,
        /// The volume that container mounts.
        volume: String,
        /// Whether the session's work survives this.
        ///
        /// `true` for a machine destroyed with its disk preserved — a host
        /// being drained keeps volumes so an archive snapshot can still be
        /// taken — and `false` when the session is finished with.
        keep_volume: bool,
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
            | Self::Remove { container, .. } => container,
        }
    }

    /// The machine this job acts on, which is also the job's identity.
    ///
    /// A `Create` carries it outright; the others carry it in the container
    /// name, which is derived from it. This is what a host answers a
    /// [`JobResult`](wire::HostToControl::JobResult) with, so the answer
    /// names the same machine the job did without the wire carrying an id
    /// beside a name that already encodes it.
    #[must_use]
    pub fn machine(&self) -> Option<MachineId> {
        match self {
            Self::Create { machine, .. } => Some(*machine),
            Self::Stop { container }
            | Self::Start { container }
            | Self::Remove { container, .. } => machine_named(container),
        }
    }

    /// The job's name, for a log line or an error message.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Create { .. } => "create",
            Self::Stop { .. } => "stop",
            Self::Start { .. } => "start",
            Self::Remove { .. } => "remove",
        }
    }
}

/// An enrolled machine, and what flyco knows how to do with it.
///
/// Holds no credential: a host authenticates itself with the token it was
/// issued, and this half opens no connection to anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    id: HostId,
    facts: HostFacts,
    image: String,
}

impl Host {
    /// Describes an enrolled machine, running sessions from
    /// [`DEFAULT_IMAGE`].
    #[must_use]
    pub fn new(id: HostId, facts: HostFacts) -> Self {
        Self {
            id,
            facts,
            image: DEFAULT_IMAGE.to_owned(),
        }
    }

    /// Runs sessions from another image.
    #[must_use]
    pub fn with_image(mut self, image: impl Into<String>) -> Self {
        self.image = image.into();
        self
    }

    /// Which enrolled machine this is.
    #[must_use]
    pub const fn id(&self) -> HostId {
        self.id
    }

    /// The machine type this host answers to, which is its own hostname.
    ///
    /// A host is its own region and its own machine type: there is one
    /// machine here and the name a person recognises it by is what it calls
    /// itself.
    #[must_use]
    pub fn machine_type(&self) -> &str {
        &self.facts.hostname
    }

    /// The one machine this provider offers: the host itself.
    ///
    /// It carries no price, because the user already owns and already pays
    /// for the hardware, and its size and architecture are the ones the
    /// machine reported at its last `Hello` rather than any flyco invented.
    ///
    /// Its runtime is [`Runtime::Container`], which it always was: a session
    /// here has always been a Podman container, and the axis is what finally
    /// says so. It carries no [`free_grant`](MachineCatalogEntry::free_grant)
    /// — a grant is a provider giving compute away, and there is no provider
    /// in this arrangement.
    #[must_use]
    pub fn catalog(&self) -> Vec<MachineCatalogEntry> {
        vec![MachineCatalogEntry {
            // Stamped by the control plane, which knows the row.
            account: None,
            provider: CloudProviderKind::Host,
            region: self.facts.hostname.clone(),
            machine_type: self.facts.hostname.clone(),
            runtime: Runtime::Container,
            free_grant: None,
            os: OsFamily::Linux,
            capacity: Some(self.facts.capacity()),
            lineage: Some(self.facts.lineage()),
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
                if request.spec.machine_type != self.machine_type() {
                    return Err(ProviderError::Unsupported {
                        provider: PROVIDER,
                        operation: "provisioning another machine",
                        reason: "an enrolled host offers exactly one machine type — itself",
                    });
                }
                if request.spec.runtime != Runtime::Container {
                    // The one entry this host publishes is a container, so a
                    // spec asking for a VM was built against something else.
                    // Refused rather than quietly satisfied with a container:
                    // the caller asked for a disk that survives a stop, and
                    // this cannot give one.
                    return Err(ProviderError::Unsupported {
                        provider: PROVIDER,
                        operation: "provisioning a virtual machine",
                        reason: "an enrolled host runs sessions as containers; \
                                 it has no hypervisor to make a virtual machine with",
                    });
                }
                Ok(ContainerJob::Create {
                    container: container_name(request.machine),
                    volume: volume_name(request.machine),
                    image: self.image.clone(),
                    machine: request.machine,
                    bootstrap: Box::new(request.bootstrap.clone()),
                })
            }
            MachineOperation::Resize { .. } => Err(ProviderError::Unsupported {
                provider: PROVIDER,
                operation: "resize",
                reason: "an enrolled host has the hardware it has; \
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
                volume: volume_name(machine.id),
                keep_volume: false,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use flyco_core::host::HostFacts;
    use flyco_core::machine::{
        CloudProviderKind, CpuArchitecture, MachinePricing, MachineSpec, MachineState, Runtime,
    };
    use flyco_core::{HostId, MachineId, PermissionMode, SessionId};

    use super::{ContainerJob, DEFAULT_IMAGE, Host, PROVIDER, container_name, volume_name};
    use crate::{
        CapacityMode, ClaudeCredential, DaemonBootstrap, HarnessCredential, Machine,
        MachineOperation, ProviderError, ProvisionRequest,
    };

    pub const HOSTNAME: &str = "build.lexo.cool";

    /// What the machine in these tests said about itself.
    pub fn facts() -> HostFacts {
        HostFacts {
            architecture: CpuArchitecture::Arm64,
            vcpus: 10,
            memory_mib: 32 * 1024,
            disk_free_gib: 400,
            podman_version: "5.4.0".to_owned(),
            kernel: "6.11.0-19-generic".to_owned(),
            hostname: HOSTNAME.to_owned(),
        }
    }

    pub fn host() -> Host {
        Host::new(HostId::generate(), facts())
    }

    pub fn bootstrap() -> DaemonBootstrap {
        DaemonBootstrap {
            session: SessionId::generate(),
            provider: CloudProviderKind::Host,
            runtime: Runtime::Container,
            control_plane_url: "https://flyco.dev/".to_owned(),
            daemon_token: "fd_token".to_owned(),
            permission_mode: PermissionMode::Default,
            auth: HarnessCredential::ClaudeCode(ClaudeCredential::Inherit),
            repo: crate::testing::checkout(),
            machine_origin: flyco_core::MachineOrigin::Auto,
            machine: crate::testing::session_machine(),
            resume_session_id: None,
            model: crate::testing::session_model(),
            mcp_servers: crate::testing::mcp_servers(),
        }
    }

    fn machine(id: MachineId) -> Machine {
        Machine {
            id,
            native_id: container_name(id),
            runtime: flyco_core::Runtime::Container,
            region: HOSTNAME.to_owned(),
            state: MachineState::Running,
            capacity_mode: CapacityMode::OnDemand,
            address: Some(HOSTNAME.to_owned()),
        }
    }

    pub fn provision(machine_type: &str) -> MachineOperation {
        MachineOperation::Provision(Box::new(ProvisionRequest {
            machine: MachineId::generate(),
            spec: MachineSpec {
                provider: CloudProviderKind::Host,
                machine_type: machine_type.to_owned(),
                runtime: Runtime::Container,
                region: HOSTNAME.to_owned(),
                spot: false,
                disk_gib: 0,
            },
            bootstrap: bootstrap(),
        }))
    }

    #[test]
    fn the_catalog_is_the_machine_itself_at_the_size_it_reported() {
        let catalog = host().catalog();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].machine_type, HOSTNAME);
        assert_eq!(catalog[0].region, HOSTNAME);
        assert_eq!(catalog[0].pricing, MachinePricing::UserOwned);
        assert_eq!(catalog[0].pricing.hourly(false), None);
        assert_eq!(
            catalog[0].runtime,
            Runtime::Container,
            "a session on an enrolled host has always been a Podman container"
        );
        assert!(
            catalog[0].free_grant.is_none(),
            "there is no provider here to give compute away"
        );
        assert_eq!(
            catalog[0].capacity.as_ref().map(|capacity| capacity.vcpus),
            Some(10)
        );
        assert_eq!(
            catalog[0]
                .lineage
                .as_ref()
                .map(|lineage| lineage.architecture),
            Some(CpuArchitecture::Arm64),
            "an arm64 host must never be offered as an x86-64 machine"
        );
    }

    #[test]
    fn a_host_refuses_to_provision_a_virtual_machine() {
        // The spec asked for a disk that survives a stop, and a host has no
        // hypervisor to make one with. Refused rather than quietly given a
        // container, which is a different bargain.
        let MachineOperation::Provision(mut request) = provision(HOSTNAME) else {
            panic!("provisioning plans a create");
        };
        request.spec.runtime = Runtime::Vm;

        let refusal = host()
            .plan(&MachineOperation::Provision(request))
            .expect_err("a host cannot make a virtual machine");
        assert!(
            matches!(
                refusal,
                ProviderError::Unsupported {
                    provider: PROVIDER,
                    operation: "provisioning a virtual machine",
                    ..
                }
            ),
            "unexpected refusal: {refusal}"
        );
    }

    #[test]
    fn provisioning_plans_a_container_and_a_volume_from_the_configured_image() {
        let host = host().with_image("localhost/flyco-session:dev");
        let job = host.plan(&provision(HOSTNAME)).expect("plan");

        let ContainerJob::Create {
            container,
            volume,
            image,
            machine,
            ..
        } = job
        else {
            panic!("provisioning plans a create");
        };
        assert_eq!(image, "localhost/flyco-session:dev");
        assert_eq!(container, container_name(machine));
        assert_eq!(volume, volume_name(machine));
        assert!(container.starts_with("flyco-"));
        assert_ne!(container, volume, "a container is not its own volume");
    }

    #[test]
    fn the_default_image_is_used_when_none_is_named() {
        let job = host().plan(&provision(HOSTNAME)).expect("plan");
        assert!(matches!(job, ContainerJob::Create { image, .. } if image == DEFAULT_IMAGE));
    }

    #[test]
    fn a_resize_is_refused_rather_than_ignored() {
        let error = host()
            .plan(&MachineOperation::Resize {
                machine: machine(MachineId::generate()),
                machine_type: "bigger".to_owned(),
            })
            .expect_err("an enrolled host cannot resize");

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
        host()
            .plan(&provision("some.other.host"))
            .expect_err("a host offers exactly one machine type");
    }

    #[test]
    fn the_lifecycle_operations_address_the_machines_own_container() {
        let host = host();
        let id = MachineId::generate();
        let container = container_name(id);

        for (operation, job) in [
            (
                MachineOperation::Deallocate {
                    machine: machine(id),
                },
                ContainerJob::Stop {
                    container: container.clone(),
                },
            ),
            (
                MachineOperation::Start {
                    machine: machine(id),
                },
                ContainerJob::Start {
                    container: container.clone(),
                },
            ),
            (
                MachineOperation::Destroy {
                    machine: machine(id),
                },
                ContainerJob::Remove {
                    container,
                    volume: volume_name(id),
                    keep_volume: false,
                },
            ),
        ] {
            assert_eq!(host.plan(&operation).expect("plan"), job);
        }
    }

    #[test]
    fn every_job_names_the_machine_it_acts_on() {
        let host = host();
        let id = MachineId::generate();
        let create = host.plan(&provision(HOSTNAME)).expect("plan");
        let ContainerJob::Create {
            machine: planned, ..
        } = &create
        else {
            panic!("provisioning plans a create");
        };

        assert_eq!(create.machine(), Some(*planned));
        for operation in [
            MachineOperation::Deallocate {
                machine: machine(id),
            },
            MachineOperation::Start {
                machine: machine(id),
            },
            MachineOperation::Destroy {
                machine: machine(id),
            },
        ] {
            assert_eq!(host.plan(&operation).expect("plan").machine(), Some(id));
        }
    }

    #[test]
    fn a_name_flyco_did_not_derive_identifies_no_machine() {
        let id = MachineId::generate();

        assert_eq!(super::machine_named(&container_name(id)), Some(id));
        assert_eq!(
            super::machine_named(&volume_name(id)),
            Some(id),
            "a volume names the machine whose work is on it"
        );
        assert_eq!(super::machine_named("postgres"), None);
        assert_eq!(super::machine_named("flyco-not-a-uuid"), None);
    }

    #[test]
    fn a_job_round_trips_down_the_socket_without_leaking_the_token() {
        let job = host().plan(&provision(HOSTNAME)).expect("plan");
        let encoded = serde_json::to_string(&job).expect("serialize");
        let back: ContainerJob = serde_json::from_str(&encoded).expect("deserialize");

        assert_eq!(back, job);
        assert_eq!(back.name(), "create");
        assert!(
            !format!("{job:?}").contains("fd_token"),
            "a job in flight must not print its daemon token"
        );
    }
}
