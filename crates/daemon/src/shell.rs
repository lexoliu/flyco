//! One-off shell commands, for the composer's `!` prefix (docs/ux.md §9.3).
//!
//! A message beginning with `!` is not a prompt: it is the user talking to
//! the machine. The room turns it into a
//! [`ControlToDaemon::RunShell`](flyco_core::ControlToDaemon::RunShell) and
//! this is what runs it — `bash -c` in the session's working directory, as
//! whoever `flycod` runs as, which on a provisioned machine is the agent's
//! own `flyco` user. Nothing about it is shown to the harness.
//!
//! Two bounds, both of them the point rather than a precaution:
//!
//! * **Time.** A command that never returns would hold the machine's one
//!   shell slot for ever. The run is killed when the timeout elapses and
//!   the transcript says so.
//! * **Output.** Every chunk is appended to the session room's durable
//!   stream, so a command that prints a gigabyte would be a gigabyte in a
//!   Durable Object. Output past the cap is dropped and the exit frame
//!   carries `truncated`, because eliding it silently would misreport what
//!   the machine printed.
//!
//! This is not the [web terminal](crate::terminal): that is a PTY with a
//! shell living in it, whose bytes are live-only and belong to no
//! transcript row. A `!` command is one command, with one exit status, in
//! the conversation.

use core::time::Duration;
use std::path::PathBuf;
use std::process::Stdio;

use flyco_core::{ShellOutcome, ShellRunId, ShellStream};
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

/// How much of a pipe is read at a time.
const CHUNK: usize = 4096;

/// One thing that happened to a running command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellEvent {
    /// Some of the command's output.
    Output {
        /// Which stream it came from.
        stream: ShellStream,
        /// The bytes, decoded UTF-8 lossy.
        data: String,
    },
    /// The command ended. Exactly one of these per run.
    Exited {
        /// How it ended.
        outcome: ShellOutcome,
        /// Whether output was dropped after the run's byte cap.
        truncated: bool,
    },
}

/// One [`ShellEvent`], and which run it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellUpdate {
    /// The run the control plane named.
    pub run: ShellRunId,
    /// What happened to it.
    pub event: ShellEvent,
}

/// A command in flight.
///
/// Dropping it lets the command run on; [`cancel`](Self::cancel) is what
/// Stop reaches, and the run answers it with
/// [`ShellOutcome::Cancelled`] rather than simply disappearing.
#[derive(Debug)]
pub struct ShellRun {
    cancel: oneshot::Sender<()>,
}

impl ShellRun {
    /// A handle that cancels through `cancel`.
    #[must_use]
    pub const fn new(cancel: oneshot::Sender<()>) -> Self {
        Self { cancel }
    }

    /// Asks the run to stop.
    ///
    /// Consuming, because a run is cancelled once: the daemon holds one
    /// handle and lets go of it in the same move.
    pub fn cancel(self) {
        // A closed receiver is a run that has already finished, which is
        // exactly what the caller wanted.
        let _ = self.cancel.send(());
    }
}

/// Starts shell commands on the session machine.
pub trait Shell: Send + 'static {
    /// Starts `command`, streaming its output into `updates` and ending
    /// with exactly one [`ShellEvent::Exited`].
    ///
    /// Returns as soon as the command is under way: a `!` command runs
    /// beside the session rather than in front of it, so the relay keeps
    /// pumping the harness while a build runs.
    fn start(
        &self,
        run: ShellRunId,
        command: String,
        updates: mpsc::UnboundedSender<ShellUpdate>,
    ) -> ShellRun;
}

/// `bash -c` in the session's working directory.
#[derive(Debug, Clone)]
pub struct Bash {
    /// The bash executable. A bare name is resolved through `PATH`.
    program: PathBuf,
    /// Where the command runs: the session's checkout.
    workdir: PathBuf,
    /// How long a command may run before it is killed.
    timeout: Duration,
    /// How much output one run may put in the transcript.
    max_output_bytes: usize,
}

impl Bash {
    /// A runner for one session's machine.
    #[must_use]
    pub const fn new(
        program: PathBuf,
        workdir: PathBuf,
        timeout: Duration,
        max_output_bytes: usize,
    ) -> Self {
        Self {
            program,
            workdir,
            timeout,
            max_output_bytes,
        }
    }
}

