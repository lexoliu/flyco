//! Performing a [`ContainerJob`] on a real host, over a real SSH
//! connection.
//!
//! Native only. The module is behind the `ssh` feature, which the Cloudflare
//! Worker build does not enable, so the Worker's dependency graph contains no
//! SSH client at all — see the [module documentation](super) for why the
//! provider is split this way.
//!
//! # What runs on the host
//!
//! One shell script per job, rendered from a template under
//! `templates/byo_ssh/` with every interpolated value shell-quoted first.
//! Podman is invoked, nothing else; flyco never writes to the host's
//! filesystem, and the session's `flycod` configuration — which carries its
//! daemon token — reaches the container through an env-file on the remote
//! shell's stdin rather than as a command-line argument a `ps` would show.
//!
//! # Host keys
//!
//! The server's key is checked against the fingerprint the user registered
//! with the host. There is no trust-on-first-use path and no "accept any
//! key" switch: flyco is about to hand this host a live session credential,
//! and an unverified host key means handing it to whoever answered.

use core::fmt;
use core::future::{Future, ready};
use std::sync::Arc;

use askama::Template;
use base64::Engine as _;
use flyco_core::machine::{MachineCatalogEntry, MachineState};
use russh::client::{self, Handle};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate, decode_secret_key};
use russh::{ChannelMsg, Disconnect};

use super::{ByoSsh, CONFIG_ENV, ContainerJob, PROVIDER};
use crate::{
    CapacityMode, CloudProvider, Machine, MachineOperation, ProviderError, ProvisionRequest, flycod,
};

/// How the flyco user reaches a registered host.
///
/// The private key is a credential and never renders: the [`fmt::Debug`]
/// below is what stops it appearing in a provisioning log.
#[derive(Clone, PartialEq, Eq)]
pub struct SshHost {
    /// Hostname or address to dial.
    pub address: String,
    /// SSH port.
    pub port: u16,
    /// Login user, which must be able to run Podman.
    pub user: String,
    /// PEM-encoded private key flyco authenticates with.
    pub private_key: String,
    /// The server key flyco expects, as `ssh-keygen -lf` prints it:
    /// `SHA256:` followed by unpadded base64.
    pub host_fingerprint: String,
}

impl fmt::Debug for SshHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshHost")
            .field("address", &self.address)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("host_fingerprint", &self.host_fingerprint)
            .finish_non_exhaustive()
    }
}

/// What running one script produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutcome {
    /// The remote process's exit status.
    pub exit_status: u32,
    /// Everything it wrote to stdout and stderr, in arrival order.
    pub output: String,
}

impl CommandOutcome {
    /// Turns a non-zero exit into a provider error naming what failed.
    fn require_success(self, operation: &'static str) -> Result<Self, ProviderError> {
        if self.exit_status == 0 {
            return Ok(self);
        }
        Err(ProviderError::OperationFailed {
            status: "Failed".to_owned(),
            code: format!("podman exit {}", self.exit_status),
            message: format!("{operation}: {}", self.output.trim()),
        })
    }
}

/// Somewhere to run a shell script as the registered host's login user.
///
/// A trait rather than a concrete SSH session so the rendered scripts are
/// assertable: the templates below are the contract with the host, and a
/// test that could only check them against a live machine would not be run.
pub trait CommandRunner {
    /// Runs one script and waits for it to finish.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the script could not be run at all. A
    /// script that ran and failed comes back as a non-zero
    /// [`CommandOutcome::exit_status`].
    fn run(
        &mut self,
        script: &str,
    ) -> impl Future<Output = Result<CommandOutcome, ProviderError>> + Send;
}

/// The byo-ssh half that actually does something.
///
/// The only implementation of [`CloudProvider`] for byo-ssh, and it exists
/// only on native targets.
#[derive(Debug)]
pub struct SshExecutor<R> {
    host: ByoSsh,
    runner: R,
}

impl<R: CommandRunner> SshExecutor<R> {
    /// Wraps a runner that can reach the registered host.
    pub const fn new(host: ByoSsh, runner: R) -> Self {
        Self { host, runner }
    }

    /// Plans an operation and performs it.
    ///
    /// # Errors
    ///
    /// Returns whatever [`ByoSsh::plan`] refuses, or the failure of the
    /// script it produced.
    pub async fn perform(&mut self, operation: &MachineOperation) -> Result<(), ProviderError> {
        let job = self.host.plan(operation)?;
        let script = render(&job)?;
        tracing::info!(
            container = job.container(),
            operation = operation.name(),
            "running a podman job on a registered host"
        );
        self.runner
            .run(&script)
            .await?
            .require_success(operation.name())?;
        Ok(())
    }

