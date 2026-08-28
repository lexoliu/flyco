//! Native entry point: local development (`skyzen dev`) and the
//! `openapi.json` export workflow run the same router off-Worker.

#[cfg(not(target_arch = "wasm32"))]
#[skyzen::main]
async fn main() -> skyzen::routing::Router {
    flyco_api::router_from_environment().await
}

#[cfg(target_arch = "wasm32")]
fn main() {}
