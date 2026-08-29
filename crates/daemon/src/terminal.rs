//! A PTY-backed interactive shell for the web terminal.
//!
//! Flycod owns one child on the session VM. Browser keystrokes arrive as
//! [`flyco_core::ControlToDaemon::TerminalInput`]; bytes the shell writes
//! leave as [`flyco_core::DaemonToControl::TerminalOutput`]. The reader
//! lives on its own thread because the PTY master is a blocking fd.

use std::io::{Read as _, Write};
use std::path::Path;
use std::thread;

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem as _};
use tokio::sync::mpsc;

/// How many output chunks may wait for a socket that is not there.
const OUTPUT_DEPTH: usize = 256;

/// The control handle of a live web terminal.
pub trait TerminalSession: Send {
    /// Writes one chunk of keystrokes.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalError`] if the terminal has stopped.
    fn write(&mut self, data: &str) -> Result<(), TerminalError>;

    /// Stops the shell and closes the PTY.
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

/// A live PTY and the stream of bytes it produces.
pub struct Terminal {
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

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
    ) -> Result<(Self, mpsc::Receiver<String>), TerminalError> {
        let system = NativePtySystem::default();
        let pair = system
            .openpty(PtySize {
                rows: 32,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| TerminalError::Pty(error.to_string()))?;

        let mut command = CommandBuilder::new(shell);
        command.cwd(workdir);
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| TerminalError::Pty(error.to_string()))?;

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| TerminalError::Pty(error.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| TerminalError::Pty(error.to_string()))?;

        let (outputs_tx, outputs) = mpsc::channel(OUTPUT_DEPTH);
        thread::Builder::new()
            .name("flycod-terminal".to_owned())
            .spawn(move || {
                let mut buf = [0_u8; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let chunk = String::from_utf8_lossy(&buf[..n]).into_owned();
                            if outputs_tx.blocking_send(chunk).is_err() {
                                break;
                            }
                        }
                    }
                }
            })
            .map_err(|error| TerminalError::Pty(error.to_string()))?;

        Ok((Self { writer, child }, outputs))
    }
}

impl TerminalSession for Terminal {
    fn write(&mut self, data: &str) -> Result<(), TerminalError> {
        self.writer
            .write_all(data.as_bytes())
            .map_err(TerminalError::Write)?;
        self.writer.flush().map_err(TerminalError::Write)
    }

    fn shutdown(&mut self) -> Result<(), TerminalError> {
        self.child
            .kill()
            .map_err(|error| TerminalError::Pty(error.to_string()))?;
        let _ = self.child.wait();
        Ok(())
    }
}

/// A stand-in terminal for relay tests.
pub struct FakeTerminal {
    writes: mpsc::UnboundedSender<String>,
}

impl core::fmt::Debug for FakeTerminal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FakeTerminal").finish_non_exhaustive()
    }
}

impl FakeTerminal {
    /// A pair: the handle the relay owns, and the stream of writes it made.
    #[must_use]
    pub fn pair() -> (
        Self,
        mpsc::UnboundedReceiver<String>,
        mpsc::Sender<String>,
        mpsc::Receiver<String>,
    ) {
        let (writes, received) = mpsc::unbounded_channel();
        let (output_tx, outputs) = mpsc::channel(OUTPUT_DEPTH);
        (Self { writes }, received, output_tx, outputs)
    }
}

impl TerminalSession for FakeTerminal {
    fn write(&mut self, data: &str) -> Result<(), TerminalError> {
        self.writes
            .send(data.to_owned())
            .map_err(|_| TerminalError::Stopped)
    }

    fn shutdown(&mut self) -> Result<(), TerminalError> {
        Ok(())
    }
}
