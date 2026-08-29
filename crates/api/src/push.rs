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

use flyco_core::{
    CurrentUser, PushSubscription, PushSubscriptionId, PushSubscriptionView, UserId, VapidPublicKey,
};
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::sql;
use skyzen::utils::{Json, State};
use skyzen_services::Db;

use crate::clock::now_unix;
use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::extract::path_id;
use crate::problem::Outcome;
use crate::respond::{Created, NoContent};

/// The columns a subscription is read back through.
///
/// `p256dh` and `auth` are never selected: they are write-only material for
/// encrypting a message body, and nothing that answers a browser needs them.
#[derive(Debug, skyzen::FromRow)]
struct SubscriptionRow {
    id: PushSubscriptionId,
    endpoint: String,
    created_at_unix: u64,
}

impl From<SubscriptionRow> for PushSubscriptionView {
    fn from(row: SubscriptionRow) -> Self {
        Self {
            id: row.id,
            endpoint: row.endpoint,
            created_at_unix: row.created_at_unix,
        }
    }
}

/// Reports the VAPID public key browsers subscribe against.
#[skyzen::openapi]
async fn vapid_public_key(State(config): State<ApiConfig>) -> Outcome<Json<VapidPublicKey>> {
    config
        .vapid_public_key()
        .map(|key| {
            Json(VapidPublicKey {
                key: key.to_owned(),
            })
        })
        .ok_or(ApiError::PushUnconfigured)
        .into()
}

/// Registers this browser for push notifications.
#[skyzen::openapi]
async fn subscribe_push(
    State(user): State<CurrentUser>,
    Json(subscription): Json<PushSubscription>,
    db: Db,
) -> Outcome<Created<Json<PushSubscriptionView>>> {
    subscribe(&db, user.id, subscription)
        .await
        .map(|view| Created(Json(view)))
        .into()
}

/// Registers a browser, or re-registers one that already exists.
///
/// The endpoint is the identity a push service knows a browser by, so a
/// browser that re-subscribes — after a service worker update, or a key
/// rotation — must land on the same row. Inserting a second one would send
/// every notification twice.
async fn subscribe(
    db: &Db,
    user: UserId,
    subscription: PushSubscription,
) -> Result<PushSubscriptionView, ApiError> {
    if subscription.endpoint.trim().is_empty() {
        return Err(ApiError::InvalidPushSubscription(
            "the endpoint is required",
        ));
    }

    let row: SubscriptionRow = sql!(
        db,
        "INSERT INTO push_subscriptions \
         (id, user_id, endpoint, p256dh, auth, expiration_time_ms, created_at_unix) \
         VALUES ({PushSubscriptionId::generate()}, {user}, {subscription.endpoint}, \
                 {subscription.keys.p256dh}, {subscription.keys.auth}, \
                 {subscription.expiration_time}, {now_unix()}) \
         ON CONFLICT (endpoint) DO UPDATE SET \
         user_id = excluded.user_id, p256dh = excluded.p256dh, auth = excluded.auth, \
         expiration_time_ms = excluded.expiration_time_ms \
         RETURNING id, endpoint, created_at_unix"
    )
    .fetch_one()
    .await?;

    Ok(row.into())
}

/// Removes one of the caller's push subscriptions.
#[skyzen::openapi]
async fn unsubscribe_push(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Outcome<NoContent> {
    unsubscribe(&db, user.id, &params).await.into()
}

async fn unsubscribe(db: &Db, user: UserId, params: &Params) -> Result<NoContent, ApiError> {
    let id: PushSubscriptionId = path_id(params, "id")?;

    let removed = sql!(
        db,
        "DELETE FROM push_subscriptions WHERE id = {id} AND user_id = {user}"
    )
    .execute()
    .await?;

    if removed.rows_written == 0 {
        return Err(ApiError::PushSubscriptionNotFound);
    }

    Ok(NoContent)
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
