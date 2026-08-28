//! Exports the control plane's `OpenAPI` document to stdout.
//!
//! Skyzen collects handler metadata through a `linkme` slice that only exists
//! in debug native builds, so this runs as `cargo run --bin openapi`. The
//! checked-in `openapi.json` at the repository root is this program's output;
//! CI regenerates it and fails on a diff, which is what keeps the TypeScript
//! client honest.

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    use std::io::Write as _;

    let spec = flyco_api::openapi_document();

    let mut json = serde_json::to_vec_pretty(&spec).expect("the OpenAPI document serializes");
    json.push(b'\n');

    std::io::stdout()
        .write_all(&json)
        .expect("stdout accepts the OpenAPI document");
}

#[cfg(target_arch = "wasm32")]
fn main() {}
