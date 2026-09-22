//! Linking Codex by signing in with a `ChatGPT` subscription, end to end
//! through the router.
//!
//! The wire format of the three `OpenAI` calls is pinned separately, against
//! recorded exchanges, in [`crate::openai`]. What is asserted here is
//! everything around them: that the browser is handed a code and a link and
//! never the `device_auth_id`, that a poll is not a consumption, that the
//! attempt is bound to the user who started it, that a switched-off device
//! flow reaches the caller as a problem document with the fix in it, and
//! that the grant the exchange produced is sealed, dated, and refreshed
//! before a session gets it.

use flyco_core::{
    CodexOauthPending, CodexOauthStart, CurrentUser, HarnessAccountView, HarnessKind, Problem,
};
use flyco_provider::{CodexCredential, HarnessCredential};
use skyzen::routing::Router;
use skyzen_services::{Db, Kv, Queue};
use skyzen_test::mock::InMemoryQueue;
use skyzen_test::{TestClient, TestContext};

use crate::anthropic::ClaudeClient;
use crate::clock::now_unix;
use crate::devin::DevinClient;
use crate::google::GoogleClient;
use crate::harness_accounts::{self, REFRESH_WINDOW_SECONDS, StoredCredential};
use crate::microsoft::MicrosoftClient;
use crate::openai::CodexClient;
use crate::session;
use crate::testing::{
    CODEX_ACCESS_TOKEN, CODEX_ACCOUNT_EMAIL, CODEX_ACCOUNT_ID, CODEX_ID_TOKEN,
    CODEX_POLL_INTERVAL_SECONDS, CODEX_REFRESH_TOKEN, CODEX_RENEWED_ACCESS_TOKEN,
    CODEX_RENEWED_REFRESH_TOKEN, CODEX_RENEWED_TOKEN_EXPIRY, CODEX_TOKEN_EXPIRY, CODEX_USER_CODE,
    TestClaude, TestCodex, TestDevin, TestGithub, TestGoogle, TestMicrosoft, migrate,
    migrated_router, seed_codex_oauth_account, seed_other_user, seed_user, test_config,
    test_router_with, test_vendors,
};
use crate::vendors::Vendors;

const START: &str = "/v1/harness-accounts/codex/oauth/start";

/// Where a running attempt is polled.
fn poll_path(started: &CodexOauthStart) -> String {
    format!("/v1/harness-accounts/codex/oauth/{}", started.attempt_id)
}

/// A signed-in caller and the router they call, with `OpenAI` standing in
/// for whatever this test needs it to be.
async fn signed_in_with(
    ctx: &TestContext,
    kv: &Kv,
    db: &Db,
    codex: TestCodex,
) -> (TestClient<Router>, CurrentUser, String) {
    migrate(db).await;
    let router = test_router_with(
        db.clone(),
        Queue::new(InMemoryQueue::new()),
        TestGithub::default(),
        Vendors::new(
            ClaudeClient::Fake(TestClaude),
            CodexClient::Fake(codex),
            MicrosoftClient::Fake(TestMicrosoft::succeeding()),
            GoogleClient::Fake(TestGoogle::succeeding()),
            DevinClient::Fake(TestDevin),
        ),
    );
    let user = seed_user(db).await;
    let token = session::issue(kv, user.id).await.expect("issue a session");
    (ctx.client(router), user, token)
}

/// Begins a sign-in, returning what the card is given to show.
async fn begin(client: &TestClient<Router>, token: &str) -> CodexOauthStart {
    let response = client.post(START).bearer(token).send().await;
    response.assert_status(200);
    response.json()
}

#[skyzen::test]
async fn a_sign_in_starts_with_the_two_things_codex_login_prints(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in_with(&ctx, &kv, &db, TestCodex::pending()).await;
    let started = begin(&client, &token).await;

    assert_eq!(started.user_code, CODEX_USER_CODE);
    assert_eq!(
        started.verification_url,
        "https://auth.openai.com/codex/device"
    );
    assert_eq!(started.interval_seconds, CODEX_POLL_INTERVAL_SECONDS);
}

#[skyzen::test]
async fn the_browser_never_receives_what_would_redeem_the_grant(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in_with(&ctx, &kv, &db, TestCodex::pending()).await;
    let response = client.post(START).bearer(&token).send().await;
    response.assert_status(200);

    let body = response.body_text();
    assert!(
        !body.contains("device_auth_id"),
        "the id that redeems the grant must stay in the control plane: {body}"
    );
}

