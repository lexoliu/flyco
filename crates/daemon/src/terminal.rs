//! A PTY-backed interactive terminal for the session.
//!
//! Flycod owns the pane on the session VM. Its foreground starts as the
//! configured shell; a
//! [`TerminalHarness`](flyco_core::ControlToDaemon::TerminalHarness)
//! command replaces it with the session's harness TUI, and when that
//! process exits the shell comes back — the pane always has a foreground.
//! Browser keystrokes arrive as
//! [`TerminalInput`](flyco_core::ControlToDaemon::TerminalInput); bytes the
//! child writes leave as
//! [`TerminalOutput`](flyco_core::DaemonToControl::TerminalOutput), and a
//! harness's natural exit leaves as
//! [`TerminalExited`](flyco_core::DaemonToControl::TerminalExited).
//!
//! Reading and lifecycle share one supervisor thread, and every
//! foreground gets a *fresh* PTY pair. Reusing a slave across sessions is
//! not portable: a BSD master reports `EIO` once the child's session ends
//! even while the slave fd stays open, and a revoked slave cannot become
//! another session's controlling terminal. Keystrokes and resizes are
//! therefore sent down the control channel too — the supervisor owns
//! whichever master is current, so a byte written mid-replacement lands
//! on the new foreground rather than a dying one.
//!
//! The same ordering gives [`TerminalEvent::Exited`] its guarantee: it is
//! sent only after the last byte the harness wrote has been read.

use std::io::{Read, Write};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{
    Child, CommandBuilder, ExitStatus, MasterPty, NativePtySystem, PtySize, PtySystem as _,
};
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::fd::{AsFd, BorrowedFd};
use rustix::io::fcntl_dupfd_cloexec;
use tokio::sync::mpsc;

/// How many output chunks may wait for a room that is not there.
const OUTPUT_DEPTH: usize = 256;

/// How often the supervisor wakes to check the foreground child and the
/// control channel when no output is arriving.
///
/// Output itself is read the moment `poll` reports it — this bounds only
/// the idle waits: how long a control message or a process exit can go
/// unnoticed.
const IDLE_POLL: Duration = Duration::from_millis(25);

/// How long the supervisor waits for a dying child's last bytes.
///
/// When `try_wait` reports an exit the kernel may still hold output the
/// child wrote; this is the drain deadline after which whatever remains is
/// abandoned. A hundred milliseconds is far longer than the flush takes in
/// practice and short enough that a harness's goodbye reaches the screen
/// before its exit does.
const DRAIN: Duration = Duration::from_millis(100);

/// How long a killed child may take to be reapable before the supervisor
/// stops waiting on it.
///
/// `wait` is never called: a session leader whose session still has
/// living members can sit in the kernel's exiting state far longer than
/// a signal takes to land, and a blocking reap would freeze the pane.
/// Children that outstay the deadline move to the reap-pending list,
/// which the loop keeps polling.
const REAP: Duration = Duration::from_secs(1);

/// How often a killed child is polled for reaping during the deadline.
const REAP_POLL: Duration = Duration::from_millis(10);

/// What the terminal's foreground produced.
#[derive(Debug)]
pub enum TerminalEvent {
    /// Bytes the foreground process wrote, UTF-8 lossy.
    Output(String),
    /// The harness exited on its own.
    ///
    /// Sent only for a harness's natural exit — the shell is ambient and
    /// its exits just respawn it, and a child killed to make room for
    /// the harness says nothing: the pane did not *lose* its foreground,
    /// it was given another.
    Exited {
        /// The exit code; `None` when a signal ended the process.
        code: Option<i32>,
    },
}

/// The control handle of a live web terminal.
pub trait TerminalSession: Send {
    /// Writes one chunk of keystrokes.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalError`] if the terminal has stopped.
    fn write(&mut self, data: &str) -> Result<(), TerminalError>;

