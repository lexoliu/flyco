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
//! # What M3a contains
//!
//! - [`config`] — the daemon's TOML configuration.
//! - [`harness`] — the harness abstraction and the Claude Code driver,
//!   which speaks to a Bun sidecar running the Claude Agent SDK.
//! - [`repl`] — a line-oriented stand-in for the control-plane client,
//!   used to drive and verify a session from a terminal.
//!
//! The control-plane WebSocket client, the web terminal, the local MCP
//! server, and the Codex driver land in later milestones.
//!
//! [`flyco_api`]: https://github.com/lexoliu/flyco/tree/main/crates/api

pub mod config;
pub mod harness;
pub mod repl;