#[skyzen::test]
async fn a_poll_before_approval_says_pending_and_keeps_the_attempt(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, _user, token) = signed_in_with(&ctx, &kv, &db, TestCodex::pending()).await;
    let started = begin(&client, &token).await;

    for _ in 0..3 {
        let response = client.get(&poll_path(&started)).bearer(&token).send().await;
        response.assert_status(200);
        assert_eq!(
            response.json::<CodexOauthPending>(),
            CodexOauthPending::Pending
        );
    }
}

#[skyzen::test]
async fn an_approved_code_links_the_account_the_id_token_names(ctx: TestContext, kv: Kv, db: Db) {
    let (client, user, token) = signed_in_with(&ctx, &kv, &db, TestCodex::approved()).await;
    let started = begin(&client, &token).await;

    let response = client.get(&poll_path(&started)).bearer(&token).send().await;
    response.assert_status(201);
    let view: HarnessAccountView = response.json();
    assert_eq!(view.harness, HarnessKind::Codex);
    assert_eq!(view.label, CODEX_ACCOUNT_EMAIL);
    assert_eq!(view.expires_at_unix, Some(CODEX_TOKEN_EXPIRY));

    let stored = harness_accounts::stored(&db, &test_config(), user.id, HarnessKind::Codex)
        .await
        .expect("read the stored credential")
        .expect("the account is linked");
    assert!(matches!(
        stored,
        StoredCredential::CodexOauth {
            id_token,
            access_token,
            refresh_token,
            account_id,
            expires_at_unix,
        } if id_token == CODEX_ID_TOKEN
            && access_token == CODEX_ACCESS_TOKEN
            && refresh_token == CODEX_REFRESH_TOKEN
            && account_id == CODEX_ACCOUNT_ID
            && expires_at_unix == CODEX_TOKEN_EXPIRY
    ));
}

#[skyzen::test]
async fn the_grant_is_sealed_before_the_database_sees_it(ctx: TestContext, kv: Kv, db: Db) {
    let (client, user, token) = signed_in_with(&ctx, &kv, &db, TestCodex::approved()).await;
    let started = begin(&client, &token).await;
    client
        .get(&poll_path(&started))
        .bearer(&token)
        .send()
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
    assert!(!sealed.contains(CODEX_REFRESH_TOKEN));
    assert!(!sealed.contains(CODEX_ACCESS_TOKEN));
}

#[skyzen::test]
async fn an_approved_attempt_is_good_once(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in_with(&ctx, &kv, &db, TestCodex::approved()).await;
    let started = begin(&client, &token).await;

    client
        .get(&poll_path(&started))
        .bearer(&token)
        .send()
        .await
        .assert_status(201);

    let again = client.get(&poll_path(&started)).bearer(&token).send().await;
    again.assert_status(400);
    assert_eq!(
        again.json::<Problem>().kind,
        "https://flyco.dev/problems/codex-oauth-attempt-expired"
    );
}

#[skyzen::test]
async fn one_users_attempt_is_not_another_users_to_finish(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in_with(&ctx, &kv, &db, TestCodex::approved()).await;
    let started = begin(&client, &token).await;

    let other = seed_other_user(&db).await;
    let other_token = session::issue(&kv, other.id)
        .await
        .expect("issue a session for the other user");

    let stolen = client
        .get(&poll_path(&started))
        .bearer(&other_token)
        .send()
        .await;
    stolen.assert_status(400);
    assert_eq!(
        stolen.json::<Problem>().kind,
        "https://flyco.dev/problems/codex-oauth-attempt-expired"
    );

    // And the real owner can still finish it: the refusal spent nothing.
    client
        .get(&poll_path(&started))
        .bearer(&token)
        .send()
        .await
        .assert_status(201);
}

#[skyzen::test]
async fn a_switched_off_device_flow_says_where_the_switch_is(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) =
        signed_in_with(&ctx, &kv, &db, TestCodex::device_auth_disabled()).await;

    let response = client.post(START).bearer(&token).send().await;
    response.assert_status(409);
    let problem: Problem = response.json();
    assert_eq!(
        problem.kind,
        "https://flyco.dev/problems/codex-device-auth-disabled"
    );
    assert!(
        problem
            .detail
            .contains("https://chatgpt.com/#settings/Security"),
        "the problem carries the page the user has to open: {}",
        problem.detail
    );
}

#[skyzen::test]
async fn a_grant_openai_refuses_reaches_the_caller_as_its_own_reason(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, _user, token) = signed_in_with(&ctx, &kv, &db, TestCodex::refused()).await;
    let started = begin(&client, &token).await;

    let response = client.get(&poll_path(&started)).bearer(&token).send().await;
    response.assert_status(422);
    let problem: Problem = response.json();
    assert_eq!(
        problem.kind,
        "https://flyco.dev/problems/codex-oauth-rejected"
    );
    assert!(problem.detail.contains("invalid_grant"));
}