impl Shell for Bash {
    fn start(
        &self,
        run: ShellRunId,
        command: String,
        updates: mpsc::UnboundedSender<ShellUpdate>,
    ) -> ShellRun {
        let (sender, cancel) = oneshot::channel();
        let bash = self.clone();
        tokio::spawn(async move { execute(bash, run, command, &updates, cancel).await });
        ShellRun::new(sender)
    }
}

/// What is left of a run's output budget.
struct Budget {
    remaining: usize,
    truncated: bool,
}

impl Budget {
    /// The part of `bytes` that still fits, and nothing once the cap is
    /// spent.
    fn take<'a>(&mut self, bytes: &'a [u8]) -> &'a [u8] {
        let fits = bytes.len().min(self.remaining);
        if fits < bytes.len() {
            self.truncated = true;
        }
        self.remaining -= fits;
        &bytes[..fits]
    }
}

/// Runs one command to its end.
async fn execute(
    bash: Bash,
    run: ShellRunId,
    command: String,
    updates: &mpsc::UnboundedSender<ShellUpdate>,
    cancel: oneshot::Receiver<()>,
) {
    let mut child = match spawn(&bash, &command) {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!(%error, program = %bash.program.display(), "a shell command would not start");
            report(
                updates,
                run,
                ShellEvent::Exited {
                    outcome: ShellOutcome::Failed {
                        error: error.to_string(),
                    },
                    truncated: false,
                },
            );
            return;
        }
    };

    let mut budget = Budget {
        remaining: bash.max_output_bytes,
        truncated: false,
    };
    let stopped = stream(&bash, &mut child, run, updates, &mut budget, cancel).await;
    let outcome = match stopped {
        Some(outcome) => {
            if let Err(error) = child.kill().await {
                tracing::warn!(%error, "a stopped shell command could not be killed");
            }
            outcome
        }
        None => match child.wait().await {
            Ok(status) => {
                status
                    .code()
                    .map_or(ShellOutcome::Signalled, |code| ShellOutcome::Exited {
                        code,
                    })
            }
            Err(error) => ShellOutcome::Failed {
                error: error.to_string(),
            },
        },
    };

    report(
        updates,
        run,
        ShellEvent::Exited {
            outcome,
            truncated: budget.truncated,
        },
    );
}

