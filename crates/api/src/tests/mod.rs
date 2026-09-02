//! Integration coverage that drives the router end to end.
//!
//! These live inside the crate rather than in `tests/` so they can use the
//! `testing` fixtures without exposing them in the public API.

mod claude_oauth;
mod contract;
mod harness_accounts;
mod mcp;
mod memory;
mod metering;
mod providers;
mod provisioning;
mod push;
mod relay;
mod responses;
mod room;
mod schema;
mod sessions;
mod skills;
mod usage;
mod webhooks;