    fn machine_at(&self, machine: &Machine, state: MachineState) -> Machine {
        Machine {
            state,
            address: Some(self.host.address().to_owned()),
            ..machine.clone()
        }
    }
}

/// Renders the script for one job.
///
/// Every interpolated value is shell-quoted before it reaches the template,
/// so a container name or an image reference cannot become another command.
fn render(job: &ContainerJob) -> Result<String, ProviderError> {
    let rendered = match job {
        ContainerJob::Create {
            container,
            image,
            machine,
            bootstrap,
        } => {
            let config = flycod::render(bootstrap)
                .map_err(|_| ProviderError::Malformed("the flycod configuration did not render"))?;
            CreateScript {
                container: quote(container),
                image: quote(image),
                machine: quote(&machine.to_string()),
                session: quote(&bootstrap.session.to_string()),
                config_env: CONFIG_ENV,
                config_base64: base64::engine::general_purpose::STANDARD.encode(config),
            }
            .render()
        }
        ContainerJob::Stop { container } => StopScript {
            container: quote(container),
        }
        .render(),
        ContainerJob::Start { container } => StartScript {
            container: quote(container),
        }
        .render(),
        ContainerJob::Remove { container } => RemoveScript {
            container: quote(container),
        }
        .render(),
    };

    rendered.map_err(|_| ProviderError::Malformed("a podman script template did not render"))
}

/// POSIX-quotes one value for the remote shell.
fn quote(value: &str) -> String {
    shell_escape::unix::escape(value.into()).into_owned()
}

#[derive(Template)]
#[template(path = "byo_ssh/create.sh", escape = "none")]
struct CreateScript {
    container: String,
    image: String,
    machine: String,
    session: String,
    config_env: &'static str,
    config_base64: String,
}

#[derive(Template)]
#[template(path = "byo_ssh/stop.sh", escape = "none")]
struct StopScript {
    container: String,
}

#[derive(Template)]
#[template(path = "byo_ssh/start.sh", escape = "none")]
struct StartScript {
    container: String,
}

#[derive(Template)]
#[template(path = "byo_ssh/remove.sh", escape = "none")]
struct RemoveScript {
    container: String,
}

impl<R: CommandRunner> CloudProvider for SshExecutor<R> {
    fn catalog(&mut self) -> impl Future<Output = Result<Vec<MachineCatalogEntry>, ProviderError>> {
        // The catalog of a registered host is known without asking it
        // anything, so this answers immediately rather than being an
        // `async fn` that never suspends.
        ready(Ok(self.host.catalog()))
    }

    async fn provision(&mut self, request: &ProvisionRequest) -> Result<Machine, ProviderError> {
        self.perform(&MachineOperation::Provision(Box::new(request.clone())))
            .await?;
        Ok(Machine {
            id: request.machine,
            native_id: super::container_name(request.machine),
            // A registered host is its own region, and there is nowhere else
            // to put it — the same answer `catalog` gives.
            region: self.host.address().to_owned(),
            state: MachineState::Running,
            // A container on hardware the user owns is never interruptible,
            // whatever the session asked for.
            capacity_mode: CapacityMode::OnDemand,
            address: Some(self.host.address().to_owned()),
        })
    }

    fn resize(
        &mut self,
        _machine: &Machine,
        _new_machine_type: &str,
    ) -> impl Future<Output = Result<Machine, ProviderError>> {
        ready(Err(ProviderError::Unsupported {
            provider: PROVIDER,
            operation: "resize",
            reason: "a registered host has the hardware it has; \
                     start a session on a cloud provider to change machine size",
        }))
    }

    async fn deallocate(&mut self, machine: &Machine) -> Result<(), ProviderError> {
        self.perform(&MachineOperation::Deallocate {
            machine: machine.clone(),
        })
        .await
    }

    async fn start(&mut self, machine: &Machine) -> Result<Machine, ProviderError> {
        self.perform(&MachineOperation::Start {
            machine: machine.clone(),
        })
        .await?;
        Ok(self.machine_at(machine, MachineState::Running))
    }

    async fn destroy(&mut self, machine: &Machine) -> Result<(), ProviderError> {
        self.perform(&MachineOperation::Destroy {
            machine: machine.clone(),
        })
        .await
    }
}

/// The production [`CommandRunner`]: one russh session per job.
///
/// A session is opened per operation rather than held open, because a
/// provisioning job may be minutes or hours apart from the next one and a
/// half-dead TCP connection is a worse starting point than a fresh dial.
#[derive(Debug)]
pub struct SshCommandRunner {
    host: SshHost,
}

