//! Native entry point: local development (`skyzen dev`) and the
//! `openapi.json` export workflow run the same router off-Worker.

// `#[skyzen::main]` expands `import_config!`, whose generated service-wiring
// impl is `async` without awaiting when every declared service resolves
// synchronously. The lint fires on code this crate does not write.
#![allow(clippy::unused_async_trait_impl)]

#[cfg(not(target_arch = "wasm32"))]
#[skyzen::main]
fn main() -> skyzen::routing::Router {
    flyco_api::router_from_environment()
}

#[cfg(target_arch = "wasm32")]
fn main() {}
