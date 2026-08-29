//! What the GitHub webhook route accepts, and — mostly — what it refuses.
//!
//! The route carries no flyco credential, so its whole security boundary is
//! the HMAC over the raw body. These tests sign real bodies with a real key
//! and then attack the signature the three ways that matter: a wrong secret,
//! a body edited after signing, and a deployment that holds no secret at
//! all. Each has to be a refusal *before* the payload is looked at, which is
//! why the malformed-payload case is only reachable once a signature is
//! valid.

use flyco_core::{ClientEvent, CurrentUser, Problem, SessionId, SessionState, UserId};
use hmac::{Hmac, Mac as _};
use sha2::Sha256;
use skyzen::routing::Router;
use skyzen_services::Db;
use skyzen_test::{TestClient, TestContext};

use crate::app::router;
use crate::github::GithubClient;
use crate::room::EventPage;
use crate::testing::{TestGithub, migrate, seed_user, test_config};
use crate::webhooks::{EVENT_HEADER, SIGNATURE_HEADER};

/// The secret a configured deployment shares with GitHub.
const SECRET: &str = "a-shared-webhook-secret";

/// A different one, which is what a forger has.
const WRONG_SECRET: &str = "not-the-shared-webhook-secret";

const PATH: &str = "/v1/webhooks/github";

const REPO: &str = "lexoliu/flyco";

const CHECK_RUN_FAILED: &str = include_str!("../../fixtures/github/check_run_failure.json");
const CHECK_RUN_PASSED: &str = include_str!("../../fixtures/github/check_run_success.json");
const WORKFLOW_RUN_FAILED: &str = include_str!("../../fixtures/github/workflow_run_failure.json");
const OTHER_REPO_FAILED: &str = include_str!("../../fixtures/github/check_run_other_repo.json");
const PING: &str = include_str!("../../fixtures/github/ping.json");

/// The router of a deployment that holds a webhook secret.
async fn configured_router(db: &Db) -> Router {
    migrate(db).await;
    router(
        test_config().with_github_webhook_secret(SECRET),
        GithubClient::Fake(TestGithub),
        db.clone(),
    )
}

/// The signature header GitHub would send for `body`, under `secret`.
fn signature(secret: &str, body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(body.as_bytes());

    let mut header = String::from("sha256=");
    header.push_str(&hex::encode(mac.finalize().into_bytes()));
    header
}

/// Delivers `body` as `event`, signed by `secret`.
async fn deliver(
    client: &TestClient<Router>,
    event: &str,
    body: &str,
    secret: &str,
) -> skyzen_test::TestResponse {
    client
        .post(PATH)
        .header(EVENT_HEADER, event)
        .header(SIGNATURE_HEADER, &signature(secret, body))
        .header("content-type", "application/json")
        .body(body.to_owned())
        .send()
        .await
}

/// Puts an active session on `repo`, which is the state a CI failure looks
/// for. The daemon's arrival is what makes a session active in production.
async fn active_session(db: &Db, user: &CurrentUser, repo: &str) -> SessionId {
    let session = crate::sessions::create(
        db,
        user.id,
        flyco_core::SESSION_CAP_MAX,
        flyco_core::HarnessKind::ClaudeCode,
        &repo.parse().expect("a valid repo slug"),
        flyco_core::BudgetConfig::new(flyco_core::Usd::from_dollars(10)).expect("a valid budget"),
    )
    .await
    .expect("open a session")
    .summary
    .id;

    crate::sessions::transition(db, user.id, session, SessionState::Active)
        .await
        .expect("activate the session");
    session
}

/// Everything the session's room recorded, which is where a forwarded user
/// message lands whether or not a daemon is attached.
async fn recorded(
    client: &TestClient<Router>,
    token: &str,
    session: SessionId,
) -> Vec<ClientEvent> {
    let response = client
        .get(&format!("/v1/sessions/{session}/events"))
        .bearer(token)
        .send()
        .await;
    response.assert_status(200);
    response
        .json::<EventPage>()
        .events
        .into_iter()
        .map(|stored| {
            serde_json::from_value(stored.event).expect("a room records ClientEvent documents")
        })
        .collect()
}

async fn sign_in(kv: &skyzen_services::Kv, user: UserId) -> String {
    crate::session::issue(kv, user).await.expect("issue")
}

#[skyzen::test]
async fn a_deployment_without_a_secret_accepts_no_webhook(ctx: TestContext, db: Db) {
    migrate(&db).await;
    // `test_config` carries no webhook secret, which is the state of any
    // deployment that has not been given one.
    let client = ctx.client(crate::testing::test_router(db.clone()));

    // Correctly signed by *a* secret — there is simply no secret here to
    // check it against, so it is refused rather than trusted.
    let response = deliver(&client, "ping", PING, SECRET).await;
    response.assert_status(501);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/webhooks-unconfigured"
    );
}

#[skyzen::test]
async fn a_correctly_signed_ping_is_accepted(ctx: TestContext, db: Db) {
    let client = ctx.client(configured_router(&db).await);
    deliver(&client, "ping", PING, SECRET)
        .await
        .assert_status(204);
}

#[skyzen::test]
async fn a_signature_from_another_secret_is_refused(ctx: TestContext, db: Db) {
    let client = ctx.client(configured_router(&db).await);

    let response = deliver(&client, "ping", PING, WRONG_SECRET).await;
    response.assert_status(403);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/webhook-unverified"
    );
}

