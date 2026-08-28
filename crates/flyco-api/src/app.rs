//! Router assembly and the handlers that are not part of the OAuth flow.

use flyco_core::{ApiKeyId, ApiKeySummary, CreateApiKey, CreatedApiKey, CurrentUser};
use serde::Serialize;
use skyzen::middleware::auth::AuthMiddleware;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Router, Routes as _};
use skyzen::utils::{Json, State};
use skyzen::{Body, Response, StatusCode};
use skyzen_services::Db;

use crate::api_keys;
use crate::authenticator::FlycoAuthenticator;
use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::github::{GithubOauth, ZenwaveGithub};
use crate::{database, oauth};

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

/// `GET /v1/me` — the identity behind the presented credential.
async fn me(State(user): State<CurrentUser>) -> Json<CurrentUser> {
    Json(user)
}

/// `POST /v1/api-keys` — mints a key and returns it once.
async fn create_api_key(
    State(user): State<CurrentUser>,
    Json(request): Json<CreateApiKey>,
    db: Db,
) -> Result<Json<CreatedApiKey>, ApiError> {
    let key = api_keys::create(&db, user.id, request.label).await?;
    tracing::info!(label = %key.label, "minted an API key");
    Ok(Json(key))
}

/// `GET /v1/api-keys` — lists the caller's keys.
async fn list_api_keys(
    State(user): State<CurrentUser>,
    db: Db,
) -> Result<Json<Vec<ApiKeySummary>>, ApiError> {
    Ok(Json(api_keys::list(&db, user.id).await?))
}

/// `DELETE /v1/api-keys/{id}` — revokes one of the caller's keys.
async fn revoke_api_key(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Result<Response, ApiError> {
    let raw = params
        .get("id")
        .map_err(|_| ApiError::CorruptRecord("the router did not bind the `id` path parameter"))?;
    let key_id = raw
        .parse::<ApiKeyId>()
        .map_err(|_| ApiError::MalformedId(raw.to_owned()))?;

    api_keys::revoke(&db, user.id, key_id).await?;

    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    Ok(response)
}

/// Routes that anyone may call.
fn public_routes<G: GithubOauth>() -> Vec<RouteNode> {
    Route::new((
        "/v1/healthz".at(healthz),
        "/v1/auth/github".route((
            "/start".post(oauth::start),
            "/callback".at(oauth::callback::<G>),
        )),
    ))
    .into_route_nodes()
}

/// Routes that require a session cookie or an API key.
fn authenticated_routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/me".at(me),
        "/v1/api-keys".post(create_api_key).get(list_api_keys),
        "/v1/api-keys/{id}".delete(revoke_api_key),
    ))
    .middleware(AuthMiddleware::new(FlycoAuthenticator::new()))
    .into_route_nodes()
}

/// Builds the control-plane router around an explicit configuration, GitHub
/// client, and database.
///
/// All three are injected rather than discovered, so tests can drive the
/// OAuth callback without reaching `github.com` or a real D1. The KV store
/// arrives separately: it is a declared portable service, so
/// `#[skyzen::main]` wraps the router with it.
#[must_use]
pub fn router<G: GithubOauth>(config: ApiConfig, github: G, db: Db) -> Router {
    let mut nodes = public_routes::<G>();
    nodes.extend(authenticated_routes());

    Route::new(nodes)
        .with(State(config))
        .with(State(github))
        .with(db)
        .build()
}

/// Builds the router the deployed control plane runs.
///
/// # Panics
///
/// Panics if any required configuration binding is missing or malformed, or
/// if the database cannot be opened — a misconfigured control plane must
/// fail at startup, not at the first sign-in attempt.
pub async fn router_from_environment() -> Router {
    let config = ApiConfig::from_environment()
        .unwrap_or_else(|error| panic!("flyco control plane is misconfigured: {error}"));
    router(config, ZenwaveGithub::new(), database::open().await)
}

#[cfg(test)]
mod tests {
    use flyco_core::{ApiKeySummary, CreateApiKey, CreatedApiKey, CurrentUser};
    use skyzen_services::{Db, Kv};
    use skyzen_test::TestContext;

    use crate::testing::{GITHUB_LOGIN, migrated_router, seed_user, test_router};
    use crate::{api_keys, session};

    fn cookie_header(token: &str) -> String {
        let mut header = String::from("flyco_session=");
        header.push_str(token);
        header
    }

