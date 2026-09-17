//! Integration coverage that drives the router end to end.
//!
//! These live inside the crate rather than in `tests/` so they can use the
//! `testing` fixtures without exposing them in the public API.

mod activity;
mod catalog;
mod claude_oauth;
mod cli;
mod codespaces;
mod codex_oauth;
mod contract;
mod devin_oauth;
mod github_oauth;
mod handoffs;
mod harness_accounts;
mod host_room;
mod hosts;
mod mcp;
mod memory;
mod metering;
mod provider_oauth;
mod providers;
mod provisioning;
mod push;
mod relay;
mod request_budget;
mod responses;
mod room;
mod row_budget;
mod schema;
mod sessions;
mod skills;
mod turnstile;
mod usage;
mod usage_limits;
mod user_events;
mod webhooks;
