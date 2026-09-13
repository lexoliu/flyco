//! `flyco login`'s handshake: open, approve or deny, poll, revoke.

use flyco_core::{ApiKeySummary, CliSession, CliSessionKey, CreateCliSession, CurrentUser};
use skyzen::routing::Router;
use skyzen_services::{Db, Kv};
use skyzen_test::{TestClient, TestContext};

use crate::session;
use crate::testing::{migrated_router, seed_user};

/// A `POST /v1/cli-sessions` body naming the machine the CLI is on.
fn attempt() -> CreateCliSession {
    CreateCliSession {
        hostname: Some("builder.local".to_owned()),
    }
}

async fn client(ctx: &TestContext, db: &Db) -> TestClient<Router> {
    ctx.client(migrated_router(db).await)
}

async fn open(client: &TestClient<Router>) -> CliSession {
    let response = client
        .post("/v1/cli-sessions")
        .json(&attempt())
        .send()
        .await;
    response.assert_status(201);
    response.json()
}

/// A browser session token for `user`, as the approving PWA would hold.
async fn browser_token(kv: &Kv, user: &CurrentUser) -> String {
    session::issue(kv, user.id).await.expect("issue a session")
}

fn poll_path(attempt: &CliSession, token: &str) -> String {
    format!("/v1/cli-sessions/{}?s={token}", attempt.id)
}

#[skyzen::test]
async fn a_cli_sign_in_round_trip_issues_a_usable_key(ctx: TestContext, kv: Kv, db: Db) {
    let client = client(&ctx, &db).await;
    let user = seed_user(&db).await;
    let token = browser_token(&kv, &user).await;

    let attempt = open(&client).await;
    assert!(
        attempt
            .authorize_url
            .ends_with(&format!("/cli/authorize?id={}", attempt.id)),
        "the CLI prints what the user opens: {}",
        attempt.authorize_url
    );
    assert!(attempt.poll_token.starts_with("fc_"));

    // Still undecided: the poll waits.
    let pending = client
        .get(&poll_path(&attempt, &attempt.poll_token))
        .send()
        .await;
    pending.assert_status(202);

    // Approved under the signed-in user: the poll now delivers a key that
    // is theirs, labeled for the machine that asked.
    let approved = client
        .post(&format!("/v1/cli-sessions/{}/approve", attempt.id))
        .bearer(&token)
        .send()
        .await;
    approved.assert_status(204);

    let granted = client
        .get(&poll_path(&attempt, &attempt.poll_token))
        .send()
        .await;
    granted.assert_status(200);
    let key: CliSessionKey = granted.json();
    assert!(
        key.key.starts_with("fk_"),
        "an API key, not a session token"
    );

    let me = client.get("/v1/me").bearer(&key.key).send().await;
    me.assert_status(200);
    let me: CurrentUser = me.json();
    assert_eq!(me.id, user.id, "the minted key belongs to the approver");

    let label: Vec<ApiKeySummary> = client
        .get("/v1/api-keys")
        .bearer(&token)
        .send()
        .await
        .json();
    assert_eq!(
        label
            .iter()
            .find(|k| k.id == key.key_id)
            .map(|k| k.label.as_str()),
        Some("flyco-cli on builder.local")
    );
}

#[skyzen::test]
async fn a_collected_key_is_not_served_twice(ctx: TestContext, kv: Kv, db: Db) {
    let client = client(&ctx, &db).await;
    let user = seed_user(&db).await;
    let token = browser_token(&kv, &user).await;
    let attempt = open(&client).await;
    client
        .post(&format!("/v1/cli-sessions/{}/approve", attempt.id))
        .bearer(&token)
        .send()
        .await
        .assert_status(204);

    client
        .get(&poll_path(&attempt, &attempt.poll_token))
        .send()
        .await
        .assert_status(200);
    client
        .get(&poll_path(&attempt, &attempt.poll_token))
        .send()
        .await
        .assert_status(410);
}

#[skyzen::test]
async fn a_denied_sign_in_fails_the_poll(ctx: TestContext, kv: Kv, db: Db) {
    let client = client(&ctx, &db).await;
    let user = seed_user(&db).await;
    let token = browser_token(&kv, &user).await;
    let attempt = open(&client).await;

    client
        .post(&format!("/v1/cli-sessions/{}/deny", attempt.id))
        .bearer(&token)
        .send()
        .await
        .assert_status(204);
    client
        .get(&poll_path(&attempt, &attempt.poll_token))
        .send()
        .await
        .assert_status(403);
}

#[skyzen::test]
async fn a_decided_attempt_cannot_be_decided_again(ctx: TestContext, kv: Kv, db: Db) {
    let client = client(&ctx, &db).await;
    let user = seed_user(&db).await;
    let token = browser_token(&kv, &user).await;
    let attempt = open(&client).await;

    client
        .post(&format!("/v1/cli-sessions/{}/deny", attempt.id))
        .bearer(&token)
        .send()
        .await
        .assert_status(204);
    client
        .post(&format!("/v1/cli-sessions/{}/approve", attempt.id))
        .bearer(&token)
        .send()
        .await
        .assert_status(409);
}

#[skyzen::test]
async fn the_poll_token_is_the_polls_credential(ctx: TestContext, _kv: Kv, db: Db) {
    let client = client(&ctx, &db).await;
    let attempt = open(&client).await;

    client
        .get(&poll_path(&attempt, "fc_not-the-issued-token"))
        .send()
        .await
        .assert_status(403);
    client
        .get(&format!("/v1/cli-sessions/{}", attempt.id))
        .send()
        .await
        .assert_status(403);
}

#[skyzen::test]
async fn an_unknown_attempt_is_gone_to_everyone(ctx: TestContext, kv: Kv, db: Db) {
    let client = client(&ctx, &db).await;
    let user = seed_user(&db).await;
    let token = browser_token(&kv, &user).await;
    let id = flyco_core::CliSessionId::generate();

    client
        .get(&format!("/v1/cli-sessions/{id}?s=fc_anything"))
        .send()
        .await
        .assert_status(410);
    client
        .post(&format!("/v1/cli-sessions/{id}/approve"))
        .bearer(&token)
        .send()
        .await
        .assert_status(410);
}

#[skyzen::test]
async fn deciding_needs_a_signed_in_user(ctx: TestContext, _kv: Kv, db: Db) {
    let client = client(&ctx, &db).await;
    let attempt = open(&client).await;

    client
        .post(&format!("/v1/cli-sessions/{}/approve", attempt.id))
        .send()
        .await
        .assert_status(401);
}

#[skyzen::test]
async fn logout_revokes_the_minted_key(ctx: TestContext, kv: Kv, db: Db) {
    let client = client(&ctx, &db).await;
    let user = seed_user(&db).await;
    let token = browser_token(&kv, &user).await;
    let attempt = open(&client).await;
    client
        .post(&format!("/v1/cli-sessions/{}/approve", attempt.id))
        .bearer(&token)
        .send()
        .await
        .assert_status(204);
    let key: CliSessionKey = client
        .get(&poll_path(&attempt, &attempt.poll_token))
        .send()
        .await
        .json();

    client
        .delete(&format!("/v1/api-keys/{}", key.key_id))
        .bearer(&key.key)
        .send()
        .await
        .assert_status(204);
    client
        .get("/v1/me")
        .bearer(&key.key)
        .send()
        .await
        .assert_status(401);
}