    #[skyzen::test]
    async fn healthz_reports_protocol_version(ctx: TestContext, db: Db) {
        let client = ctx.client(test_router(db));
        let response = client.get("/v1/healthz").send().await;
        response.assert_status(200);
        let body: serde_json::Value = response.json();
        assert_eq!(
            body["wire_protocol_version"],
            u64::from(flyco_core::WIRE_PROTOCOL_VERSION)
        );
    }

    #[skyzen::test]
    async fn me_rejects_an_anonymous_request(ctx: TestContext, _kv: Kv, db: Db) {
        let client = ctx.client(migrated_router(&db).await);
        client.get("/v1/me").send().await.assert_status(401);
    }

    #[skyzen::test]
    async fn me_rejects_credentials_that_do_not_exist(ctx: TestContext, _kv: Kv, db: Db) {
        let client = ctx.client(migrated_router(&db).await);

        client
            .get("/v1/me")
            .bearer("fk_not-a-real-key")
            .send()
            .await
            .assert_status(401);

        client
            .get("/v1/me")
            .header("Cookie", &cookie_header("not-a-real-session"))
            .send()
            .await
            .assert_status(401);
    }

    #[skyzen::test]
    async fn me_answers_for_a_session_cookie(ctx: TestContext, kv: Kv, db: Db) {
        let router = migrated_router(&db).await;
        let user = seed_user(&db).await;
        let token = session::issue(&kv, user.id).await.expect("issue a session");

        let response = ctx
            .client(router)
            .get("/v1/me")
            .header("Cookie", &cookie_header(&token))
            .send()
            .await;

        response.assert_status(200);
        assert_eq!(response.json::<CurrentUser>(), user);
    }

    #[skyzen::test]
    async fn me_answers_for_an_api_key(ctx: TestContext, _kv: Kv, db: Db) {
        let router = migrated_router(&db).await;
        let user = seed_user(&db).await;
        let key = api_keys::create(&db, user.id, "ci".to_owned())
            .await
            .expect("mint a key");

        let response = ctx
            .client(router)
            .get("/v1/me")
            .bearer(&key.token)
            .send()
            .await;

        response.assert_status(200);
        assert_eq!(response.json::<CurrentUser>().login, GITHUB_LOGIN);

        let listed = api_keys::list(&db, user.id).await.expect("list");
        assert!(
            listed[0].last_used_unix.is_some(),
            "authenticating with a key stamps it as used"
        );
    }

    #[skyzen::test]
    async fn api_keys_round_trip(ctx: TestContext, kv: Kv, db: Db) {
        let router = migrated_router(&db).await;
        let user = seed_user(&db).await;
        let token = session::issue(&kv, user.id).await.expect("issue a session");
        let client = ctx.client(router);
        let cookie = cookie_header(&token);

        let created = client
            .post("/v1/api-keys")
            .header("Cookie", &cookie)
            .json(&CreateApiKey {
                label: "laptop".to_owned(),
            })
            .send()
            .await;
        created.assert_status(200);
        let created: CreatedApiKey = created.json();
        assert!(created.token.starts_with("fk_"));
        assert_eq!(created.label, "laptop");

        let listed = client
            .get("/v1/api-keys")
            .header("Cookie", &cookie)
            .send()
            .await;
        listed.assert_status(200);
        let listed: Vec<ApiKeySummary> = listed.json();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, created.id);
        assert_eq!(listed[0].last_used_unix, None);

        // The plaintext key is never handed out a second time.
        assert!(!listed[0].label.contains(&created.token));

        let path = format!("/v1/api-keys/{}", created.id);
        client
            .delete(&path)
            .header("Cookie", &cookie)
            .send()
            .await
            .assert_status(204);
        client
            .delete(&path)
            .header("Cookie", &cookie)
            .send()
            .await
            .assert_status(404);

        let listed = client
            .get("/v1/api-keys")
            .header("Cookie", &cookie)
            .send()
            .await;
        assert_eq!(
            listed.json::<Vec<ApiKeySummary>>(),
            [] as [ApiKeySummary; 0]
        );
    }

    #[skyzen::test]
    async fn a_malformed_key_id_is_a_bad_request(ctx: TestContext, kv: Kv, db: Db) {
        let router = migrated_router(&db).await;
        let user = seed_user(&db).await;
        let token = session::issue(&kv, user.id).await.expect("issue a session");

        ctx.client(router)
            .delete("/v1/api-keys/not-a-uuid")
            .header("Cookie", &cookie_header(&token))
            .send()
            .await
            .assert_status(400);
    }
}