impl SshCommandRunner {
    /// Prepares a runner for one registered host.
    #[must_use]
    pub const fn new(host: SshHost) -> Self {
        Self { host }
    }

    async fn connect(&self) -> Result<Handle<FingerprintCheck>, ProviderError> {
        let key = decode_secret_key(&self.host.private_key, None).map_err(|_| {
            ProviderError::Malformed("the registered SSH private key did not parse")
        })?;

        let config = Arc::new(client::Config::default());
        let checker = FingerprintCheck {
            expected: self.host.host_fingerprint.clone(),
        };

        let mut session = client::connect(
            config,
            (self.host.address.as_str(), self.host.port),
            checker,
        )
        .await
        .map_err(|error| ProviderError::Rejected(format!("SSH connection failed: {error}")))?;

        let hash_alg = session
            .best_supported_rsa_hash()
            .await
            .map_err(|error| ProviderError::Rejected(format!("SSH negotiation failed: {error}")))?
            .flatten();

        let authenticated = session
            .authenticate_publickey(
                self.host.user.clone(),
                PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg),
            )
            .await
            .map_err(|error| {
                ProviderError::Rejected(format!("SSH authentication failed: {error}"))
            })?;

        if !authenticated.success() {
            return Err(ProviderError::Rejected(
                "the registered host rejected flyco's SSH key".to_owned(),
            ));
        }
        Ok(session)
    }
}

impl CommandRunner for SshCommandRunner {
    async fn run(&mut self, script: &str) -> Result<CommandOutcome, ProviderError> {
        let session = self.connect().await?;
        let mut channel = session
            .channel_open_session()
            .await
            .map_err(|error| ProviderError::Rejected(format!("SSH channel failed: {error}")))?;

        channel
            .exec(true, script.as_bytes())
            .await
            .map_err(|error| ProviderError::Rejected(format!("SSH exec failed: {error}")))?;

        let mut output = Vec::new();
        let mut exit_status = None;
        while let Some(message) = channel.wait().await {
            match message {
                ChannelMsg::Data { ref data } | ChannelMsg::ExtendedData { ref data, .. } => {
                    output.extend_from_slice(data);
                }
                ChannelMsg::ExitStatus { exit_status: code } => exit_status = Some(code),
                _ => {}
            }
        }

        // Disconnecting is best effort: the command has already run and its
        // status is what the caller needs, so a teardown failure must not
        // turn a successful provision into an error.
        if let Err(error) = session
            .disconnect(Disconnect::ByApplication, "", "en")
            .await
        {
            tracing::debug!(%error, "an SSH session did not close cleanly");
        }

        Ok(CommandOutcome {
            exit_status: exit_status.ok_or(ProviderError::Malformed(
                "the registered host closed the channel without an exit status",
            ))?,
            output: String::from_utf8_lossy(&output).into_owned(),
        })
    }
}

/// Accepts exactly the server key the user registered.
struct FingerprintCheck {
    expected: String,
}

impl client::Handler for FingerprintCheck {
    type Error = russh::Error;

    fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send {
        // Comparing two fingerprints suspends on nothing, so this answers
        // immediately rather than being an `async fn` that never awaits.
        let accepted = match server_public_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => {
                key.fingerprint(HashAlg::Sha256).to_string() == self.expected
            }
            // A host certificate would need its CA pinned instead, which
            // flyco does not model; refusing is the honest answer.
            PublicKeyOrCertificate::Certificate(_) => false,
        };
        ready(Ok(accepted))
    }
}

#[cfg(test)]
mod tests {
    use flyco_core::machine::{CloudProviderKind, MachineSpec, MachineState};
    use flyco_core::{MachineId, PermissionMode, SessionId};

    use core::future::{Future, ready};

    use super::{CommandOutcome, CommandRunner, SshExecutor, SshHost, render};
    use crate::byo_ssh::{ByoSsh, CONFIG_ENV, ContainerJob, container_name};
    use crate::{
        ClaudeCredential, CloudProvider, DaemonBootstrap, HarnessCredential, MachineOperation,
        ProviderError, ProvisionRequest,
    };

    const HOST: &str = "build.lexo.cool";
    const TOKEN: &str = "fd_a-live-daemon-token";

    /// Records the scripts it was asked to run and answers with a canned
    /// outcome.
    #[derive(Debug, Default)]
    struct RecordingRunner {
        scripts: Vec<String>,
        exit_status: u32,
    }

