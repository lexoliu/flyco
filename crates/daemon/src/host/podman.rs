//! Performing a [`ContainerJob`] on this machine, under rootless Podman.
//!
//! The successor of the SSH executor flyco deleted with the byo-ssh
//! provider: the same rendered scripts, run here rather than pushed down a
//! connection the control plane cannot open. Nothing is planned in this
//! module and no shell is assembled in it — [`flyco_provider::host::script`]
//! renders every job, quoting included, and this runs exactly what it
//! produced.
//!
//! # Root, dropping to `flyco`
//!
//! The unit runs as root because `/etc/flyco/host.toml` holds this machine's
//! host token and nothing else may read it. Every podman command drops to
//! the unprivileged container user with `runuser`, so the containers
//! themselves are rootless: an agent with a shell inside one is inside a
//! user namespace on somebody's real machine, which is the entire point of
//! the sandbox.
//!
//! # Jobs are idempotent
//!
//! A host's room delivers at least once — a job is written to its SQLite
//! before it is forwarded and stays there until the machine answers, so a
//! stream that dropped between the work and the answer means the same job
//! arrives again. Every job therefore has to be safe to repeat:
//!
//! * A **create** whose container is already there starts it instead of
//!   rebuilding it. The create script begins by force-removing any container
//!   of that name, which for a redelivered job would destroy a *live*
//!   session's container; `podman start` on a container that is already
//!   running is the no-op the redelivery deserves.
//! * A **stop** whose container is gone is already done.
//! * A **remove** is `podman rm --force --ignore`, which says the same
//!   thing.

use core::future::Future;
use std::path::PathBuf;

use flyco_core::host::JobOutcome;
use flyco_provider::ProviderError;
use flyco_provider::host::{ContainerJob, script};

use super::config::PodmanConfig;

/// The program that drops root to the container user.
const RUNUSER: &str = "runuser";

/// The shell a rendered job script is handed to.
///
/// `-c`, not stdin: a create script feeds the session's configuration to
/// `podman run --env-file /dev/stdin` through a here-document, so the
/// shell's own standard input is not free to carry the script as well.
const SHELL: &str = "/bin/sh";

/// Podman itself, for the one thing that is a question rather than a job.
const PODMAN: &str = "podman";

/// `PATH` every podman command runs with.
///
/// Stated rather than inherited: what a systemd unit's environment holds is
/// not a thing to discover from a container that failed to start.
const PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// The program that reports a user's numeric id.
const ID: &str = "id";

/// A container job could not be performed at all.
///
/// Distinct from a job that ran and failed, which is a
/// [`JobOutcome::Failed`] carrying what Podman said: this is the machine
/// itself not being in a state to try.
#[derive(Debug, thiserror::Error)]
pub enum PodmanError {
    /// A program could not be run.
    #[error("could not run `{program}` on this machine")]
    Spawn {
        /// The program that was tried.
        program: String,
        /// The underlying cause.
        #[source]
        source: std::io::Error,
    },
    /// The container user does not exist, so nothing can be run as it.
    #[error("this machine has no `{user}` user to run containers as: {output}")]
    UnknownUser {
        /// The user the configuration names.
        user: String,
        /// What `id -u` said.
        output: String,
    },
    /// `id -u` answered something that is not a user id.
    #[error("`id -u {user}` answered {output:?}, which is not a user id")]
    NotAUserId {
        /// The user that was asked about.
        user: String,
        /// What was answered.
        output: String,
    },
    /// A script would not render, which is flyco's own bug rather than
    /// anything the machine did.
    #[error("a podman script could not be rendered")]
    Render(#[source] ProviderError),
    /// The script ran and Podman refused.
    #[error("{job}: {output}")]
    Refused {
        /// Which job it was.
        job: &'static str,
        /// What Podman said.
        output: String,
    },
}

/// One process to run: a program and its argument vector, never a shell
/// string.
///
/// Every argument is one element and the program is executed directly, so a
/// container name cannot be re-split into another command — the same
/// property the rendered scripts have, kept on the way to them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// Program to execute, resolved on `PATH`.
    pub program: String,
    /// Arguments, already split.
    pub args: Vec<String>,
}

