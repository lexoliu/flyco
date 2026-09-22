//! Linking Claude Code by signing in, end to end through the router.
//!
//! The wire format of the two Anthropic calls is pinned separately, against
//! recorded exchanges, in [`crate::anthropic`]. What is asserted here is
//! everything around them: that the attempt is single-use, bound to the user
//! who started it and to the `state` it published, that a refusal reaches the
//! caller as a problem document they can act on, and that the grant the
//! exchange produced is sealed, dated, and refreshed before it is used.

use flyco_core::{
    ClaudeOauthStart, CompleteClaudeOauth, CurrentUser, HarnessAccountView, HarnessKind, Problem,
};
use flyco_provider::{ClaudeCredential, HarnessCredential};
use skyzen::routing::Router;
use skyzen_services::{Db, Kv};
use skyzen_test::{TestClient, TestContext};

use crate::clock::now_unix;
use crate::harness_accounts::{self, REFRESH_WINDOW_SECONDS, StoredCredential};
use crate::session;
use crate::testing::{
    CLAUDE_ACCESS_TOKEN, CLAUDE_ACCOUNT_EMAIL, CLAUDE_CODE, CLAUDE_REFRESH_TOKEN,
    CLAUDE_RENEWED_ACCESS_TOKEN, CLAUDE_RENEWED_REFRESH_TOKEN, CLAUDE_TOKEN_LIFETIME,
    migrated_router, seed_claude_oauth_account, seed_other_user, seed_user, test_config,
    test_vendors,
};

const START: &str = "/v1/harness-accounts/claude/oauth/start";
const COMPLETE: &str = "/v1/harness-accounts/claude/oauth/complete";

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
async fn begin(client: &TestClient<Router>, token: &str) -> (ClaudeOauthStart, String) {
    let response = client.post(START).bearer(token).send().await;
    response.assert_status(200);
    let started: ClaudeOauthStart = response.json();

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
    started: &ClaudeOauthStart,
    code: &str,
) -> skyzen_test::TestResponse {
    client
        .post(COMPLETE)
        .bearer(token)
        .json(&CompleteClaudeOauth {
            attempt_id: started.attempt_id,
            code: code.to_owned(),
        })
        .send()
        .await
}

/// What the pasted string looks like: `CODE#STATE`.
fn pasted(code: &str, state: &str) -> String {
    let mut value = String::with_capacity(code.len() + 1 + state.len());
    value.push_str(code);
    value.push('#');
    value.push_str(state);
    value
}

