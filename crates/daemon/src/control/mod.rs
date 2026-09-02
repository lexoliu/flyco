//! The daemon's half of the control-plane connection.
//!
//! Two channels, for two kinds of fact:
//!
//! * [`wire`] — the live relay. One WebSocket to the session's Durable
//!   Object, carrying the harness stream out and user commands in. Frames
//!   are ephemeral: a browser that missed one catches up from the room's
//!   stored tail, not from a replay this daemon keeps.
//! * [`rest`] — everything that must outlive a socket. A pending approval,
//!   a transcript batch, the transcript a resuming session reads back.
//!
//! [`store::RemoteTranscriptStore`] sits on top of [`rest`] and is what
//! makes History work: the transcript is in the control plane rather than on
//! a disk a spot eviction can take away.

pub mod rest;
pub mod store;
pub mod wire;

#[cfg(test)]
mod tests;

pub use rest::{
    AgentApi, ApprovalRaiser, ControlApi, ControlApiError, HttpControlApi, TranscriptRead,
};
pub use store::RemoteTranscriptStore;
pub use wire::{Endpoint, SessionRelay, WireError};
