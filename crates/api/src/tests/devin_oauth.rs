//! Linking a Devin account by signing in, end to end through the router.
//!
//! The wire format of the two Devin calls is pinned separately, against
//! recorded exchanges, in [`crate::devin`]. What is asserted here is
//! everything around them: that the attempt is single-use and bound to the
//! user who started it, that a refusal reaches the caller as a problem
//! document they can act on, and that the token the exchange produced is
//! sealed before it is stored.

use flyco_core::{
    CompleteDevinOauth, CurrentUser, DevinOauthStart, HarnessAccountView, HarnessKind, Problem,
};
use skyzen::routing::Router;
use skyzen_services::{Db, Kv};
use skyzen_test::{TestClient, TestContext};

use crate::harness_accounts::{self, StoredCredential};
use crate::session;
use crate::testing::{
    DEVIN_ACCOUNT_NAME, DEVIN_CODE, DEVIN_SESSION_TOKEN, migrated_router, seed_other_user,
    seed_user, test_config,
};

const START: &str = "/v1/harness-accounts/devin/oauth/start";
const COMPLETE: &str = "/v1/harness-accounts/devin/oauth/complete";

/// A signed-in caller and the router they call.
async fn signed_in(
    ctx: &TestContext,
    kv: &Kv,
    db: &Db,
) -> (TestClient<Router>, CurrentUser, String) {
    let router = migrated_router(db).await;
    let user = seed_user(db).await;
    let token = session::issue(kv, user.id).await.expect("issue a session");
    (ctx.client(router), user, token)
}

/// Begins a sign-in, returning the attempt.
async fn begin(client: &TestClient<Router>, token: &str) -> DevinOauthStart {
    let response = client.post(START).bearer(token).send().await;
    response.assert_status(200);
    response.json()
}

async fn complete(
    client: &TestClient<Router>,
    token: &str,
    started: &DevinOauthStart,
    code: &str,
) -> skyzen_test::TestResponse {
    client
        .post(COMPLETE)
        .bearer(token)
        .json(&CompleteDevinOauth {
            attempt_id: started.attempt_id,
            code: code.to_owned(),
        })
        .send()
        .await
}

#[skyzen::test]
async fn a_sign_in_starts_at_devins_manual_flow_with_a_challenge(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token).await;

    let url = url::Url::parse(&started.authorize_url).expect("an absolute authorize URL");
    assert_eq!(url.host_str(), Some("app.devin.ai"));
    assert_eq!(url.path(), "/auth/cli/continue");
    let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    // The port-free variant: the marker makes the page show the code, and
    // no redirect URI sends the browser to a port nothing binds.
    assert_eq!(query.get("cli_pkce_marker").map(String::as_str), Some("1"));
    assert_eq!(query.get("redirect_uri"), None);
    assert_eq!(
        query.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    assert_eq!(
        query.get("prompt").map(String::as_str),
        Some("select_account")
    );
    // The verifier is the half that must never leave the control plane.
    assert!(!started.authorize_url.contains("code_verifier"));

    let second = begin(&client, &token).await;
    assert_ne!(started.attempt_id, second.attempt_id);
    assert_ne!(
        started.authorize_url, second.authorize_url,
        "every attempt mints its own challenge"
    );
}

#[skyzen::test]
async fn the_pasted_code_links_the_account(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token).await;

    let response = complete(&client, &token, &started, DEVIN_CODE).await;
    response.assert_status(201);

    let view: HarnessAccountView = response.json();
    assert_eq!(view.harness, HarnessKind::Devin);
    assert_eq!(view.label, DEVIN_ACCOUNT_NAME);
    // A session token states no lifetime, the way a pasted key does not.
    assert_eq!(view.expires_at_unix, None);

    // The account is listed, and nothing about it carries the token.
    let listed = client
        .get("/v1/harness-accounts")
        .bearer(&token)
        .send()
        .await;
    listed.assert_status(200);
    let body = serde_json::to_string(&listed.json::<Vec<HarnessAccountView>>())
        .expect("encode the account list");
    assert!(!body.contains(DEVIN_SESSION_TOKEN));
}

#[skyzen::test]
async fn the_whitespace_a_copy_picks_up_is_dropped(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token).await;

    complete(&client, &token, &started, &format!("  {DEVIN_CODE}\n"))
        .await
        .assert_status(201);
}

#[skyzen::test]
async fn the_token_is_sealed_before_it_is_stored(ctx: TestContext, kv: Kv, db: Db) {
    let (client, user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token).await;
    complete(&client, &token, &started, DEVIN_CODE)
        .await
        .assert_status(201);

    let user_id = user.id;
    let sealed: String = skyzen::sql!(
        db,
        "SELECT credential_enc FROM harness_accounts WHERE user_id = {user_id}"
    )
    .fetch_scalar()
    .await
    .expect("read the sealed credential");
    assert!(!sealed.contains(DEVIN_SESSION_TOKEN));

    let stored = harness_accounts::stored(&db, &test_config(), user.id, HarnessKind::Devin)
        .await
        .expect("read the stored credential")
        .expect("the account is linked");
    assert!(matches!(
        stored,
        StoredCredential::ApiKey { ref key } if key == DEVIN_SESSION_TOKEN
    ));
}

#[skyzen::test]
async fn an_attempt_is_good_once(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token).await;

    complete(&client, &token, &started, DEVIN_CODE)
        .await
        .assert_status(201);

    let again = complete(&client, &token, &started, DEVIN_CODE).await;
    again.assert_status(400);
    assert_eq!(
        again.json::<Problem>().kind,
        "https://flyco.dev/problems/devin-oauth-attempt-expired"
    );
}

#[skyzen::test]
async fn one_user_cannot_finish_anothers_sign_in(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token).await;

    let stranger = seed_other_user(&db).await;
    let stranger = session::issue(&kv, stranger.id)
        .await
        .expect("issue a session");

    let response = complete(&client, &stranger, &started, DEVIN_CODE).await;
    response.assert_status(400);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/devin-oauth-attempt-expired"
    );
}

#[skyzen::test]
async fn a_code_devin_refuses_is_the_callers_problem(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token).await;

    let response = complete(&client, &token, &started, "mistyped").await;
    response.assert_status(422);
    let problem = response.json::<Problem>();
    assert_eq!(
        problem.kind,
        "https://flyco.dev/problems/devin-oauth-rejected"
    );
    assert!(
        problem.detail.contains("invalid_grant"),
        "the caller is told what Devin said: {}",
        problem.detail
    );
}

#[skyzen::test]
async fn an_empty_paste_is_refused_before_devin_is_called(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token).await;

    let response = complete(&client, &token, &started, "   ").await;
    response.assert_status(422);
    assert!(
        response
            .json::<Problem>()
            .kind
            .ends_with("invalid-harness-credential"),
        "a blank paste holds no code"
    );
}