    /// Tells the PTY how many columns and rows the browser's pane shows.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalError`] if the terminal has stopped.
    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), TerminalError>;

    /// Puts `command` — the session's harness TUI — in the foreground,
    /// replacing the shell.
    ///
    /// An *ensure*: called while a harness already runs in the foreground,
    /// it does nothing, so re-attaching to a session cannot kill the turn
    /// on its screen. When the harness exits the shell returns.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalError`] if the terminal has stopped.
    fn launch_harness(&mut self, command: CommandBuilder) -> Result<(), TerminalError>;

    /// Writes a line into the terminal's output stream, as if the
    /// foreground had printed it.
    ///
    /// How a launch failure reaches the user: the pane and a bridged CLI
    /// both render output, so "could not start claude" belongs there, not
    /// in a log only an operator reads.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalError`] if the terminal has stopped.
    fn print(&mut self, text: &str) -> Result<(), TerminalError>;

    /// Stops the foreground child and closes the PTY.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalError`] if the child could not be stopped.
    fn shutdown(&mut self) -> Result<(), TerminalError>;
}

/// The PTY could not be used.
#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    /// Opening or spawning the PTY failed.
    #[error("could not start the web terminal: {0}")]
    Pty(String),
    /// Writing keystrokes to the PTY failed.
    #[error("could not write to the web terminal")]
    Write(#[source] std::io::Error),
    /// The terminal has already been shut down.
    #[error("the web terminal has stopped")]
    Stopped,
}

/// What the supervisor's control channel carries.
enum Supervision {
    /// Keystrokes for the current foreground.
    Input(Vec<u8>),
    /// A new pane size; remembered for foregrounds spawned later.
    Resize(PtySize),
    /// Replace the foreground with the session's harness TUI.
    Harness(CommandBuilder),
    /// Kill the foreground child and end the supervisor.
    Stop,
}

/// A live PTY and the thread that supervises its foreground.
pub struct Terminal {
    /// Reports the line a failed launch prints — shares the supervisor's
    /// channel so the line lands in order with real output.
    events: mpsc::Sender<TerminalEvent>,
    control: std::sync::mpsc::SyncSender<Supervision>,
    /// Joined on shutdown, so the child's reap is done before the PTY
    /// goes away.
    thread: Option<thread::JoinHandle<()>>,
}

/// The size a PTY opens at, until the browser says what its pane shows.
///
/// Also what the shell sees if no browser ever opens the pane, so it is a
/// size a shell is comfortable at rather than a placeholder.
const INITIAL_SIZE: PtySize = PtySize {
    rows: 32,
    cols: 120,
    pixel_width: 0,
    pixel_height: 0,
};

/// What the foreground child is told it is running in.
///
/// Without `TERM` fish opens with a warning and falls back to plain
/// `xterm`; xterm.js in the browser renders 256 colours and truecolour,
/// so the shell — and a harness TUI — is told so.
const TERM: &str = "xterm-256color";
const COLORTERM: &str = "truecolor";

impl core::fmt::Debug for Terminal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Terminal").finish_non_exhaustive()
    }
}

impl Terminal {
    /// Spawns `shell` in `workdir` on a new PTY.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalError::Pty`] if the PTY cannot be opened or the
    /// shell cannot be spawned.
    pub fn spawn(
        shell: &Path,
        workdir: &Path,
    ) -> Result<(Self, mpsc::Receiver<TerminalEvent>), TerminalError> {
        let shell = foreground_command(shell, workdir);
        let pane = Pane::open(&shell, INITIAL_SIZE)?;

        let (events, terminal_out) = mpsc::channel(OUTPUT_DEPTH);
        let (control, supervised) = std::sync::mpsc::sync_channel(64);
        let supervised_events = events.clone();
        let thread = thread::Builder::new()
            .name("flycod-terminal".to_owned())
            .spawn(move || supervise(pane, &shell, &supervised_events, &supervised))
            .map_err(|error| TerminalError::Pty(error.to_string()))?;

        Ok((
            Self {
                events,
                control,
                thread: Some(thread),
            },
            terminal_out,
        ))
    }
}

impl TerminalSession for Terminal {
    fn write(&mut self, data: &str) -> Result<(), TerminalError> {
        self.control
            .send(Supervision::Input(data.as_bytes().to_vec()))
            .map_err(|_| TerminalError::Stopped)
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), TerminalError> {
        self.control
            .send(Supervision::Resize(PtySize {
                rows,
                cols,
                ..INITIAL_SIZE
            }))
            .map_err(|_| TerminalError::Stopped)
    }

