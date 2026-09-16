//! Linking a Devin account by signing in, end to end through the router.
//!
//! The wire format of the two Devin calls is pinned separately, against
//! recorded exchanges, in [`crate::devin`]. What is asserted here is
//! everything around them: that the attempt is single-use, bound to the
//! user who started it and to the `state` it published, that a refusal
//! reaches the caller as a problem document they can act on, and that the
//! token the exchange produced is sealed before it is stored.

use flyco_core::{
    CompleteDevinOauth, CurrentUser, DevinOauthStart, HarnessAccountView, HarnessKind, Problem,
};
use skyzen::routing::Router;
use skyzen_services::{Db, Kv};
use skyzen_test::{TestClient, TestContext};

use crate::devin::REDIRECT_URI;
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

/// Begins a sign-in, returning the attempt and the `state` it published.
async fn begin(client: &TestClient<Router>, token: &str) -> (DevinOauthStart, String) {
    let response = client.post(START).bearer(token).send().await;
    response.assert_status(200);
    let started: DevinOauthStart = response.json();

    let url = url::Url::parse(&started.authorize_url).expect("an absolute authorize URL");
    let state = url
        .query_pairs()
        .find_map(|(key, value)| (key == "state").then(|| value.into_owned()))
        .expect("the authorize URL carries the state");
    (started, state)
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

/// What the pasted string looks like: the address of the page that could
/// not load, `code` and `state` in its query.
fn pasted(code: &str, state: &str) -> String {
    format!("{REDIRECT_URI}?code={code}&state={state}")
}

#[skyzen::test]
async fn a_sign_in_starts_at_devin_with_a_challenge_and_a_state(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let (started, state) = begin(&client, &token).await;

    let url = url::Url::parse(&started.authorize_url).expect("an absolute authorize URL");
    assert_eq!(url.host_str(), Some("app.devin.ai"));
    assert_eq!(url.path(), "/auth/cli/continue");
    let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(
        query.get("redirect_uri").map(String::as_str),
        Some(REDIRECT_URI)
    );
    assert_eq!(
        query.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    assert_eq!(
        query.get("prompt").map(String::as_str),
        Some("select_account")
    );
    assert!(!state.is_empty());
    // The verifier is the half that must never leave the control plane.
    assert!(!started.authorize_url.contains("code_verifier"));

    let (second, second_state) = begin(&client, &token).await;
    assert_ne!(started.attempt_id, second.attempt_id);
    assert_ne!(state, second_state, "every attempt mints its own state");
}

#[skyzen::test]
async fn a_pasted_redirect_url_links_the_account(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let (started, state) = begin(&client, &token).await;

    let response = complete(&client, &token, &started, &pasted(DEVIN_CODE, &state)).await;
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
async fn the_bare_code_is_accepted_without_its_state(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let (started, _state) = begin(&client, &token).await;

    complete(&client, &token, &started, DEVIN_CODE)
        .await
        .assert_status(201);
}

#[skyzen::test]
async fn the_token_is_sealed_before_it_is_stored(ctx: TestContext, kv: Kv, db: Db) {
    let (client, user, token) = signed_in(&ctx, &kv, &db).await;
    let (started, state) = begin(&client, &token).await;
    complete(&client, &token, &started, &pasted(DEVIN_CODE, &state))
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
    let (started, state) = begin(&client, &token).await;

    complete(&client, &token, &started, &pasted(DEVIN_CODE, &state))
        .await
        .assert_status(201);

    let again = complete(&client, &token, &started, &pasted(DEVIN_CODE, &state)).await;
    again.assert_status(400);
    assert_eq!(
        again.json::<Problem>().kind,
        "https://flyco.dev/problems/devin-oauth-attempt-expired"
    );
}

#[skyzen::test]
async fn a_code_pasted_into_a_different_sign_in_is_refused(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let (first, _first_state) = begin(&client, &token).await;
    let (_second, second_state) = begin(&client, &token).await;

    // The state belongs to the second attempt; the attempt id to the first.
    let response = complete(&client, &token, &first, &pasted(DEVIN_CODE, &second_state)).await;
    response.assert_status(400);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/devin-oauth-state-mismatch"
    );
}

#[skyzen::test]
async fn one_user_cannot_finish_anothers_sign_in(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let (started, state) = begin(&client, &token).await;

    let stranger = seed_other_user(&db).await;
    let stranger = session::issue(&kv, stranger.id)
        .await
        .expect("issue a session");

    let response = complete(&client, &stranger, &started, &pasted(DEVIN_CODE, &state)).await;
    response.assert_status(400);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/devin-oauth-attempt-expired"
    );
}

#[skyzen::test]
async fn a_code_devin_refuses_is_the_callers_problem(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let (started, state) = begin(&client, &token).await;

    let response = complete(&client, &token, &started, &pasted("mistyped", &state)).await;
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
async fn a_declined_consent_is_the_refusal_devin_stated(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let (started, _state) = begin(&client, &token).await;

    let refused = format!("{REDIRECT_URI}?error=access_denied&error_description=User+declined");
    let response = complete(&client, &token, &started, &refused).await;
    response.assert_status(422);
    let problem = response.json::<Problem>();
    assert_eq!(
        problem.kind,
        "https://flyco.dev/problems/devin-oauth-rejected"
    );
    assert!(
        problem.detail.contains("access_denied"),
        "the caller is told the consent was declined: {}",
        problem.detail
    );
}

#[skyzen::test]
async fn an_empty_paste_is_refused_before_devin_is_called(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let (started, _state) = begin(&client, &token).await;

    for paste in ["   ", REDIRECT_URI] {
        let response = complete(&client, &token, &started, paste).await;
        response.assert_status(422);
        assert!(
            response
                .json::<Problem>()
                .kind
                .ends_with("invalid-harness-credential"),
            "paste {paste:?} holds no code"
        );
    }
}
