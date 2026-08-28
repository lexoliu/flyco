//! Flyco control plane.
//!
//! A skyzen application deployed to Cloudflare Workers. Routes live under
//! `/v1`; the same API serves the flyco frontend and external developers.

mod app;

pub use app::router;

#[cfg(target_arch = "wasm32")]
#[skyzen::main]
fn worker() -> skyzen::routing::Router {
    router()
}
