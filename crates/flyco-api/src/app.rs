//! Router assembly and handlers.

use serde::Serialize;
use skyzen::routing::{CreateRouteNode, Route, Router};
use skyzen::utils::Json;

/// Health probe response.
#[derive(Debug, Serialize)]
struct Health {
    /// Wire protocol version this control plane speaks to daemons.
    wire_protocol_version: u32,
}

async fn healthz() -> Json<Health> {
    Json(Health {
        wire_protocol_version: flyco_core::WIRE_PROTOCOL_VERSION,
    })
}

/// Builds the full control-plane router.
#[must_use]
pub fn router() -> Router {
    Route::new(("/v1/healthz".at(healthz),)).build()
}

#[cfg(test)]
mod tests {
    use skyzen_test::TestContext;

    #[skyzen::test]
    async fn healthz_reports_protocol_version(ctx: TestContext) {
        let client = ctx.client(super::router());
        let response = client.get("/v1/healthz").send().await;
        response.assert_status(200);
        let body: serde_json::Value = response.json();
        assert_eq!(
            body["wire_protocol_version"],
            u64::from(flyco_core::WIRE_PROTOCOL_VERSION)
        );
    }
}