    impl CommandRunner for RecordingRunner {
        fn run(
            &mut self,
            script: &str,
        ) -> impl Future<Output = Result<CommandOutcome, ProviderError>> + Send {
            self.scripts.push(script.to_owned());
            ready(Ok(CommandOutcome {
                exit_status: self.exit_status,
                output: "podman said something".to_owned(),
            }))
        }
    }

    fn request(machine: MachineId) -> ProvisionRequest {
        ProvisionRequest {
            machine,
            spec: MachineSpec {
                provider: CloudProviderKind::ByoSsh,
                machine_type: HOST.to_owned(),
                region: HOST.to_owned(),
                spot: true,
                disk_gib: 0,
            },
            bootstrap: DaemonBootstrap {
                session: SessionId::generate(),
                control_plane_url: "https://flyco.dev/".to_owned(),
                daemon_token: TOKEN.to_owned(),
                permission_mode: PermissionMode::Default,
                auth: HarnessCredential::ClaudeCode(ClaudeCredential::Inherit),
                repo: crate::testing::checkout(),
                machine_origin: flyco_core::MachineOrigin::Auto,
                machine: crate::testing::session_machine(),
                resume_session_id: None,
            },
        }
    }

    #[tokio::test]
    async fn provisioning_runs_one_podman_run_and_reports_the_container() {
        let mut executor = SshExecutor::new(ByoSsh::new(HOST), RecordingRunner::default());
        let machine_id = MachineId::generate();

        let machine = executor
            .provision(&request(machine_id))
            .await
            .expect("provision");

        assert_eq!(machine.native_id, container_name(machine_id));
        assert_eq!(machine.state, MachineState::Running);
        assert_eq!(machine.address.as_deref(), Some(HOST));
        assert!(
            !machine.capacity_mode.is_spot(),
            "a container on the user's own hardware is never spot"
        );
    }

    #[tokio::test]
    async fn the_daemon_token_never_reaches_the_command_line() {
        let mut runner = RecordingRunner::default();
        let request = request(MachineId::generate());
        let script = render(
            &ByoSsh::new(HOST)
                .plan(&MachineOperation::Provision(Box::new(request)))
                .expect("plan"),
        )
        .expect("render");
        runner.scripts.push(script.clone());

        let (command_lines, heredoc): (Vec<&str>, Vec<&str>) = script
            .lines()
            .partition(|line| !line.starts_with(CONFIG_ENV));

        assert!(
            command_lines.iter().all(|line| !line.contains(TOKEN)),
            "the token must not appear in any podman argument"
        );
        assert_eq!(heredoc.len(), 1, "the config rides one env-file line");
        assert!(heredoc[0].starts_with(CONFIG_ENV));
        assert!(
            !heredoc[0].contains(TOKEN),
            "the config is base64, so even the env-file line is not the token verbatim"
        );
    }

    #[test]
    fn a_hostile_container_name_cannot_become_another_command() {
        let script = render(&ContainerJob::Stop {
            container: "flyco-x; rm -rf /".to_owned(),
        })
        .expect("render");

        assert!(script.contains("'flyco-x; rm -rf /'"));
        assert!(!script.contains("podman stop flyco-x; rm"));
    }

    #[tokio::test]
    async fn a_failing_script_is_an_error_rather_than_a_shrug() {
        let mut executor = SshExecutor::new(
            ByoSsh::new(HOST),
            RecordingRunner {
                scripts: Vec::new(),
                exit_status: 125,
            },
        );

        let error = executor
            .provision(&request(MachineId::generate()))
            .await
            .expect_err("a non-zero podman exit is a failure");
        assert!(matches!(error, ProviderError::OperationFailed { .. }));
    }

    #[tokio::test]
    async fn resize_is_refused_by_the_executor_too() {
        let mut executor = SshExecutor::new(ByoSsh::new(HOST), RecordingRunner::default());
        let machine = executor
            .provision(&request(MachineId::generate()))
            .await
            .expect("provision");

        assert!(matches!(
            executor.resize(&machine, "bigger").await,
            Err(ProviderError::Unsupported {
                operation: "resize",
                ..
            })
        ));
    }

    #[test]
    fn a_registered_host_never_debug_prints_its_private_key() {
        let host = SshHost {
            address: HOST.to_owned(),
            port: 22,
            user: "flyco".to_owned(),
            private_key: "-----BEGIN OPENSSH PRIVATE KEY-----\nsecret\n".to_owned(),
            host_fingerprint: "SHA256:abc".to_owned(),
        };
        let rendered = format!("{host:?}");
        assert!(!rendered.contains("secret"));
        assert!(rendered.contains("SHA256:abc"));
    }
}