/// What running one process produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutcome {
    /// Exit code, or `None` for a process a signal ended.
    pub code: Option<i32>,
    /// Everything it wrote to stdout and stderr.
    pub output: String,
}

impl CommandOutcome {
    /// Whether the process exited zero.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.code == Some(0)
    }
}

/// Somewhere to run a process on this machine.
///
/// A trait rather than a bare [`tokio::process::Command`] so the argument
/// vector every job produces is assertable: what reaches Podman on somebody
/// else's hardware is the contract, and a test that could only check it
/// against a live machine would not be run.
pub trait CommandRunner: Send + Sync {
    /// Runs one process to completion.
    ///
    /// # Errors
    ///
    /// Returns [`PodmanError::Spawn`] when the process could not be started.
    /// A process that ran and failed comes back as a non-zero
    /// [`CommandOutcome::code`].
    fn run(
        &self,
        invocation: Invocation,
    ) -> impl Future<Output = Result<CommandOutcome, PodmanError>> + Send;
}

/// The production runner: an actual child process.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessRunner;

impl CommandRunner for ProcessRunner {
    async fn run(&self, invocation: Invocation) -> Result<CommandOutcome, PodmanError> {
        let output = tokio::process::Command::new(&invocation.program)
            .args(&invocation.args)
            .output()
            .await
            .map_err(|source| PodmanError::Spawn {
                program: invocation.program.clone(),
                source,
            })?;

        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok(CommandOutcome {
            code: output.status.code(),
            output: text.trim().to_owned(),
        })
    }
}

/// The unprivileged user containers run as, and the environment they need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rootless {
    user: String,
    home: PathBuf,
    uid: u32,
}

impl Rootless {
    /// The user `uid` is, with `home` as its home directory.
    #[must_use]
    pub const fn new(user: String, home: PathBuf, uid: u32) -> Self {
        Self { user, home, uid }
    }

    /// Looks the configured container user up on this machine.
    ///
    /// The numeric id is needed rather than only the name: rootless Podman
    /// keeps its runtime state in `/run/user/<uid>`, and a daemon that
    /// guessed that path would leave every container without the socket its
    /// own user manager is listening on.
    ///
    /// # Errors
    ///
    /// Returns [`PodmanError`] if `id` cannot be run, the user does not
    /// exist, or what came back is not a number.
    pub async fn resolve<R: CommandRunner>(
        podman: &PodmanConfig,
        runner: &R,
    ) -> Result<Self, PodmanError> {
        let outcome = runner
            .run(Invocation {
                program: ID.to_owned(),
                args: vec!["-u".to_owned(), podman.user.clone()],
            })
            .await?;

        if !outcome.succeeded() {
            return Err(PodmanError::UnknownUser {
                user: podman.user.clone(),
                output: outcome.output,
            });
        }
        let uid = outcome
            .output
            .trim()
            .parse()
            .map_err(|_| PodmanError::NotAUserId {
                user: podman.user.clone(),
                output: outcome.output.clone(),
            })?;

        Ok(Self::new(podman.user.clone(), podman.home.clone(), uid))
    }

    /// The environment every podman command runs with.
    fn environment(&self) -> [String; 3] {
        [
            format!("HOME={}", self.home.display()),
            format!("XDG_RUNTIME_DIR=/run/user/{}", self.uid),
            format!("PATH={PATH}"),
        ]
    }

    /// One process, run as the container user.
    fn command<I>(&self, program: &str, arguments: I) -> Invocation
    where
        I: IntoIterator<Item = String>,
    {
        let mut args = vec![
            "-u".to_owned(),
            self.user.clone(),
            "--".to_owned(),
            "env".to_owned(),
        ];
        args.extend(self.environment());
        args.push(program.to_owned());
        args.extend(arguments);
        Invocation {
            program: RUNUSER.to_owned(),
            args,
        }
    }

    /// The invocation that runs one rendered job script.
    #[must_use]
    pub fn shell(&self, script: &str) -> Invocation {
        self.command(SHELL, ["-c".to_owned(), script.to_owned()])
    }

    /// The invocation that asks Podman a question.
    #[must_use]
    pub fn podman<const N: usize>(&self, arguments: [&str; N]) -> Invocation {
        self.command(PODMAN, arguments.map(ToOwned::to_owned))
    }
}

