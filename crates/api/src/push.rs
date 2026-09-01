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
    CurrentUser, PushSubscription, PushSubscriptionId, PushSubscriptionView, SessionId, UserId,
    VapidPublicKey,
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
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use jwt_compact::{AlgorithmExt as _, Claims, Header, alg::Es256};
use web_push_native::{Auth, WebPushBuilder, p256::PublicKey};
use zenwave::Client as _;

const PUSH_TTL_SECONDS: u32 = 60 * 60;

/// A user-visible notification delivered by the service worker.
#[derive(Debug, serde::Serialize)]
struct PushNotification<'a> {
    title: &'a str,
    body: &'a str,
    url: String,
    tag: String,
}

#[derive(serde::Serialize)]
struct VapidClaims<'a> {
    aud: String,
    sub: &'a str,
    exp: u64,
}

#[derive(Debug, skyzen::FromRow)]
struct DeliverySubscription {
    id: PushSubscriptionId,
    endpoint: String,
    p256dh: String,
    auth: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    Sent,
    Expired,
}

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
    Ok::<_, ApiError>(Json(VapidPublicKey {
        key: config.vapid().public_key().to_owned(),
    }))
    .into()
}

/// Registers this browser for push notifications.
#[skyzen::openapi]
async fn subscribe_push(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    Json(subscription): Json<PushSubscription>,
    db: Db,
) -> Outcome<Created<Json<PushSubscriptionView>>> {
    subscribe(&db, &config, user.id, subscription)
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
    config: &ApiConfig,
    user: UserId,
    subscription: PushSubscription,
) -> Result<PushSubscriptionView, ApiError> {
    if subscription.endpoint.trim().is_empty() {
        return Err(ApiError::InvalidPushSubscription(
            "the endpoint is required",
        ));
    }
    let endpoint = url::Url::parse(&subscription.endpoint)
        .map_err(|_| ApiError::InvalidPushSubscription("the endpoint must be an absolute URL"))?;
    if endpoint.scheme() != "https" {
        return Err(ApiError::InvalidPushSubscription(
            "the endpoint must use HTTPS",
        ));
    }
    build_message(config, &subscription, b"Flyco subscription check").map_err(|_| {
        ApiError::InvalidPushSubscription("the endpoint or encryption keys are invalid")
    })?;

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

/// Sends an approval-required notification to every browser owned by the
/// session's user.
///
/// # Errors
///
/// Returns a database or Web Push encoding error.
pub async fn notify_approval(
    db: &Db,
    config: &ApiConfig,
    session: SessionId,
) -> Result<(), ApiError> {
    notify_session(
        db,
        config,
        session,
        PushNotification {
            title: "Approval required",
            body: "A Flyco session is waiting for your decision.",
            url: format!("/sessions/{session}"),
            tag: format!("approval-{session}"),
        },
    )
    .await
}

/// Sends a terminal-turn notification to every browser owned by the
/// session's user.
///
/// # Errors
///
/// Returns a database or Web Push encoding error.
pub async fn notify_turn(
    db: &Db,
    config: &ApiConfig,
    session: SessionId,
    completed: bool,
) -> Result<(), ApiError> {
    let (title, body) = if completed {
        ("Turn completed", "Your Flyco session finished its turn.")
    } else {
        ("Turn failed", "Your Flyco session needs your attention.")
    };
    notify_session(
        db,
        config,
        session,
        PushNotification {
            title,
            body,
            url: format!("/sessions/{session}"),
            tag: format!("turn-{session}"),
        },
    )
    .await
}

async fn notify_session(
    db: &Db,
    config: &ApiConfig,
    session: SessionId,
    notification: PushNotification<'_>,
) -> Result<(), ApiError> {
    let subscriptions: Vec<DeliverySubscription> = sql!(
        db,
        "SELECT p.id, p.endpoint, p.p256dh, p.auth FROM push_subscriptions p \
         JOIN sessions s ON s.user_id = p.user_id WHERE s.id = {session}"
    )
    .fetch_all()
    .await?;
    let payload = serde_json::to_vec(&notification)
        .map_err(|error| ApiError::PushDeliveryFailed(error.to_string()))?;

    for subscription in subscriptions {
        match deliver(config, &subscription, &payload).await {
            Ok(Delivery::Sent) => {}
            Ok(Delivery::Expired) => {
                let id = subscription.id;
                sql!(db, "DELETE FROM push_subscriptions WHERE id = {id}")
                    .execute()
                    .await?;
            }
            Err(error) => {
                tracing::error!(%error, subscription = %subscription.id, %session, "web push delivery failed");
            }
        }
    }
    Ok(())
}

async fn deliver(
    config: &ApiConfig,
    subscription: &DeliverySubscription,
    payload: &[u8],
) -> Result<Delivery, ApiError> {
    let source = PushSubscription {
        endpoint: subscription.endpoint.clone(),
        expiration_time: None,
        keys: flyco_core::PushKeys {
            p256dh: subscription.p256dh.clone(),
            auth: subscription.auth.clone(),
        },
    };
    let request = build_message(config, &source, payload)?;
    let (parts, body) = request.into_parts();
    let mut client = zenwave::client();
    let mut outbound = client
        .post(parts.uri.to_string())
        .map_err(|error| ApiError::PushDeliveryFailed(error.to_string()))?;
    for (name, value) in &parts.headers {
        outbound = outbound
            .header(name.as_str(), value.as_bytes())
            .map_err(|error| ApiError::PushDeliveryFailed(error.to_string()))?;
    }
    match outbound.bytes_body(body).await {
        Ok(_) => Ok(Delivery::Sent),
        Err(zenwave::Error::Http { status, .. }) if matches!(status.as_u16(), 404 | 410) => {
            Ok(Delivery::Expired)
        }
        Err(error) => Err(ApiError::PushDeliveryFailed(error.to_string())),
    }
}

fn build_message(
    config: &ApiConfig,
    subscription: &PushSubscription,
    payload: &[u8],
) -> Result<http::Request<Vec<u8>>, ApiError> {
    let endpoint: http::Uri = subscription
        .endpoint
        .parse()
        .map_err(|error: http::uri::InvalidUri| ApiError::PushDeliveryFailed(error.to_string()))?;
    let public_bytes = URL_SAFE_NO_PAD
        .decode(&subscription.keys.p256dh)
        .map_err(|error| ApiError::PushDeliveryFailed(error.to_string()))?;
    let public_key = PublicKey::from_sec1_bytes(&public_bytes)
        .map_err(|error| ApiError::PushDeliveryFailed(error.to_string()))?;
    let auth_bytes = URL_SAFE_NO_PAD
        .decode(&subscription.keys.auth)
        .map_err(|error| ApiError::PushDeliveryFailed(error.to_string()))?;
    let auth: [u8; 16] = auth_bytes.try_into().map_err(|_| {
        ApiError::InvalidPushSubscription("the auth secret must be exactly 16 bytes")
    })?;
    let mut request = WebPushBuilder::new(endpoint, public_key, Auth::from(auth))
        .with_valid_duration(core::time::Duration::from_secs(u64::from(PUSH_TTL_SECONDS)))
        .build(payload)
        .map_err(|error| ApiError::PushDeliveryFailed(error.to_string()))?;

    let endpoint_url = url::Url::parse(&subscription.endpoint)
        .map_err(|error| ApiError::PushDeliveryFailed(error.to_string()))?;
    let claims = Claims::new(VapidClaims {
        aud: endpoint_url.origin().ascii_serialization(),
        sub: config.vapid().subject().as_str(),
        exp: now_unix() + 12 * 60 * 60,
    });
    let token = Es256
        .token(&Header::empty(), &claims, config.vapid().signing_key())
        .map_err(|error| ApiError::PushDeliveryFailed(error.to_string()))?;
    let authorization = format!("vapid t={token}, k={}", config.vapid().public_key());
    request.headers_mut().insert(
        http::header::AUTHORIZATION,
        authorization
            .parse()
            .map_err(|error: http::header::InvalidHeaderValue| {
                ApiError::PushDeliveryFailed(error.to_string())
            })?,
    );
    Ok(request)
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

#[cfg(test)]
mod unit {
    use flyco_core::{PushKeys, PushSubscription};

    use super::build_message;
    use crate::testing::test_config;

    #[test]
    fn builds_an_encrypted_vapid_authenticated_request() {
        let subscription = PushSubscription {
            endpoint: "https://updates.push.services.mozilla.com/wpush/v2/flyco-test"
                .to_owned(),
            expiration_time: None,
            keys: PushKeys {
                p256dh: "BH1HTeKM7-NwaLGHEqxeu2IamQaVVLkcsFHPIHmsCnqxcBHPQBprF41bEMOr3O1hUQ2jU1opNEm1F_lZV_sxMP8".to_owned(),
                auth: "sBXU5_tIYz-5w7G2B25BEw".to_owned(),
            },
        };
        let plaintext = b"approval required";
        let request = build_message(&test_config(), &subscription, plaintext)
            .expect("the browser subscription is valid");

        assert_eq!(request.headers()["content-encoding"], "aes128gcm");
        assert!(
            request.headers()["authorization"]
                .to_str()
                .expect("authorization is text")
                .starts_with("vapid t=")
        );
        assert!(
            !request
                .body()
                .windows(plaintext.len())
                .any(|window| window == plaintext)
        );
    }
}