#[skyzen::test]
async fn a_sign_in_starts_at_claude_with_a_challenge_and_a_state(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let (started, state) = begin(&client, &token).await;

    let url = url::Url::parse(&started.authorize_url).expect("an absolute authorize URL");
    assert_eq!(url.host_str(), Some("claude.ai"));
    let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(
        query.get("client_id").map(String::as_str),
        Some(crate::testing::CLAUDE_CLIENT_ID)
    );
    assert_eq!(query.get("code").map(String::as_str), Some("true"));
    assert_eq!(
        query.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    assert_eq!(
        query.get("redirect_uri").map(String::as_str),
        Some(crate::anthropic::REDIRECT_URI)
    );
    assert!(!state.is_empty());
    // The verifier is the half that must never leave the control plane.
    assert!(!started.authorize_url.contains("code_verifier"));

    let (second, second_state) = begin(&client, &token).await;
    assert_ne!(started.attempt_id, second.attempt_id);
    assert_ne!(state, second_state, "every attempt mints its own state");
}

#[skyzen::test]
async fn a_pasted_code_and_state_links_the_account(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let (started, state) = begin(&client, &token).await;

    let before = now_unix();
    let response = complete(&client, &token, &started, &pasted(CLAUDE_CODE, &state)).await;
    response.assert_status(201);

    let view: HarnessAccountView = response.json();
    assert_eq!(view.harness, HarnessKind::ClaudeCode);
    assert_eq!(view.label, CLAUDE_ACCOUNT_EMAIL);
    let expires_at = view.expires_at_unix.expect("an OAuth grant states its end");
    assert!(expires_at >= before + CLAUDE_TOKEN_LIFETIME);

    // The account is listed, and nothing about it carries a token.
    let listed = client
        .get("/v1/harness-accounts")
        .bearer(&token)
        .send()
        .await;
    listed.assert_status(200);
    let body = serde_json::to_string(&listed.json::<Vec<HarnessAccountView>>())
        .expect("encode the account list");
    assert!(!body.contains(CLAUDE_ACCESS_TOKEN));
    assert!(!body.contains(CLAUDE_REFRESH_TOKEN));
}

#[skyzen::test]
async fn the_bare_code_is_accepted_without_its_state(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let (started, _state) = begin(&client, &token).await;

    complete(&client, &token, &started, CLAUDE_CODE)
        .await
        .assert_status(201);
}

#[skyzen::test]
async fn the_grant_is_sealed_with_the_refresh_token_that_renews_it(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, user, token) = signed_in(&ctx, &kv, &db).await;
    let (started, state) = begin(&client, &token).await;
    complete(&client, &token, &started, &pasted(CLAUDE_CODE, &state))
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
    assert!(!sealed.contains(CLAUDE_ACCESS_TOKEN));

    let stored = harness_accounts::stored(&db, &test_config(), user.id, HarnessKind::ClaudeCode)
        .await
        .expect("read the stored credential")
        .expect("the account is linked");
    assert!(matches!(
        stored,
        StoredCredential::ClaudeOauth {
            access_token,
            refresh_token,
            ..
        } if access_token == CLAUDE_ACCESS_TOKEN && refresh_token == CLAUDE_REFRESH_TOKEN
    ));
}

#[skyzen::test]
async fn an_attempt_is_good_once(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let (started, state) = begin(&client, &token).await;

    complete(&client, &token, &started, &pasted(CLAUDE_CODE, &state))
        .await
        .assert_status(201);

    let again = complete(&client, &token, &started, &pasted(CLAUDE_CODE, &state)).await;
    again.assert_status(400);
    assert_eq!(
        again.json::<Problem>().kind,
        "https://flyco.dev/problems/claude-oauth-attempt-expired"
    );
}

#[skyzen::test]
async fn a_code_pasted_into_a_different_sign_in_is_refused(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let (first, _first_state) = begin(&client, &token).await;
    let (_second, second_state) = begin(&client, &token).await;

    // The state belongs to the second attempt; the attempt id to the first.
    let response = complete(&client, &token, &first, &pasted(CLAUDE_CODE, &second_state)).await;
    response.assert_status(400);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/claude-oauth-state-mismatch"
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

    let response = complete(&client, &stranger, &started, &pasted(CLAUDE_CODE, &state)).await;
    response.assert_status(400);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/claude-oauth-attempt-expired"
    );
}

#[skyzen::test]
async fn a_code_anthropic_refuses_is_the_callers_problem(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let (started, state) = begin(&client, &token).await;

    let response = complete(&client, &token, &started, &pasted("mistyped", &state)).await;
    response.assert_status(422);
    let problem = response.json::<Problem>();
    assert_eq!(
        problem.kind,
        "https://flyco.dev/problems/claude-oauth-rejected"
    );
    assert!(
        problem.detail.contains("invalid_grant"),
        "the caller is told what Anthropic said: {}",
        problem.detail
    );
}

#[skyzen::test]
async fn an_empty_paste_is_refused_before_anthropic_is_called(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let (started, _state) = begin(&client, &token).await;

    let response = complete(&client, &token, &started, "   ").await;
    response.assert_status(422);
    assert!(
        response
            .json::<Problem>()
            .kind
            .ends_with("invalid-harness-credential")
    );
}

// ── Refresh on use ──