/// Somewhere a container job can be performed.
///
/// The relay is generic over this so its ordering — a job's result is
/// durable before the frame announcing it leaves — is assertable without a
/// machine that has Podman on it.
pub trait Jobs: Send + Sync + 'static {
    /// Performs one job and says what came of it.
    ///
    /// Infallible on purpose: every way a job can go wrong is something the
    /// control plane has to be *told*, because a session is waiting on the
    /// container it names. A failure that only reached this machine's log
    /// would be a session that never starts and never says why.
    fn perform(&self, job: ContainerJob) -> impl Future<Output = JobOutcome> + Send;
}

/// The half of the host provider that actually does something.
#[derive(Debug, Clone)]
pub struct LocalExecutor<R> {
    rootless: Rootless,
    runner: R,
}

impl<R: CommandRunner> LocalExecutor<R> {
    /// An executor that runs its jobs as `rootless` through `runner`.
    #[must_use]
    pub const fn new(rootless: Rootless, runner: R) -> Self {
        Self { rootless, runner }
    }

    /// Performs one job, or says why it could not be started at all.
    async fn attempt(&self, job: &ContainerJob) -> Result<JobOutcome, PodmanError> {
        match job {
            ContainerJob::Create {
                container, volume, ..
            } => {
                let script = if self.exists(container).await? {
                    tracing::info!(
                        container,
                        "this container is already here; starting it rather than rebuilding it"
                    );
                    render(&ContainerJob::Start {
                        container: container.clone(),
                    })?
                } else {
                    render(job)?
                };
                self.script(&script, job.name()).await?;
                Ok(JobOutcome::Running {
                    container: container.clone(),
                    volume: volume.clone(),
                })
            }
            ContainerJob::Stop { container } => {
                if self.exists(container).await? {
                    self.script(&render(job)?, job.name()).await?;
                } else {
                    tracing::info!(container, "nothing to stop: this container is already gone");
                }
                Ok(JobOutcome::Done)
            }
            ContainerJob::Start { .. } | ContainerJob::Remove { .. } => {
                self.script(&render(job)?, job.name()).await?;
                Ok(JobOutcome::Done)
            }
        }
    }

    /// Whether Podman knows a container by this name, running or not.
    async fn exists(&self, container: &str) -> Result<bool, PodmanError> {
        Ok(self
            .runner
            .run(self.rootless.podman(["container", "exists", container]))
            .await?
            .succeeded())
    }

    /// Runs one rendered script, refusing on a non-zero exit.
    async fn script(&self, script: &str, job: &'static str) -> Result<(), PodmanError> {
        let outcome = self.runner.run(self.rootless.shell(script)).await?;
        if outcome.succeeded() {
            return Ok(());
        }
        Err(PodmanError::Refused {
            job,
            output: outcome.output,
        })
    }
}

impl<R: CommandRunner + 'static> Jobs for LocalExecutor<R> {
    async fn perform(&self, job: ContainerJob) -> JobOutcome {
        tracing::info!(
            container = job.container(),
            job = job.name(),
            "running a podman job"
        );
        match self.attempt(&job).await {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::warn!(%error, container = job.container(), "a podman job failed");
                JobOutcome::Failed {
                    message: error.to_string(),
                }
            }
        }
    }
}

