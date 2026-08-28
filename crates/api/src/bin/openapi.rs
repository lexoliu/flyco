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

    let document = flyco_api::openapi_document();
    assert!(
        document.is_enabled(),
        "OpenAPI collection is compiled out; build this binary in debug on a native target"
    );

    let mut spec = document.to_utoipa_spec();
    // Skyzen stamps its own crate name and version into `info`; the document
    // describes flyco's API, whose version is the `/v1` prefix. Using the
    // crate version instead would churn the checked-in file on every release.
    spec.info = utoipa::openapi::Info::new("Flyco control plane", "v1");

    let mut json = serde_json::to_vec_pretty(&spec).expect("the OpenAPI document serializes");
    json.push(b'\n');

    std::io::stdout()
        .write_all(&json)
        .expect("stdout accepts the OpenAPI document");
}

#[cfg(target_arch = "wasm32")]
fn main() {}
