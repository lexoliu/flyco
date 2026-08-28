//! Flyco control plane.
//!
//! A skyzen application deployed to Cloudflare Workers. Routes live under
//! `/v1`; the same API serves the flyco frontend and external developers.
//!
//! Milestone M2a covers the auth stack: GitHub OAuth sign-in, opaque session
//! tokens in KV, hashed API keys in D1, and the authenticator that turns
//! either credential into a [`flyco_core::CurrentUser`].

pub mod api_keys;
pub mod app;
pub mod authenticator;
pub mod clock;
pub mod config;
pub mod crypto;
pub mod database;
pub mod error;
pub mod expiring;
pub mod github;
pub mod oauth;
pub mod session;
pub mod users;

#[cfg(test)]
mod testing;

pub use app::{router, router_from_environment};
pub use config::ApiConfig;
pub use error::ApiError;

#[cfg(target_arch = "wasm32")]
#[skyzen::main]
async fn worker() -> skyzen::routing::Router {
    router_from_environment().await
}