#[skyzen::test]
async fn a_body_edited_after_signing_is_refused(ctx: TestContext, db: Db) {
    let client = ctx.client(configured_router(&db).await);

    // The signature is over the original bytes; the body that arrives names
    // somebody else's repository. This is the attack the HMAC exists for.
    let tampered = CHECK_RUN_FAILED.replace(REPO, "attacker/repo");
    assert_ne!(tampered, CHECK_RUN_FAILED);

    let response = client
        .post(PATH)
        .header(EVENT_HEADER, "check_run")
        .header(SIGNATURE_HEADER, &signature(SECRET, CHECK_RUN_FAILED))
        .header("content-type", "application/json")
        .body(tampered)
        .send()
        .await;

    response.assert_status(403);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/webhook-unverified"
    );
}

#[skyzen::test]
async fn an_unsigned_delivery_is_refused(ctx: TestContext, db: Db) {
    let client = ctx.client(configured_router(&db).await);

    let response = client
        .post(PATH)
        .header(EVENT_HEADER, "ping")
        .header("content-type", "application/json")
        .body(PING.to_owned())
        .send()
        .await;

    response.assert_status(403);
}

#[skyzen::test]
async fn a_failure_on_a_repository_nobody_is_working_on_is_accepted_quietly(
    ctx: TestContext,
    db: Db,
) {
    let client = ctx.client(configured_router(&db).await);
    let user = seed_user(&db).await;
    // A session exists, on another repository. The delivery matches nothing,
    // which is the normal case for a hook installed org-wide.
    active_session(&db, &user, REPO).await;

    deliver(&client, "check_run", OTHER_REPO_FAILED, SECRET)
        .await
        .assert_status(204);
}

#[skyzen::test]
async fn a_passing_run_wakes_nobody(ctx: TestContext, kv: skyzen_services::Kv, db: Db) {
    let client = ctx.client(configured_router(&db).await);
    let user = seed_user(&db).await;
    let token = sign_in(&kv, user.id).await;
    let session = active_session(&db, &user, REPO).await;

    deliver(&client, "check_run", CHECK_RUN_PASSED, SECRET)
        .await
        .assert_status(204);

    assert!(
        recorded(&client, &token, session).await.is_empty(),
        "a green build is not something to interrupt an agent with"
    );
}

#[skyzen::test]
async fn a_failing_check_run_wakes_the_session_on_that_repository(
    ctx: TestContext,
    kv: skyzen_services::Kv,
    db: Db,
) {
    let client = ctx.client(configured_router(&db).await);
    let user = seed_user(&db).await;
    let token = sign_in(&kv, user.id).await;
    let session = active_session(&db, &user, REPO).await;

    deliver(&client, "check_run", CHECK_RUN_FAILED, SECRET)
        .await
        .assert_status(204);

    let events = recorded(&client, &token, session).await;
    let [ClientEvent::UserMessage { text }] = events.as_slice() else {
        panic!("a failing check run must wake the session exactly once: {events:?}");
    };
    assert!(text.contains("[flyco CI notice]"));
    assert!(text.contains(REPO));
    assert!(text.contains("cargo clippy"), "the run's name is named");
    assert!(text.contains("feat/m6-webhook-usage"), "so is its branch");
    assert!(text.contains("push the fix"), "the notice is a goal");
}

#[skyzen::test]
async fn a_failing_workflow_run_wakes_it_the_same_way(
    ctx: TestContext,
    kv: skyzen_services::Kv,
    db: Db,
) {
    let client = ctx.client(configured_router(&db).await);
    let user = seed_user(&db).await;
    let token = sign_in(&kv, user.id).await;
    let session = active_session(&db, &user, REPO).await;

    deliver(&client, "workflow_run", WORKFLOW_RUN_FAILED, SECRET)
        .await
        .assert_status(204);

    let events = recorded(&client, &token, session).await;
    let [ClientEvent::UserMessage { text }] = events.as_slice() else {
        panic!("a failing workflow run must wake the session exactly once: {events:?}");
    };
    assert!(text.contains("a workflow run"));
    assert!(text.contains("CI"));
}

#[skyzen::test]
async fn a_session_that_is_not_active_is_left_alone(
    ctx: TestContext,
    kv: skyzen_services::Kv,
    db: Db,
) {
    let client = ctx.client(configured_router(&db).await);
    let user = seed_user(&db).await;
    let token = sign_in(&kv, user.id).await;
    let session = active_session(&db, &user, REPO).await;
    crate::sessions::transition(&db, user.id, session, SessionState::Archived)
        .await
        .expect("archive the session");

    deliver(&client, "check_run", CHECK_RUN_FAILED, SECRET)
        .await
        .assert_status(204);

    assert!(
        recorded(&client, &token, session).await.is_empty(),
        "an archived session has no daemon to hear a CI notice"
    );
}

#[skyzen::test]
async fn a_verified_delivery_that_is_not_its_event_is_a_bad_request(ctx: TestContext, db: Db) {
    let client = ctx.client(configured_router(&db).await);

    // Signed correctly, so this is only reachable once verification passed —
    // which is the point: the payload is read on the far side of the HMAC.
    let response = deliver(&client, "check_run", PING, SECRET).await;
    response.assert_status(400);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/webhook-malformed"
    );
}

#[skyzen::test]
async fn an_event_flyco_does_not_act_on_is_accepted(ctx: TestContext, db: Db) {
    let client = ctx.client(configured_router(&db).await);

    // A hook installed for `check_run` also delivers whatever else the user
    // ticked. Refusing would make GitHub redeliver it forever.
    deliver(&client, "push", PING, SECRET)
        .await
        .assert_status(204);
}
