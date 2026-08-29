//! Integration coverage that drives the router end to end.
//!
//! These live inside the crate rather than in `tests/` so they can use the
//! `testing` fixtures without exposing them in the public API.

mod contract;
mod mcp;
mod memory;
mod push;
mod relay;
mod responses;
mod room;
mod schema;
mod sessions;
mod skills;