    fn launch_harness(&mut self, command: CommandBuilder) -> Result<(), TerminalError> {
        self.control
            .send(Supervision::Harness(command))
            .map_err(|_| TerminalError::Stopped)
    }

    fn print(&mut self, text: &str) -> Result<(), TerminalError> {
        self.events
            .blocking_send(TerminalEvent::Output(text.to_owned()))
            .map_err(|_| TerminalError::Stopped)
    }

    fn shutdown(&mut self) -> Result<(), TerminalError> {
        if let Some(thread) = self.thread.take() {
            // A dead supervisor closed the channel; the join still runs so
            // the thread is reaped either way.
            let _ = self.control.send(Supervision::Stop);
            thread
                .join()
                .map_err(|_| TerminalError::Pty("the supervisor thread panicked".to_owned()))?;
        }
        Ok(())
    }
}

/// The command a PTY foreground runs, with the environment every child
/// shares.
fn foreground_command(program: &Path, workdir: &Path) -> CommandBuilder {
    let mut command = CommandBuilder::new(program);
    command.cwd(workdir);
    command.env("TERM", TERM);
    command.env("COLORTERM", COLORTERM);
    command
}

/// One foreground: a PTY pair, the child living on its slave, and a dup
/// of the master the supervisor polls.
///
/// The slave is dropped the moment the child has its stdio — the pair's
/// only long-lived half is the master, so the child's session owns the
/// terminal outright and its death is visible as a quiet master.
struct Pane {
    /// Resizes; also the fd `poll` is a dup of.
    master: Box<dyn MasterPty + Send>,
    /// Keystrokes go here.
    writer: Box<dyn Write + Send>,
    /// Output is read here.
    reader: Box<dyn Read + Send>,
    /// A pollable dup of the master's fd.
    poll: rustix::fd::OwnedFd,
    child: Box<dyn Child + Send + Sync>,
}

impl Pane {
    /// Opens a PTY at `size` and spawns `command` on it.
    fn open(command: &CommandBuilder, size: PtySize) -> Result<Self, TerminalError> {
        let pair = NativePtySystem::default()
            .openpty(size)
            .map_err(|error| TerminalError::Pty(error.to_string()))?;
        let child = pair
            .slave
            .spawn_command(command.clone())
            .map_err(|error| TerminalError::Pty(error.to_string()))?;
        // The child holds the slave as its stdio and controlling tty;
        // our copy would only delay the master's hangup.
        drop(pair.slave);
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| TerminalError::Pty(error.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| TerminalError::Pty(error.to_string()))?;
        // The supervisor polls its own dup of the master: readable means
        // the shared file description has bytes, whoever holds the fd.
        // SAFETY: `master` owns the fd and outlives the dup call.
        let poll = pair
            .master
            .as_raw_fd()
            .map(|raw| unsafe { BorrowedFd::borrow_raw(raw) })
            .and_then(|borrowed| fcntl_dupfd_cloexec(borrowed, 0).ok())
            .ok_or_else(|| TerminalError::Pty("the PTY master has no fd to poll".to_owned()))?;
        Ok(Self {
            master: pair.master,
            writer,
            reader,
            poll,
            child,
        })
    }
}

/// The `poll` timeout as a kernel `timespec`.
fn timeout(duration: Duration) -> Timespec {
    Timespec {
        tv_sec: i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: i64::from(duration.subsec_nanos()),
    }
}

/// Whether the master is readable within `within`.
fn readable<Fd: AsFd>(fd: &Fd, within: &Timespec) -> bool {
    let mut fds = [PollFd::new(fd, PollFlags::IN)];
    matches!(rustix::event::poll(&mut fds, Some(within)), Ok(n) if n > 0)
}

