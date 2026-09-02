//! `flycod` — flyco's execution-plane daemon.
//!
//! One instance runs on every session VM. It supervises the coding harness,
//! normalizes its stream to [`flyco_core::HarnessEvent`], routes tool
//! approvals to flyco's own UI rather than to the model, and owns the
//! session transcript so a session can resume onto a different machine.
//!
//! The control plane ([`flyco_api`]) is a Cloudflare Worker on wasm32: no
//! processes, no sockets it can hold open, no tokio. Everything
//! process-shaped therefore lives here, and the two halves agree only on
//! [`flyco_core`].
//!
//! # What is here
//!
//! - [`config`] — the daemon's TOML configuration.
//! - [`harness`] — the harness abstraction, the Claude Code driver
//!   (Bun sidecar / Agent SDK), and the Codex driver (`codex app-server`).
//! - [`control`] — the control-plane connection: the relay WebSocket, the
//!   REST client behind it, and the transcript store that makes a session
//!   resumable onto any machine.
//! - [`repl`] — a line-oriented stand-in for the control plane, used to
//!   drive and verify a session from a terminal.
//! - [`terminal`] — a PTY-backed `fish` shell relayed to the browser.
//! - [`git`] — `git status --short` polling, dirty-tree keep-awake, and
//!   workdir snapshots for automatic archive.
//! - [`mcp`] — the local stdio MCP server, which is the *only* sanctioned
//!   way an agent acts on its own session: what machine it is on, what the
//!   budget has left, and moving to another machine.
//! - [`mount`] — telling the harness to run that server and only the
//!   servers flyco named, through root-owned configuration the agent's user
//!   cannot write, and refusing a session whose harness came up without it.
//! - [`spot`] — the provider's eviction notice, watched on the machine's own
//!   instance-metadata endpoint, and the disk flush the daemon answers it
//!   with. The agent takes no part in a reclamation and is told about it
//!   afterwards.
//! - [`notice`] — every sentence flyco says to the agent, compiled from a
//!   template rather than assembled from strings.
//!
//! `flycod run` picks between [`control`] and [`repl`] on whether the
//! configuration names a control plane, and logs which it chose.
//! `flycod mcp` is a second process, launched by the harness rather than by
//! the machine, speaking MCP over its own stdio.
//!
//! [`flyco_api`]: https://github.com/lexoliu/flyco/tree/main/crates/api

pub mod config;
pub mod control;
pub mod git;
pub mod harness;
pub mod mcp;
pub mod mount;
pub mod notice;
pub mod repl;
pub mod spot;
pub mod terminal;

#[cfg(test)]
mod testing;