#[skyzen::test]
async fn a_grant_near_its_end_is_renewed_before_a_session_gets_it(_ctx: TestContext, db: Db) {
    crate::testing::migrate(&db).await;
    let user = seed_user(&db).await;
    seed_claude_oauth_account(&db, user.id, now_unix() + 60).await;

    let credential = harness_accounts::credential(
        &db,
        &test_config(),
        &test_vendors(),
        user.id,
        HarnessKind::ClaudeCode,
    )
    .await
    .expect("unseal the credential");

    assert_eq!(
        credential,
        HarnessCredential::ClaudeCode(ClaudeCredential::OauthToken {
            token: CLAUDE_RENEWED_ACCESS_TOKEN.to_owned()
        }),
        "the daemon is handed the token the refresh produced"
    );

    // The rotation is persisted: both halves and the new expiry.
    let stored = harness_accounts::stored(&db, &test_config(), user.id, HarnessKind::ClaudeCode)
        .await
        .expect("read the stored credential")
        .expect("the account is still linked");
    assert!(matches!(
        stored,
        StoredCredential::ClaudeOauth {
            access_token,
            refresh_token,
            expires_at_unix,
        } if access_token == CLAUDE_RENEWED_ACCESS_TOKEN
            && refresh_token == CLAUDE_RENEWED_REFRESH_TOKEN
            && expires_at_unix > now_unix() + REFRESH_WINDOW_SECONDS
    ));

    let user_id = user.id;
    let expires_at: Option<u64> = skyzen::sql!(
        db,
        "SELECT expires_at_unix FROM harness_accounts WHERE user_id = {user_id}"
    )
    .fetch_scalar()
    .await
    .expect("read the account's expiry");
    assert!(
        expires_at.is_some_and(|at| at > now_unix() + REFRESH_WINDOW_SECONDS),
        "the listed expiry follows the rotation"
    );
}

#[skyzen::test]
async fn a_grant_with_time_left_is_handed_over_untouched(_ctx: TestContext, db: Db) {
    crate::testing::migrate(&db).await;
    let user = seed_user(&db).await;
    let expires_at = now_unix() + REFRESH_WINDOW_SECONDS + 3_600;
    seed_claude_oauth_account(&db, user.id, expires_at).await;

    let credential = harness_accounts::credential(
        &db,
        &test_config(),
        &test_vendors(),
        user.id,
        HarnessKind::ClaudeCode,
    )
    .await
    .expect("unseal the credential");

    assert_eq!(
        credential,
        HarnessCredential::ClaudeCode(ClaudeCredential::OauthToken {
            token: CLAUDE_ACCESS_TOKEN.to_owned()
        })
    );
    let stored = harness_accounts::stored(&db, &test_config(), user.id, HarnessKind::ClaudeCode)
        .await
        .expect("read the stored credential")
        .expect("the account is still linked");
    assert_eq!(stored.expires_at_unix(), Some(expires_at));
}

#[skyzen::test]
async fn a_refresh_token_anthropic_will_not_take_fails_loudly(_ctx: TestContext, db: Db) {
    crate::testing::migrate(&db).await;
    let user = seed_user(&db).await;
    // A grant whose refresh token is not the one the fixture will renew:
    // the account was revoked at Anthropic, and provisioning must say so
    // rather than hand a machine a token that cannot work.
    let credential = StoredCredential::ClaudeOauth {
        access_token: CLAUDE_ACCESS_TOKEN.to_owned(),
        refresh_token: "sk-ant-ort01-revoked".to_owned(),
        expires_at_unix: now_unix() + 60,
    };
    harness_accounts::store(
        &db,
        &test_config(),
        &test_vendors(),
        user.id,
        "lexo@lexo.cool",
        HarnessKind::ClaudeCode,
        &credential,
    )
    .await
    .expect("link the account");

    let error = harness_accounts::credential(
        &db,
        &test_config(),
        &test_vendors(),
        user.id,
        HarnessKind::ClaudeCode,
    )
    .await
    .expect_err("a revoked grant cannot be renewed");

    assert_eq!(
        error.problem().kind,
        "https://flyco.dev/problems/claude-oauth-rejected"
    );
}
