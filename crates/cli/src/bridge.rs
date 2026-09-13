//! The human half of `flyco claude`/`codex`/`resume`: the local terminal,
//! bridged to the harness's native TUI running inside the session's PTY.
//!
//! There is nothing to emulate: the PTY lives on the machine. This module
//! puts the local TTY in raw mode and runs three pumps —
//!
//! * stdin → `POST …/terminal/input`, verbatim bytes (UTF-8 safe),
//! * the session's event stream → stdout for `TerminalOutput` chunks,
//! * `SIGWINCH` → `POST …/terminal/resize` with the new `winsize`.
//!
//! `Ctrl-]` detaches locally: the harness keeps running on the machine and
//! `flyco resume` re-attaches to it. `TerminalExited` — the harness's own
//! exit — ends the bridge and hands its exit code back.

use flyco_core::SessionId;
use flyco_core::wire::ClientEvent;
use rustix::termios::{
    ControlModes, InputModes, LocalModes, OptionalActions, OutputModes, SpecialCodeIndex, Termios,
    tcgetattr, tcgetwinsize, tcsetattr,
};
use tokio::io::AsyncReadExt as _;

use crate::client::Api;
use crate::follow::Follow;
use crate::{Failure, Outcome, out};

/// The detach key: `Ctrl-]` (0x1D).
///
/// Borrowed from `telnet`'s escape and unused by either harness — `claude`
/// and `codex` take `Ctrl-C`, `Esc` and `Ctrl-D`, none of which is this —
/// so pressing it locally never does anything to the remote TUI.
const DETACH: u8 = 0x1D;

/// The read size for the stdin pump. Keystrokes arrive a handful of bytes
/// at a time; a page is already far more than a paste burst holds.
const STDIN_CHUNK: usize = 4096;

/// How the bridge ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The harness TUI exited on its own — the code it died with.
    Exited(Option<i32>),
    /// The user pressed `Ctrl-]`; the session and its TUI keep running.
    Detached,
    /// The local stdin closed; a TUI nobody can type at is detached from.
    InputEnded,
}

/// Runs the bridge until the harness exits or the user detaches.
///
/// Takes the caller's `follow` rather than opening a second stream: the
/// attach wait rode this one, and a fresh stream could miss the TUI's
/// first paint emitted between the harness POST and the reconnect.
///
/// The local terminal is put in raw mode for the duration and restored on
/// every exit path, including error ones, through [`RawMode`]'s drop.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) on transport failure, or when the local
/// terminal cannot be put into raw mode.
pub async fn run(api: &Api, session: SessionId, follow: &mut Follow<'_>) -> Outcome<Ended> {
    let _raw = RawMode::enter()?;
    send_resize(api, session).await?;

    let mut stdin = tokio::io::stdin();
    let mut buf = vec![0u8; STDIN_CHUNK];
    // Held-back tail of an incomplete UTF-8 sequence, prepended to the next
    // chunk so a multi-byte character is never split into replacement
    // characters on the far side.
    let mut pending: Vec<u8> = Vec::new();

    let mut winch =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
            .map_err(|error| Failure::transport(format!("cannot watch SIGWINCH: {error}")))?;

    loop {
        let ended = tokio::select! {
            item = follow.next() => {
                match item?.event() {
                    Some(ClientEvent::TerminalOutput { data }) => {
                        out::raw_out(data.as_bytes())?;
                        None
                    }
                    Some(ClientEvent::TerminalExited { code }) => Some(Ended::Exited(code)),
                    _ => None,
                }
            }
            read = stdin.read(&mut buf) => {
                match read {
                    Ok(0) => Some(Ended::InputEnded),
                    Ok(n) => {
                        let mut chunk = core::mem::take(&mut pending);
                        chunk.extend_from_slice(&buf[..n]);
                        let (send, hold) = split_utf8(&chunk);
                        pending = hold;
                        if bytes_contain(&send, DETACH) {
                            Some(Ended::Detached)
                        } else if send.is_empty() {
                            None
                        } else {
                            send_input(api, session, &send).await?;
                            None
                        }
                    }
                    Err(error) => return Err(Failure::transport(format!("stdin: {error}"))),
                }
            }
            _ = winch.recv() => {
                send_resize(api, session).await?;
                None
            }
        };
        if let Some(ended) = ended {
            return Ok(ended);
        }
    }
}

