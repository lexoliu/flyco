//! Router assembly and the handlers that are not part of the OAuth flow.

use flyco_core::{ApiKeyId, ApiKeySummary, CreateApiKey, CreatedApiKey, CurrentUser};
use serde::Serialize;
use skyzen::middleware::ErrorHandlingMiddleware;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Router, Routes as _};
use skyzen::utils::{Json, State};
use skyzen::{Body, HttpError as _, Response, StatusCode};
use skyzen_services::Db;

use crate::api_keys;
use crate::authenticator::FlycoAuthenticator;
use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::github::{GithubOauth, ZenwaveGithub};
use crate::middleware::RequireAuth;
use crate::problem::Outcome;
use crate::{database, oauth, problem};

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
) -> Outcome<Json<CreatedApiKey>> {
    api_keys::create(&db, user.id, request.label)
        .await
        .inspect(|key| tracing::info!(label = %key.label, "minted an API key"))
        .map(Json)
        .into()
}

/// `GET /v1/api-keys` — lists the caller's keys.
async fn list_api_keys(
    State(user): State<CurrentUser>,
    db: Db,
) -> Outcome<Json<Vec<ApiKeySummary>>> {
    api_keys::list(&db, user.id).await.map(Json).into()
}

/// `DELETE /v1/api-keys/{id}` — revokes one of the caller's keys.
async fn revoke_api_key(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Outcome<Response> {
    revoke(&user, &params, &db).await.into()
}

async fn revoke(user: &CurrentUser, params: &Params, db: &Db) -> Result<Response, ApiError> {
    let raw = params
        .get("id")
        .map_err(|_| ApiError::CorruptRecord("the router did not bind the `id` path parameter"))?;
    let key_id = raw
        .parse::<ApiKeyId>()
        .map_err(|_| ApiError::MalformedId(raw.to_owned()))?;

    api_keys::revoke(db, user.id, key_id).await?;

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

/// Routes that require a bearer credential.
fn authenticated_routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/me".at(me),
        "/v1/api-keys".post(create_api_key).get(list_api_keys),
        "/v1/api-keys/{id}".delete(revoke_api_key),
    ))
    .middleware(RequireAuth::new(FlycoAuthenticator::new()))
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
        // Outermost, so extractor and routing failures answer in the same
        // shape flyco's own errors do.
        .with(ErrorHandlingMiddleware::new(
            |error: skyzen::BoxHttpError| async move {
                let status = error.status();
                let title = status.canonical_reason().unwrap_or("Error");
                let detail = if status.is_server_error() {
                    tracing::error!(%error, "request failed");
                    "The control plane failed to handle this request.".to_owned()
                } else {
                    error.to_string()
                };
                problem::response(
                    &flyco_core::Problem::about_blank(status.as_u16(), title, detail),
                    None,
                )
            },
        ))
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
    use flyco_core::{ApiKeySummary, CreateApiKey, CreatedApiKey, CurrentUser, Problem};
    use skyzen_services::{Db, Kv};
    use skyzen_test::TestContext;

    use crate::testing::{GITHUB_LOGIN, migrated_router, seed_user, test_router};
    use crate::{api_keys, session};

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
    async fn an_anonymous_request_is_challenged(ctx: TestContext, _kv: Kv, db: Db) {
        let response = ctx
            .client(migrated_router(&db).await)
            .get("/v1/me")
            .send()
            .await;

        response.assert_status(401);
        response.assert_header("content-type", "application/problem+json");
        response.assert_header("www-authenticate", "Bearer");

        let problem: Problem = response.json();
        assert_eq!(problem.status, 401);
        assert_eq!(
            problem.kind,
            "https://flyco.dev/problems/missing-credential"
        );
    }

    #[skyzen::test]
    async fn a_rejected_credential_is_named_as_such(ctx: TestContext, _kv: Kv, db: Db) {
        let client = ctx.client(migrated_router(&db).await);

        for token in [
            "fk_not-a-real-key",
            "fs_not-a-real-session",
            "neither-prefix",
        ] {
            let response = client.get("/v1/me").bearer(token).send().await;
            response.assert_status(401);
            response.assert_header("www-authenticate", "Bearer error=\"invalid_token\"");
            assert_eq!(
                response.json::<Problem>().kind,
                "https://flyco.dev/problems/invalid-credential"
            );
        }
    }

    #[skyzen::test]
    async fn a_non_bearer_scheme_is_treated_as_no_credential(ctx: TestContext, _kv: Kv, db: Db) {
        let response = ctx
            .client(migrated_router(&db).await)
            .get("/v1/me")
            .header("Authorization", "Basic Zm9vOmJhcg==")
            .send()
            .await;

        response.assert_status(401);
        response.assert_header("www-authenticate", "Bearer");
    }

    #[skyzen::test]
    async fn me_answers_for_a_session_token(ctx: TestContext, kv: Kv, db: Db) {
        let router = migrated_router(&db).await;
        let user = seed_user(&db).await;
        let token = session::issue(&kv, user.id).await.expect("issue a session");
        assert!(token.starts_with(session::TOKEN_PREFIX));

        let response = ctx.client(router).get("/v1/me").bearer(&token).send().await;

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

        let created = client
            .post("/v1/api-keys")
            .bearer(&token)
            .json(&CreateApiKey {
                label: "laptop".to_owned(),
            })
            .send()
            .await;
        created.assert_status(200);
        let created: CreatedApiKey = created.json();
        assert!(created.token.starts_with(api_keys::TOKEN_PREFIX));
        assert_eq!(created.label, "laptop");

        // D1 keeps the hash and nothing else, so the plaintext key cannot be
        // recovered from the table.
        let stored: std::collections::BTreeMap<String, String> = db
            .query("SELECT token_hash FROM api_keys WHERE id = ?")
            .bind(created.id.to_string())
            .fetch_one()
            .await
            .expect("read the stored key");
        let stored = stored.get("token_hash").expect("token_hash column");
        assert_eq!(stored, &crate::crypto::token_hash(&created.token));
        assert!(!stored.contains(&created.token));

        let listed = client.get("/v1/api-keys").bearer(&token).send().await;
        listed.assert_status(200);
        let listed: Vec<ApiKeySummary> = listed.json();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, created.id);
        assert_eq!(listed[0].last_used_unix, None);

        let path = format!("/v1/api-keys/{}", created.id);
        client
            .delete(&path)
            .bearer(&token)
            .send()
            .await
            .assert_status(204);

        let gone = client.delete(&path).bearer(&token).send().await;
        gone.assert_status(404);
        assert_eq!(
            gone.json::<Problem>().kind,
            "https://flyco.dev/problems/api-key-not-found"
        );

        let listed = client.get("/v1/api-keys").bearer(&token).send().await;
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

        let response = ctx
            .client(router)
            .delete("/v1/api-keys/not-a-uuid")
            .bearer(&token)
            .send()
            .await;

        response.assert_status(400);
        assert_eq!(
            response.json::<Problem>().kind,
            "https://flyco.dev/problems/malformed-id"
        );
    }

    #[skyzen::test]
    async fn an_extractor_failure_still_answers_with_a_problem(ctx: TestContext, kv: Kv, db: Db) {
        let router = migrated_router(&db).await;
        let user = seed_user(&db).await;
        let token = session::issue(&kv, user.id).await.expect("issue a session");

        // No `Content-Type: application/json`, so the body extractor refuses.
        let response = ctx
            .client(router)
            .post("/v1/api-keys")
            .bearer(&token)
            .body("{}")
            .send()
            .await;

        response.assert_status(400);
        response.assert_header("content-type", "application/problem+json");
        assert_eq!(response.json::<Problem>().kind, "about:blank");
    }
}
