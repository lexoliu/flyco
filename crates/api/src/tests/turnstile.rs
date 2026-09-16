//! The Turnstile gate on `POST /v1/auth/github/start`, end to end.
//!
//! [`crate::turnstile`]'s own tests pin the decision logic; these pin the
//! wiring: a refused check mints no OAuth state at all, a siteverify that
//! cannot be reached is a `502` rather than a sign-in, and the deployment's
//! public config carries the sitekey the login page renders.

use serde_json::json;
use skyzen::routing::Router;
use skyzen_services::{Db, Kv, Queue};
use skyzen_test::TestContext;
use skyzen_test::mock::InMemoryQueue;

use crate::testing::{
    TestCodespaces, TestGithub, TestTurnstile, migrate, test_router_full, test_vendors,
};

/// A migrated router whose sign-in gate consults the given verdict.
async fn router_with(db: &Db, turnstile: TestTurnstile) -> Router {
    migrate(db).await;
    test_router_full(
        db.clone(),
        Queue::new(InMemoryQueue::new()),
        TestGithub::default(),
        turnstile,
        test_vendors(),
        TestCodespaces::succeeding(),
    )
}

/// The body the login page posts once its widget has answered.
fn start_body() -> serde_json::Value {
    json!({"turnstile_token": "the-widget's-answer"})
}

/// OAuth states minted so far — a refused sign-in must leave none behind.
async fn minted_states(kv: &Kv) -> Vec<String> {
    kv.list_all(Some("auth:oauth-state:"))
        .await
        .expect("list the KV")
}

#[skyzen::test]
async fn a_passed_check_opens_sign_in(ctx: TestContext, _kv: Kv, db: Db) {
    let client = ctx.client(router_with(&db, TestTurnstile::passing()).await);

    let response = client
        .post("/v1/auth/github/start")
        .json(&start_body())
        .send()
        .await;

    response.assert_status(200);
    let body: flyco_core::AuthorizeUrl = response.json();
    assert!(body.authorize_url.contains("github.com"));
}

#[skyzen::test]
async fn a_refused_check_mints_no_state(ctx: TestContext, kv: Kv, db: Db) {
    let client =
        ctx.client(router_with(&db, TestTurnstile::refusing(&["invalid-input-response"])).await);

    let response = client
        .post("/v1/auth/github/start")
        .json(&start_body())
        .send()
        .await;

    response.assert_status(403);
    assert_eq!(
        response.json::<flyco_core::Problem>().kind,
        "https://flyco.dev/problems/turnstile-refused"
    );
    assert!(
        minted_states(&kv).await.is_empty(),
        "a refused check must mint no OAuth state"
    );
}

#[skyzen::test]
async fn a_pass_from_another_site_is_refused(ctx: TestContext, _kv: Kv, db: Db) {
    let client = ctx.client(router_with(&db, TestTurnstile::elsewhere()).await);

    let response = client
        .post("/v1/auth/github/start")
        .json(&start_body())
        .send()
        .await;

    response.assert_status(403);
    assert_eq!(
        response.json::<flyco_core::Problem>().kind,
        "https://flyco.dev/problems/turnstile-refused"
    );
}

#[skyzen::test]
async fn siteverify_failing_is_an_outage_not_a_sign_in(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(router_with(&db, TestTurnstile::unreachable()).await);

    let response = client
        .post("/v1/auth/github/start")
        .json(&start_body())
        .send()
        .await;

    response.assert_status(502);
    assert_eq!(
        response.json::<flyco_core::Problem>().kind,
        "https://flyco.dev/problems/turnstile-unavailable"
    );
    assert!(minted_states(&kv).await.is_empty());
}

#[skyzen::test]
async fn the_public_config_serves_the_sitekey(ctx: TestContext, _kv: Kv, db: Db) {
    let client = ctx.client(router_with(&db, TestTurnstile::passing()).await);

    let response = client.get("/v1/config").send().await;

    response.assert_status(200);
    assert_eq!(
        response.json::<serde_json::Value>()["turnstile_sitekey"],
        crate::testing::TURNSTILE_SITEKEY
    );
}
