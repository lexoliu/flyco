//! Web push, over the standard stack.
//!
//! Flyco is a PWA first, so a notification is [RFC 8030] push with
//! [RFC 8292] VAPID identifying the application server — no vendor SDK, no
//! proprietary token. The public key route is public because a browser needs
//! it *before* it has anything else, and it is a public key: it identifies
//! the application server to the push service and reveals nothing.
//!
//! [RFC 8030]: https://www.rfc-editor.org/rfc/rfc8030
//! [RFC 8292]: https://www.rfc-editor.org/rfc/rfc8292

use flyco_core::{CurrentUser, PushSubscription, PushSubscriptionView, VapidPublicKey};
use skyzen::Response;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::utils::{Json, State};
use skyzen_services::Db;

use crate::config::ApiConfig;
use crate::problem::Outcome;
use crate::respond::WithStatus;

/// Reports the VAPID public key browsers subscribe against.
#[skyzen::openapi]
async fn vapid_public_key(State(_config): State<ApiConfig>) -> Outcome<Json<VapidPublicKey>> {
    todo!("M5: return the configured VAPID public key, base64url without padding")
}

/// Registers this browser for push notifications.
#[skyzen::openapi]
async fn subscribe_push(
    State(_user): State<CurrentUser>,
    Json(_subscription): Json<PushSubscription>,
    _db: Db,
) -> Outcome<WithStatus<Json<PushSubscriptionView>>> {
    todo!("M5: upsert on the endpoint, so a re-subscribing browser is not notified twice")
}

/// Removes one of the caller's push subscriptions.
#[skyzen::openapi]
async fn unsubscribe_push(
    State(_user): State<CurrentUser>,
    _params: Params,
    _db: Db,
) -> Outcome<Response> {
    todo!("M5: delete the subscription scoped to the caller")
}

/// The user-scoped push routes.
pub fn routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/push/subscriptions".post(subscribe_push),
        "/v1/push/subscriptions/{id}".delete(unsubscribe_push),
    ))
    .into_route_nodes()
}

/// The VAPID public key, which a browser needs before it can subscribe.
pub fn public_routes() -> Vec<RouteNode> {
    Route::new(("/v1/push/vapid-public-key".at(vapid_public_key),)).into_route_nodes()
}