/// The script one job runs, as the control plane's own planner renders it.
fn render(job: &ContainerJob) -> Result<String, PodmanError> {
    script::render(job).map_err(PodmanError::Render)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use flyco_core::MachineId;
    use flyco_core::host::JobOutcome;
    use flyco_provider::host::{ContainerJob, DEFAULT_IMAGE, container_name, volume_name};
    use tokio::sync::mpsc;

    use super::{CommandOutcome, CommandRunner, Invocation, Jobs, LocalExecutor, Rootless};

    /// The uid the container user has in these tests.
    const UID: u32 = 1001;

    /// A runner that records what it was asked to run and answers from a
    /// script of exit codes.
    #[derive(Debug)]
    struct FakeRunner {
        ran: mpsc::UnboundedSender<Invocation>,
        /// Exit codes, in order; anything past the end exits zero.
        codes: Mutex<std::collections::VecDeque<i32>>,
    }

    impl FakeRunner {
        fn new(codes: &[i32]) -> (Self, mpsc::UnboundedReceiver<Invocation>) {
            let (ran, received) = mpsc::unbounded_channel();
            (
                Self {
                    ran,
                    codes: Mutex::new(codes.iter().copied().collect()),
                },
                received,
            )
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(
            &self,
            invocation: Invocation,
        ) -> impl core::future::Future<Output = Result<CommandOutcome, super::PodmanError>> + Send
        {
            self.ran.send(invocation).expect("record");
            let code = self
                .codes
                .lock()
                .expect("the fake runner's script")
                .pop_front()
                .unwrap_or(0);
            core::future::ready(Ok(CommandOutcome {
                code: Some(code),
                output: if code == 0 {
                    String::new()
                } else {
                    "podman: no space left on device".to_owned()
                },
            }))
        }
    }

    fn rootless() -> Rootless {
        Rootless::new("flyco".to_owned(), PathBuf::from("/home/flyco"), UID)
    }

    fn create(machine: MachineId) -> ContainerJob {
        ContainerJob::Create {
            container: container_name(machine),
            volume: volume_name(machine),
            image: DEFAULT_IMAGE.to_owned(),
            machine,
            bootstrap: Box::new(crate::testing::bootstrap()),
        }
    }

    /// The argument vector every job's process carries, up to the program
    /// being run.
    fn prefix() -> Vec<String> {
        [
            "-u",
            "flyco",
            "--",
            "env",
            "HOME=/home/flyco",
            "XDG_RUNTIME_DIR=/run/user/1001",
            "PATH=/usr/local/bin:/usr/bin:/bin",
        ]
        .map(ToOwned::to_owned)
        .to_vec()
    }

    /// Everything the executor ran, in order.
    fn drain(received: &mut mpsc::UnboundedReceiver<Invocation>) -> Vec<Invocation> {
        let mut ran = Vec::new();
        while let Ok(invocation) = received.try_recv() {
            ran.push(invocation);
        }
        ran
    }

    #[tokio::test]
    async fn creating_asks_whether_the_container_is_there_and_then_runs_podman_run() {
        let machine = MachineId::generate();
        let (runner, mut received) = FakeRunner::new(&[1]);
        let executor = LocalExecutor::new(rootless(), runner);

        let outcome = executor.perform(create(machine)).await;

        let ran = drain(&mut received);
        assert_eq!(ran.len(), 2, "an existence check, then the create");

        let mut expected = prefix();
        expected.extend(
            ["podman", "container", "exists", &container_name(machine)].map(ToOwned::to_owned),
        );
        assert_eq!(ran[0].program, "runuser");
        assert_eq!(ran[0].args, expected);

        assert_eq!(ran[1].program, "runuser");
        assert_eq!(ran[1].args[..prefix().len()], prefix()[..]);
        assert_eq!(ran[1].args[prefix().len()], "/bin/sh");
        assert_eq!(ran[1].args[prefix().len() + 1], "-c");
        let script = &ran[1].args[prefix().len() + 2];
        assert!(script.contains("podman run --detach"), "{script}");
        assert!(
            script.contains(&format!("--name {}", container_name(machine))),
            "{script}"
        );
        assert!(
            script.contains(&format!("podman volume exists {}", volume_name(machine))),
            "{script}"
        );

        assert_eq!(
            outcome,
            JobOutcome::Running {
                container: container_name(machine),
                volume: volume_name(machine),
            }
        );
    }

    #[tokio::test]
    async fn creating_a_container_that_is_already_there_starts_it_rather_than_rebuilding_it() {
        let machine = MachineId::generate();
        // The existence check exits zero: this container is already here.
        let (runner, mut received) = FakeRunner::new(&[0]);
        let executor = LocalExecutor::new(rootless(), runner);

        let outcome = executor.perform(create(machine)).await;

        let ran = drain(&mut received);
        assert_eq!(ran.len(), 2);
        let script = ran[1].args.last().expect("a script");
        assert_eq!(
            script.trim(),
            format!("set -eu\npodman start {}", container_name(machine)),
            "a redelivered create must not force-remove a live session's container"
        );
        assert_eq!(
            outcome,
            JobOutcome::Running {
                container: container_name(machine),
                volume: volume_name(machine),
            },
            "and it is still the same container and volume that now exist"
        );
    }

    #[tokio::test]
    async fn stopping_a_container_that_is_gone_is_already_done() {
        let machine = MachineId::generate();
        let (runner, mut received) = FakeRunner::new(&[1]);
        let executor = LocalExecutor::new(rootless(), runner);

        let outcome = executor
            .perform(ContainerJob::Stop {
                container: container_name(machine),
            })
            .await;

        assert_eq!(outcome, JobOutcome::Done);
        let ran = drain(&mut received);
        assert_eq!(ran.len(), 1, "only the existence check ran: {ran:?}");
    }

    #[tokio::test]
    async fn stopping_a_container_that_is_there_stops_it() {
        let machine = MachineId::generate();
        let (runner, mut received) = FakeRunner::new(&[0]);
        let executor = LocalExecutor::new(rootless(), runner);

        let outcome = executor
            .perform(ContainerJob::Stop {
                container: container_name(machine),
            })
            .await;

        assert_eq!(outcome, JobOutcome::Done);
        let ran = drain(&mut received);
        assert_eq!(
            ran[1].args.last().expect("a script").trim(),
            format!("set -eu\npodman stop {}", container_name(machine))
        );
    }

    #[tokio::test]
    async fn starting_runs_podman_start_and_asks_nothing_first() {
        let machine = MachineId::generate();
        let (runner, mut received) = FakeRunner::new(&[]);
        let executor = LocalExecutor::new(rootless(), runner);

        let outcome = executor
            .perform(ContainerJob::Start {
                container: container_name(machine),
            })
            .await;

        assert_eq!(outcome, JobOutcome::Done);
        let ran = drain(&mut received);
        assert_eq!(ran.len(), 1);
        assert_eq!(
            ran[0].args.last().expect("a script").trim(),
            format!("set -eu\npodman start {}", container_name(machine))
        );
    }

    #[tokio::test]
    async fn removing_keeps_the_volume_exactly_when_it_was_told_to() {
        for (keep_volume, keeps) in [(true, false), (false, true)] {
            let machine = MachineId::generate();
            let (runner, mut received) = FakeRunner::new(&[]);
            let executor = LocalExecutor::new(rootless(), runner);

            let outcome = executor
                .perform(ContainerJob::Remove {
                    container: container_name(machine),
                    volume: volume_name(machine),
                    keep_volume,
                })
                .await;

            assert_eq!(outcome, JobOutcome::Done);
            let ran = drain(&mut received);
            let script = ran[0].args.last().expect("a script");
            assert!(
                script.contains(&format!(
                    "podman rm --force --ignore {}",
                    container_name(machine)
                )),
                "{script}"
            );
            assert_eq!(
                script.contains(&format!(
                    "podman volume rm --force {}",
                    volume_name(machine)
                )),
                keeps,
                "keep_volume = {keep_volume}: {script}"
            );
        }
    }

    #[tokio::test]
    async fn a_job_podman_refuses_is_reported_rather_than_thrown_away() {
        let machine = MachineId::generate();
        // The existence check says the container is not there; the create
        // then fails.
        let (runner, _received) = FakeRunner::new(&[1, 125]);
        let executor = LocalExecutor::new(rootless(), runner);

        let outcome = executor.perform(create(machine)).await;

        let JobOutcome::Failed { message } = outcome else {
            panic!("a refused job fails: {outcome:?}");
        };
        assert!(message.contains("create"), "{message}");
        assert!(message.contains("no space left on device"), "{message}");
    }

    #[tokio::test]
    async fn the_container_user_is_looked_up_rather_than_assumed() {
        let (runner, mut received) = FakeRunner::new(&[]);
        let resolved = Rootless::resolve(&super::PodmanConfig::default(), &runner)
            .await
            .expect_err("the fake runner answers an empty id");

        let ran = drain(&mut received);
        assert_eq!(ran[0].program, "id");
        assert_eq!(ran[0].args, ["-u", "flyco"]);
        assert!(
            matches!(resolved, super::PodmanError::NotAUserId { .. }),
            "{resolved:?}"
        );
    }
}