/// Starts `bash -c command` with both pipes captured.
fn spawn(bash: &Bash, command: &str) -> std::io::Result<tokio::process::Child> {
    Command::new(&bash.program)
        .arg("-c")
        .arg(command)
        .current_dir(&bash.workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A daemon that stops mid-run must not leave the command behind on
        // the machine: the handle is dropped when this task ends, whatever
        // ended it.
        .kill_on_drop(true)
        .spawn()
}

/// Streams both pipes until they end, and answers with what stopped them.
///
/// `None` is the ordinary case: both pipes reached end of file and the exit
/// status is the child's to give. `Some` is this daemon deciding the run is
/// over — a timeout, a cancellation, or a relay that stopped listening.
///
/// The pipes are read until end of file rather than until the child exits:
/// a process can be gone with its output still in the pipe, and a
/// transcript missing the last line of a build is a transcript that cannot
/// be trusted. Reading also has to *continue* past the output cap — a child
/// blocked writing into a full pipe would never exit, and the timeout would
/// report as a hang something that was only ever ignored.
async fn stream(
    bash: &Bash,
    child: &mut tokio::process::Child,
    run: ShellRunId,
    updates: &mpsc::UnboundedSender<ShellUpdate>,
    budget: &mut Budget,
    cancel: oneshot::Receiver<()>,
) -> Option<ShellOutcome> {
    let mut stdout = child.stdout.take().expect("stdout was piped");
    let mut stderr = child.stderr.take().expect("stderr was piped");
    let mut out_buf = [0_u8; CHUNK];
    let mut err_buf = [0_u8; CHUNK];
    let (mut out_open, mut err_open) = (true, true);

    let deadline = tokio::time::sleep(bash.timeout);
    tokio::pin!(deadline);
    tokio::pin!(cancel);

    loop {
        if !out_open && !err_open {
            return None;
        }
        tokio::select! {
            read = stdout.read(&mut out_buf), if out_open => {
                match read {
                    Ok(0) => out_open = false,
                    Ok(n) => {
                        if !emit(updates, run, ShellStream::Stdout, budget.take(&out_buf[..n])) {
                            return Some(ShellOutcome::Cancelled);
                        }
                    }
                    Err(error) => {
                        tracing::debug!(%error, "a shell command's stdout ended");
                        out_open = false;
                    }
                }
            }
            read = stderr.read(&mut err_buf), if err_open => {
                match read {
                    Ok(0) => err_open = false,
                    Ok(n) => {
                        if !emit(updates, run, ShellStream::Stderr, budget.take(&err_buf[..n])) {
                            return Some(ShellOutcome::Cancelled);
                        }
                    }
                    Err(error) => {
                        tracing::debug!(%error, "a shell command's stderr ended");
                        err_open = false;
                    }
                }
            }
            () = &mut deadline => {
                tracing::warn!(%run, seconds = bash.timeout.as_secs(), "a shell command ran past its timeout");
                return Some(ShellOutcome::TimedOut { after_seconds: bash.timeout.as_secs() });
            }
            _ = &mut cancel => {
                tracing::info!(%run, "a shell command was cancelled");
                return Some(ShellOutcome::Cancelled);
            }
        }
    }
}

/// Sends one chunk, and says whether anyone is still listening.
///
/// An empty slice is the run's cap already spent: the pipe is still being
/// drained so the child can finish, but nothing more goes to the browser.
fn emit(
    updates: &mpsc::UnboundedSender<ShellUpdate>,
    run: ShellRunId,
    stream: ShellStream,
    bytes: &[u8],
) -> bool {
    if bytes.is_empty() {
        return true;
    }
    updates
        .send(ShellUpdate {
            run,
            event: ShellEvent::Output {
                stream,
                data: String::from_utf8_lossy(bytes).into_owned(),
            },
        })
        .is_ok()
}

/// Files the run's last word, which nobody may be there to hear.
fn report(updates: &mpsc::UnboundedSender<ShellUpdate>, run: ShellRunId, event: ShellEvent) {
    if updates.send(ShellUpdate { run, event }).is_err() {
        tracing::debug!(%run, "a shell command outlived the relay that asked for it");
    }
}

/// One command a [`FakeShell`] was asked to run.
#[derive(Debug)]
pub struct StartedRun {
    /// The run the control plane named.
    pub run: ShellRunId,
    /// What was asked for, without the `!`.
    pub command: String,
    /// Where the test writes the run's output and its exit.
    pub updates: mpsc::UnboundedSender<ShellUpdate>,
    /// Resolves when the daemon cancels this run.
    pub cancelled: oneshot::Receiver<()>,
}

/// A [`Shell`] that starts no processes, for relay tests.
#[derive(Debug)]
pub struct FakeShell {
    started: mpsc::UnboundedSender<StartedRun>,
}

impl FakeShell {
    /// A pair: the shell the relay owns, and the runs it was asked for.
    #[must_use]
    pub fn pair() -> (Self, mpsc::UnboundedReceiver<StartedRun>) {
        let (started, runs) = mpsc::unbounded_channel();
        (Self { started }, runs)
    }
}

impl Shell for FakeShell {
    fn start(
        &self,
        run: ShellRunId,
        command: String,
        updates: mpsc::UnboundedSender<ShellUpdate>,
    ) -> ShellRun {
        let (sender, cancelled) = oneshot::channel();
        self.started
            .send(StartedRun {
                run,
                command,
                updates,
                cancelled,
            })
            .expect("the test is watching the runs it asked for");
        ShellRun::new(sender)
    }
}

#[cfg(test)]
mod tests {
    use super::{Bash, Shell as _, ShellEvent, ShellUpdate};
    use core::time::Duration;
    use flyco_core::{ShellOutcome, ShellRunId, ShellStream};
    use std::path::PathBuf;
    use tokio::sync::mpsc;

    /// A runner over a real `bash`, in a directory that certainly exists.
    fn bash(timeout: Duration, max_output_bytes: usize) -> Bash {
        Bash::new(
            PathBuf::from("bash"),
            std::env::temp_dir(),
            timeout,
            max_output_bytes,
        )
    }

    /// Runs `command` and collects everything it produced.
    async fn run(bash: &Bash, command: &str) -> (String, String, ShellOutcome, bool) {
        let (sender, mut updates) = mpsc::unbounded_channel();
        let _handle = bash.start(ShellRunId::generate(), command.to_owned(), sender);
        collect(&mut updates).await
    }

    async fn collect(
        updates: &mut mpsc::UnboundedReceiver<ShellUpdate>,
    ) -> (String, String, ShellOutcome, bool) {
        let (mut out, mut err) = (String::new(), String::new());
        loop {
            let update = tokio::time::timeout(Duration::from_secs(30), updates.recv())
                .await
                .expect("the run answered")
                .expect("the run did not vanish");
            match update.event {
                ShellEvent::Output { stream, data } => match stream {
                    ShellStream::Stdout => out.push_str(&data),
                    ShellStream::Stderr => err.push_str(&data),
                },
                ShellEvent::Exited { outcome, truncated } => {
                    return (out, err, outcome, truncated);
                }
            }
        }
    }

    #[tokio::test]
    async fn a_command_streams_both_pipes_and_reports_its_status() {
        let (out, err, outcome, truncated) = run(
            &bash(Duration::from_secs(30), 4096),
            "echo out; echo err >&2; exit 3",
        )
        .await;

        assert_eq!(out, "out\n");
        assert_eq!(err, "err\n");
        assert_eq!(outcome, ShellOutcome::Exited { code: 3 });
        assert!(!truncated);
    }

    #[tokio::test]
    async fn a_command_runs_in_the_working_directory_it_was_given() {
        // Not the daemon's own cwd: an agent's `!ls` is about the session's
        // checkout, which is the only directory the user is looking at.
        let workdir = std::env::temp_dir().join("flycod-shell-test");
        tokio::fs::create_dir_all(&workdir)
            .await
            .expect("a writable temporary directory");
        let bash = Bash::new(
            PathBuf::from("bash"),
            workdir.clone(),
            Duration::from_secs(30),
            4096,
        );
        let (out, _, outcome, _) = run(&bash, "pwd").await;

        assert_eq!(outcome, ShellOutcome::Exited { code: 0 });
        assert!(
            out.trim().ends_with("flycod-shell-test"),
            "the command ran in {out}"
        );
    }

    #[tokio::test]
    async fn a_command_that_will_not_end_is_killed_at_its_timeout() {
        let (_, _, outcome, _) = run(&bash(Duration::from_secs(1), 4096), "sleep 60").await;

        assert_eq!(outcome, ShellOutcome::TimedOut { after_seconds: 1 });
    }

    #[tokio::test]
    async fn stop_cancels_a_running_command() {
        let (sender, mut updates) = mpsc::unbounded_channel();
        let handle = bash(Duration::from_secs(60), 4096).start(
            ShellRunId::generate(),
            "sleep 60".to_owned(),
            sender,
        );
        handle.cancel();

        let (_, _, outcome, _) = collect(&mut updates).await;
        assert_eq!(outcome, ShellOutcome::Cancelled);
    }

    #[tokio::test]
    async fn output_past_the_cap_is_dropped_and_said_to_have_been_dropped() {
        let cap = 64;
        let (out, _, outcome, truncated) = run(
            &bash(Duration::from_secs(30), cap),
            "head -c 100000 /dev/zero | tr '\\0' 'a'",
        )
        .await;

        assert_eq!(out.len(), cap, "nothing past the cap reaches the room");
        assert!(truncated, "a truncated run says so");
        assert_eq!(
            outcome,
            ShellOutcome::Exited { code: 0 },
            "draining the rest of the pipe still lets the command finish"
        );
    }

    #[tokio::test]
    async fn a_shell_that_is_not_there_fails_the_run_rather_than_the_daemon() {
        let missing = Bash::new(
            PathBuf::from("flycod-has-no-such-shell"),
            std::env::temp_dir(),
            Duration::from_secs(30),
            4096,
        );
        let (_, _, outcome, _) = run(&missing, "echo hello").await;

        assert!(
            matches!(outcome, ShellOutcome::Failed { .. }),
            "{outcome:?} must name the failure"
        );
    }
}