/// What one read of the master found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reading {
    /// Bytes were read and passed on.
    Chunk,
    /// Nothing there. `Ok(0)` means the child's session ended and no
    /// slave remains open — a genuine EOF — but it is still not a
    /// reason for the supervisor to die: exits are detected by
    /// `try_wait`, and the next foreground gets a fresh master anyway.
    /// Non-`Ok(0)` errors land here too — the poll loop paces retries
    /// and the control channel still ends the thread.
    Quiet,
    /// The output channel is gone; nobody will read again.
    Gone,
}

/// Reads one chunk into `events`.
fn read_out(
    reader: &mut dyn Read,
    buf: &mut [u8],
    events: &mpsc::Sender<TerminalEvent>,
) -> Reading {
    match reader.read(buf) {
        Ok(0) | Err(_) => Reading::Quiet,
        Ok(n) => {
            let chunk = String::from_utf8_lossy(&buf[..n]).into_owned();
            if events.blocking_send(TerminalEvent::Output(chunk)).is_ok() {
                Reading::Chunk
            } else {
                Reading::Gone
            }
        }
    }
}

/// What the foreground is, for the supervisor's two policies: a second
/// harness launch is dropped rather than restarting a live TUI, and only
/// a harness's exit is reported — the shell's just respawns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Foreground {
    /// The configured shell — what the pane starts on and returns to.
    Shell,
    /// The session's harness TUI.
    Harness,
}

/// Kills `child` and gives it [`REAP`] to die; `false` if it is still not
/// reapable and must join the pending list instead.
fn reaped(child: &mut dyn Child) -> bool {
    let _ = child.kill();
    let deadline = Instant::now() + REAP;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return true,
            Ok(None) => thread::sleep(REAP_POLL),
        }
    }
    false
}

/// Opens the harness pane `command` asks for, or prints the failure into
/// the terminal and opens the shell back. `None` when even the shell
/// refused to start — the pane is dead either way.
fn foreground_pane(
    command: &CommandBuilder,
    shell: &CommandBuilder,
    size: PtySize,
    events: &mpsc::Sender<TerminalEvent>,
) -> Option<(Pane, Foreground)> {
    match Pane::open(command, size) {
        Ok(pane) => Some((pane, Foreground::Harness)),
        Err(error) => {
            let _ = events.blocking_send(TerminalEvent::Output(format!(
                "\r\nflycod: could not start the harness: {error}\r\n"
            )));
            Pane::open(shell, size)
                .ok()
                .map(|pane| (pane, Foreground::Shell))
        }
    }
}

/// The exit status as the wire reports it: `None` when a signal ended the
/// process, its code otherwise.
fn exit_code(status: &ExitStatus) -> Option<i32> {
    if status.signal().is_some() {
        None
    } else {
        Some(i32::try_from(status.exit_code()).unwrap_or(-1))
    }
}