/// Sends the terminal's current size to the session's PTY.
///
/// Sent once at bridge start (a well-behaved TUI repaints on resize, which
/// is also how output missed before the stream attached gets re-rendered)
/// and then per `SIGWINCH`. `human` sends one before bridging too — a TUI
/// already in the foreground answers a resize with a full screen, which is
/// how an attach to a live-but-quiet session learns it is attached.
pub(crate) async fn send_resize(api: &Api, session: SessionId) -> Outcome<()> {
    let winsize = tcgetwinsize(std::io::stdout())
        .map_err(|error| Failure::transport(format!("cannot read the terminal size: {error}")))?;
    api.post::<flyco_core::TerminalSize, serde_json::Value>(
        &format!("/v1/sessions/{session}/terminal/resize"),
        &flyco_core::TerminalSize {
            cols: winsize.ws_col,
            rows: winsize.ws_row,
        },
    )
    .await?;
    Ok(())
}

/// Forwards one stdin chunk to the remote PTY.
async fn send_input(api: &Api, session: SessionId, bytes: &[u8]) -> Outcome<()> {
    let text = core::str::from_utf8(bytes)
        .expect("split_utf8 holds back only the incomplete tail")
        .to_owned();
    api.post::<flyco_core::TerminalInput, serde_json::Value>(
        &format!("/v1/sessions/{session}/terminal/input"),
        &flyco_core::TerminalInput { data: text },
    )
    .await?;
    Ok(())
}

/// Splits a byte chunk into (the longest valid-UTF-8 prefix, the
/// incomplete tail to hold for next time).
///
/// `TerminalInput.data` is UTF-8; a read boundary inside a multi-byte
/// character would otherwise corrupt it on the far side.
fn split_utf8(bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut send = Vec::new();
    let mut rest = bytes;
    loop {
        match core::str::from_utf8(rest) {
            Ok(_) => {
                send.extend_from_slice(rest);
                return (send, Vec::new());
            }
            // Incomplete sequence at the very end — hold it back.
            Err(error) if error.error_len().is_none() => {
                send.extend_from_slice(&rest[..error.valid_up_to()]);
                return (send, rest[error.valid_up_to()..].to_vec());
            }
            // A genuinely invalid byte, mid-chunk: send what came before
            // and keep going past it, rather than corrupting the stream
            // with a replacement character the TUI would render.
            Err(error) => {
                send.extend_from_slice(&rest[..error.valid_up_to()]);
                rest = &rest[error.valid_up_to() + error.error_len().unwrap_or(0)..];
            }
        }
    }
}

/// `haystack.contains(needle)` for bytes.
fn bytes_contain(haystack: &[u8], needle: u8) -> bool {
    haystack.contains(&needle)
}

/// The local terminal in raw mode, restored on drop.
///
/// `cfmakeraw`, written out: input translation off, output post-processing
/// off, canonical mode and echo off, `ISIG` off so `Ctrl-C` arrives as a
/// byte for the remote TUI rather than a signal for us, `VMIN=1 VTIME=0`.
struct RawMode {
    fd: std::os::fd::BorrowedFd<'static>,
    saved: Termios,
}

impl RawMode {
    fn enter() -> Outcome<Self> {
        use std::os::fd::AsRawFd as _;
        // SAFETY: stdin is open for the process's life; the fd is borrowed
        // for exactly that long.
        let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(std::io::stdin().as_raw_fd()) };
        let saved =
            tcgetattr(fd).map_err(|error| Failure::transport(format!("tcgetattr: {error}")))?;
        let mut raw = saved.clone();
        raw.input_modes -= InputModes::BRKINT
            | InputModes::ICRNL
            | InputModes::INPCK
            | InputModes::ISTRIP
            | InputModes::IXON;
        raw.output_modes -= OutputModes::OPOST;
        raw.control_modes |= ControlModes::CS8;
        raw.local_modes -=
            LocalModes::ECHO | LocalModes::ICANON | LocalModes::IEXTEN | LocalModes::ISIG;
        raw.special_codes[SpecialCodeIndex::VMIN] = 1;
        raw.special_codes[SpecialCodeIndex::VTIME] = 0;
        tcsetattr(fd, OptionalActions::Now, &raw)
            .map_err(|error| Failure::transport(format!("tcsetattr raw: {error}")))?;
        Ok(Self { fd, saved })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = tcsetattr(self.fd, OptionalActions::Now, &self.saved);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_incomplete_tail_is_held_back() {
        // `€` is three bytes; a chunk ending two bytes in must defer them.
        let (send, hold) = split_utf8(b"abc\xE2\x82".as_slice());
        assert_eq!(send, b"abc");
        assert_eq!(hold, b"\xE2\x82".as_slice());

        // Completing it yields the whole character next chunk.
        let mut chunk = hold;
        chunk.extend_from_slice(b"\xAC");
        let (send, hold) = split_utf8(&chunk);
        assert_eq!(send, "€".as_bytes());
        assert!(hold.is_empty());
    }

    #[test]
    fn an_invalid_byte_is_dropped() {
        let (send, hold) = split_utf8(b"a\xFFb".as_slice());
        assert_eq!(send, b"ab");
        assert!(hold.is_empty());
    }
}