#[skyzen::test]
async fn an_unknown_attempt_is_indistinguishable_from_an_expired_one(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let router = migrated_router(&db).await;
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    let client = ctx.client(router);

    let response = client
        .get("/v1/harness-accounts/codex/oauth/11111111-2222-4333-8444-555555555555")
        .bearer(&token)
        .send()
        .await;
    response.assert_status(400);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/codex-oauth-attempt-expired"
    );
}

// ── Refresh on use ──

#[skyzen::test]
async fn a_chatgpt_grant_near_its_end_is_renewed_before_a_session_gets_it(
    _ctx: TestContext,
    db: Db,
) {
    migrate(&db).await;
    let user = seed_user(&db).await;
    seed_codex_oauth_account(&db, user.id, now_unix() + 60).await;

    let credential = harness_accounts::credential(
        &db,
        &test_config(),
        &test_vendors(),
        user.id,
        HarnessKind::Codex,
    )
    .await
    .expect("unseal the credential");

    assert_eq!(
        credential,
        HarnessCredential::Codex(CodexCredential::ChatGpt {
            id_token: CODEX_ID_TOKEN.to_owned(),
            access_token: CODEX_RENEWED_ACCESS_TOKEN.to_owned(),
            refresh_token: CODEX_RENEWED_REFRESH_TOKEN.to_owned(),
            account_id: CODEX_ACCOUNT_ID.to_owned(),
        }),
        "the daemon writes auth.json from the set the refresh produced"
    );

    let stored = harness_accounts::stored(&db, &test_config(), user.id, HarnessKind::Codex)
        .await
        .expect("read the stored credential")
        .expect("the account is still linked");
    assert!(matches!(
        stored,
        StoredCredential::CodexOauth {
            refresh_token,
            expires_at_unix,
            ..
        } if refresh_token == CODEX_RENEWED_REFRESH_TOKEN
            && expires_at_unix == CODEX_RENEWED_TOKEN_EXPIRY
    ));
}

#[skyzen::test]
async fn a_chatgpt_grant_with_time_left_is_handed_over_untouched(_ctx: TestContext, db: Db) {
    migrate(&db).await;
    let user = seed_user(&db).await;
    let expires_at = now_unix() + REFRESH_WINDOW_SECONDS + 3_600;
    seed_codex_oauth_account(&db, user.id, expires_at).await;

    let credential = harness_accounts::credential(
        &db,
        &test_config(),
        &test_vendors(),
        user.id,
        HarnessKind::Codex,
    )
    .await
    .expect("unseal the credential");

    assert_eq!(
        credential,
        HarnessCredential::Codex(CodexCredential::ChatGpt {
            id_token: CODEX_ID_TOKEN.to_owned(),
            access_token: CODEX_ACCESS_TOKEN.to_owned(),
            refresh_token: CODEX_REFRESH_TOKEN.to_owned(),
            account_id: CODEX_ACCOUNT_ID.to_owned(),
        })
    );
    let stored = harness_accounts::stored(&db, &test_config(), user.id, HarnessKind::Codex)
        .await
        .expect("read the stored credential")
        .expect("the account is still linked");
    assert_eq!(stored.expires_at_unix(), Some(expires_at));
}

#[skyzen::test]
async fn a_refresh_token_openai_will_not_take_fails_loudly(_ctx: TestContext, db: Db) {
    migrate(&db).await;
    let user = seed_user(&db).await;
    // A grant whose refresh token is not the one the fixture will renew:
    // the account was revoked at OpenAI, and provisioning must say so
    // rather than hand a machine a token that cannot work.
    let credential = StoredCredential::CodexOauth {
        id_token: CODEX_ID_TOKEN.to_owned(),
        access_token: CODEX_ACCESS_TOKEN.to_owned(),
        refresh_token: "rt_flyco-test-revoked".to_owned(),
        account_id: CODEX_ACCOUNT_ID.to_owned(),
        expires_at_unix: now_unix() + 60,
    };
    harness_accounts::store(
        &db,
        &test_config(),
        &test_vendors(),
        user.id,
        CODEX_ACCOUNT_EMAIL,
        HarnessKind::Codex,
        &credential,
    )
    .await
    .expect("link the account");

    let error = harness_accounts::credential(
        &db,
        &test_config(),
        &test_vendors(),
        user.id,
        HarnessKind::Codex,
    )
    .await
    .expect_err("a revoked grant cannot be renewed");

    assert_eq!(
        error.problem().kind,
        "https://flyco.dev/problems/codex-oauth-rejected"
    );
}