/// Reads the pane and watches its foreground child, one thread for both.
///
/// A blocking `read` could never report an exit — the pane's lifecycle
/// is `try_wait` — and two threads racing the same channel could let
/// `Exited` overtake the child's last bytes. One thread that polls is
/// the shape that has neither problem.
fn supervise(
    mut pane: Pane,
    shell: &CommandBuilder,
    events: &mpsc::Sender<TerminalEvent>,
    control: &std::sync::mpsc::Receiver<Supervision>,
) {
    let mut foreground = Foreground::Shell;
    let mut size = INITIAL_SIZE;
    // Killed children that outlived their reap deadline; still polled so
    // the process table stays clean.
    let mut pending: Vec<Box<dyn Child + Send + Sync>> = Vec::new();
    let mut buf = [0_u8; 4096];
    let idle = timeout(IDLE_POLL);
    let drain = timeout(DRAIN);

    loop {
        if readable(&pane.poll, &idle) {
            match read_out(&mut *pane.reader, &mut buf, events) {
                Reading::Chunk => {}
                Reading::Quiet => {
                    // A dead session's master can stay pollable (HUP
                    // counts as an event) with nothing to read; without
                    // a pause that state is a busy loop.
                    thread::sleep(IDLE_POLL);
                }
                Reading::Gone => return,
            }
        }

        let mut stop = false;
        for message in control.try_iter() {
            match message {
                Supervision::Input(bytes) => {
                    if let Err(error) = pane.writer.write_all(&bytes) {
                        tracing::warn!(%error, "terminal input dropped: the pane is gone");
                    }
                }
                Supervision::Resize(next) => {
                    size = next;
                    if let Err(error) = pane.master.resize(next) {
                        tracing::warn!(%error, "terminal resize refused by the pane");
                    }
                }
                Supervision::Harness(command) => {
                    if foreground == Foreground::Harness {
                        continue;
                    }
                    // The pane is dead with its child: destructuring drops
                    // the master and writer along with it rather than
                    // leaving a revoked pair half-held.
                    let Pane { mut child, .. } = pane;
                    if !reaped(&mut *child) {
                        pending.push(child);
                    }
                    match foreground_pane(&command, shell, size, events) {
                        Some((next, became)) => {
                            pane = next;
                            foreground = became;
                        }
                        None => return,
                    }
                }
                Supervision::Stop => stop = true,
            }
        }
        if stop {
            let _ = pane.child.kill();
            return;
        }

        pending.retain_mut(|child| child.try_wait().ok().flatten().is_none());

        // A wait that fails is a child we can no longer see — reported as
        // a lost foreground rather than silently ignored.
        let exited = match pane.child.try_wait() {
            Ok(Some(status)) => Some(exit_code(&status)),
            Err(_) => Some(None),
            Ok(None) => None,
        };
        if let Some(code) = exited {
            // Drain what the child left before saying so: `Exited` must
            // not overtake the last bytes it wrote. A quiet read — the
            // session ending — ends the drain as surely as the deadline.
            while readable(&pane.poll, &drain)
                && read_out(&mut *pane.reader, &mut buf, events) == Reading::Chunk
            {}
            if foreground == Foreground::Harness
                && events
                    .blocking_send(TerminalEvent::Exited { code })
                    .is_err()
            {
                return;
            }
            match Pane::open(shell, size) {
                Ok(next) => {
                    pane = next;
                    foreground = Foreground::Shell;
                }
                Err(error) => {
                    tracing::error!(%error, "the terminal's shell could not be restarted");
                    return;
                }
            }
        }
    }
}

/// What the relay asked a [`FakeTerminal`] to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalCall {
    /// Keystrokes written to the foreground.
    Write(String),
    /// The pane's size, columns then rows.
    Resize(u16, u16),
    /// The harness TUI was asked into the foreground.
    LaunchHarness,
    /// A line was printed into the output stream.
    Print(String),
}

/// A stand-in terminal for relay tests.
pub struct FakeTerminal {
    calls: mpsc::UnboundedSender<TerminalCall>,
}

impl core::fmt::Debug for FakeTerminal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FakeTerminal").finish_non_exhaustive()
    }
}

impl FakeTerminal {
    /// A pair: the handle the relay owns, and the stream of calls it made.
    #[must_use]
    pub fn pair() -> (
        Self,
        mpsc::UnboundedReceiver<TerminalCall>,
        mpsc::Sender<TerminalEvent>,
        mpsc::Receiver<TerminalEvent>,
    ) {
        let (calls, received) = mpsc::unbounded_channel();
        let (output_tx, outputs) = mpsc::channel(OUTPUT_DEPTH);
        (Self { calls }, received, output_tx, outputs)
    }
}

impl TerminalSession for FakeTerminal {
    fn write(&mut self, data: &str) -> Result<(), TerminalError> {
        self.calls
            .send(TerminalCall::Write(data.to_owned()))
            .map_err(|_| TerminalError::Stopped)
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), TerminalError> {
        self.calls
            .send(TerminalCall::Resize(cols, rows))
            .map_err(|_| TerminalError::Stopped)
    }

    fn launch_harness(&mut self, _command: CommandBuilder) -> Result<(), TerminalError> {
        self.calls
            .send(TerminalCall::LaunchHarness)
            .map_err(|_| TerminalError::Stopped)
    }

    fn print(&mut self, text: &str) -> Result<(), TerminalError> {
        self.calls
            .send(TerminalCall::Print(text.to_owned()))
            .map_err(|_| TerminalError::Stopped)
    }

    fn shutdown(&mut self) -> Result<(), TerminalError> {
        Ok(())
    }
}
