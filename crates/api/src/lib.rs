//! Flyco control plane.
//!
//! A skyzen application deployed to Cloudflare Workers. Routes live under
//! `/v1`; the same API serves the flyco frontend and external developers.
//!
//! Milestone M2a covers the auth stack: GitHub OAuth sign-in, opaque session
//! tokens in KV, hashed API keys in D1, and the authenticator that turns
//! either credential into a [`flyco_core::CurrentUser`].

pub mod agents_md;
pub mod anthropic;
pub mod api_keys;
pub mod app;
pub mod approvals;
pub mod authenticator;
pub mod bonuses;
pub mod budgets;
pub mod claude_oauth;
pub mod clock;
pub mod codex_oauth;
pub mod config;
pub mod crypto;
pub mod daemon_tokens;
pub mod env;
pub mod error;
pub mod expiring;
pub mod extract;
pub mod github;
pub mod harness_accounts;
pub mod machines;
pub mod mcp;
pub mod memory;
pub mod metering;
pub mod middleware;
pub mod oauth;
pub mod observations;
pub mod openai;
pub mod problem;
pub mod provider_accounts;
pub mod provisioning;
pub mod provisioning_queue;
pub mod push;
pub mod relay;
pub mod releases;
pub mod repos;
pub mod respond;
pub mod responses;
pub mod room;
pub mod rooms;
pub mod session;
pub mod sessions;
pub mod skills;
pub mod transcripts;
pub mod turns;
pub mod users;
pub mod vendors;
pub mod webhooks;
pub mod workdirs;

#[cfg(test)]
mod testing;

#[cfg(test)]
mod tests;

pub use app::{openapi_document, router, router_from_environment};
pub use config::ApiConfig;
pub use error::ApiError;

#[cfg(target_arch = "wasm32")]
#[skyzen::main]
fn worker() -> skyzen::routing::Router {
    router_from_environment()
}
