//! The Codespaces link and the bootstrap a running codespace calls, end to
//! end through the router.
//!
//! Two contracts are covered here rather than in [`crate::tests::provider_oauth`]
//! with the other sign-ins: the link's *finish* is different work (it creates
//! the environment repository rather than naming a choice), and the bootstrap
//! endpoint is a public route with an authentication of its own — the
//! `GITHUB_TOKEN`/`CODESPACE_NAME` pair GitHub injects, not a flyco credential.

use flyco_core::{
    CloudProviderKind, CodespacesBootstrap, FinishCodespacesOauth, InterruptedReason, MachineId,
    MachineSpec, MachineState, Problem, ProviderAccountView, ProviderCredentials,
    ProviderOauthProgress, ProviderOauthStart, SessionId, SessionState,
};
use skyzen::routing::Router;
use skyzen::sql;
use skyzen_services::{Db, Kv, Queue};
use skyzen_test::mock::InMemoryQueue;
use skyzen_test::{TestClient, TestContext, TestResponse};
use url::Url;

use crate::anthropic::ClaudeClient;
use crate::google::GoogleClient;
use crate::microsoft::MicrosoftClient;
use crate::openai::CodexClient;
use crate::provider_accounts::StoredSecrets;
use crate::session;
use crate::testing::{
    CLIENT_ID, CLOUD_CODE, CODESPACES_ENV_REPO, GITHUB_ACCESS_TOKEN, GITHUB_ID, GITHUB_LOGIN,
    TestClaude, TestCodespaces, TestCodex, TestGithub, TestGoogle, TestMicrosoft, migrate,
    seed_codespaces_account, seed_session, seed_user, test_config, test_rooms, test_router_full,
};
use crate::vendors::Vendors;
use crate::{machines, sessions};

const START: &str = "/v1/providers/codespaces/oauth/start";
const BOOTSTRAP: &str = "/v1/providers/codespaces/bootstrap";

/// The name GitHub generates for a codespace, which `CODESPACE_NAME`
/// reports inside it.
const CODESPACE_NAME: &str = "organic-space-pancake-5j2q4j9q7c9x4w";

/// What the provision sealed onto the machine row — asserted back verbatim,
/// because the endpoint's whole job is to return it untouched.
const CONFIG_TOML: &str = "[session]\nid = \"a-session\"\n";

/// A signed-in caller against a router whose GitHub grants the codespace
/// scopes, with the Codespaces link seam the caller chose.
async fn signed_in_with(
    ctx: &TestContext,
    kv: &Kv,
    db: &Db,
    github: TestGithub,
    codespaces: TestCodespaces,
) -> (TestClient<Router>, flyco_core::CurrentUser, String) {
    migrate(db).await;
    let router = test_router_full(
        db.clone(),
        Queue::new(InMemoryQueue::new()),
        github,
        Vendors::new(
            ClaudeClient::Fake(TestClaude),
            CodexClient::Fake(TestCodex::approved()),
            MicrosoftClient::Fake(TestMicrosoft::succeeding()),
            GoogleClient::Fake(TestGoogle::succeeding()),
        ),
        codespaces,
    );
    let user = seed_user(db).await;
    let token = session::issue(kv, user.id).await.expect("issue a session");
    (ctx.client(router), user, token)
}

/// The same, with GitHub granting everything a link needs.
async fn signed_in(
    ctx: &TestContext,
    kv: &Kv,
    db: &Db,
) -> (TestClient<Router>, flyco_core::CurrentUser, String) {
    signed_in_with(
        ctx,
        kv,
        db,
        TestGithub::codespaces_authorized(),
        TestCodespaces::succeeding(),
    )
    .await
}

/// Begins a sign-in and hands back what the page was given.
async fn begin(client: &TestClient<Router>, token: &str) -> ProviderOauthStart {
    let response = client.post(START).bearer(token).send().await;
    response.assert_status(200);
    response.json()
}

/// The `state` an authorize URL published.
fn state_of(started: &ProviderOauthStart) -> String {
    Url::parse(&started.authorize_url)
        .expect("the authorize URL is absolute")
        .query_pairs()
        .find_map(|(key, value)| (key == "state").then(|| value.into_owned()))
        .expect("the authorize URL carries a state")
}

/// Where a `303` sent the browser.
fn location(response: &TestResponse) -> Url {
    let raw = response
        .headers()
        .get("location")
        .expect("the response redirects")
        .to_str()
        .expect("the location is ASCII");
    Url::parse(raw).expect("the location is an absolute URL")
}

/// One query parameter of a URL.
fn query(url: &Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
}

fn poll_path(started: &ProviderOauthStart) -> String {
    format!("/v1/providers/codespaces/oauth/{}", started.attempt_id)
}

fn finish_path(started: &ProviderOauthStart) -> String {
    format!(
        "/v1/providers/codespaces/oauth/{}/finish",
        started.attempt_id
    )
}

fn callback_path(code: &str, state: &str) -> String {
    // The shared GitHub callback — the OAuth app registers one URI per
    // hostname and the `state` decides which flow is coming back.
    format!("/v1/auth/github/callback?code={code}&state={state}")
}

// ── The sign-in ──

#[skyzen::test]
async fn a_codespaces_sign_in_starts_at_github_asking_for_the_codespace_scope(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token).await;

    let url = Url::parse(&started.authorize_url).expect("the authorize URL is absolute");
    assert_eq!(url.host_str(), Some("github.com"));
    assert_eq!(url.path(), "/login/oauth/authorize");
    assert_eq!(
        query(&url, "client_id").as_deref(),
        Some(CLIENT_ID),
        "the same OAuth app as flyco's own sign-in"
    );
    assert_eq!(
        query(&url, "redirect_uri").as_deref(),
        Some("https://flyco.test/v1/auth/github/callback"),
        "the sign-in's own callback — the OAuth app registers one URI per hostname"
    );

    let scope = query(&url, "scope").expect("the URL names its scopes");
    for needed in ["repo", "codespace", "read:packages"] {
        assert!(scope.contains(needed), "the link needs `{needed}`: {scope}");
    }
    assert!(!state_of(&started).is_empty());
}

#[skyzen::test]
async fn a_codespaces_sign_in_reports_the_account_and_nothing_to_choose(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token).await;

    client
        .get(&callback_path(CLOUD_CODE, &state_of(&started)))
        .send()
        .await
        .assert_status(303);

    let polled = client.get(&poll_path(&started)).bearer(&token).send().await;
    polled.assert_status(200);
    assert_eq!(
        polled.json::<ProviderOauthProgress>(),
        ProviderOauthProgress::Authorized {
            account: GITHUB_LOGIN.to_owned(),
            // Nothing to choose: flyco creates the one repository it needs.
            choices: Vec::new(),
        }
    );
}

#[skyzen::test]
async fn a_sign_in_that_declined_the_codespace_scope_is_told_so(ctx: TestContext, kv: Kv, db: Db) {
    // A token from flyco's own sign-in: `repo`, but not `codespace`.
    let (client, _user, token) = signed_in_with(
        &ctx,
        &kv,
        &db,
        TestGithub::default(),
        TestCodespaces::succeeding(),
    )
    .await;
    let started = begin(&client, &token).await;

    let response = client
        .get(&callback_path(CLOUD_CODE, &state_of(&started)))
        .send()
        .await;
    response.assert_status(303);
    let landed = location(&response);
    assert_eq!(landed.path(), "/connect/return");
    assert_eq!(query(&landed, "provider").as_deref(), Some("codespaces"));
    assert_eq!(
        query(&landed, "problem").as_deref(),
        Some("github-rejected")
    );

    let polled = client.get(&poll_path(&started)).bearer(&token).send().await;
    match polled.json::<ProviderOauthProgress>() {
        ProviderOauthProgress::Failed { problem, reason } => {
            assert_eq!(problem, "github-rejected");
            assert!(reason.contains("codespace"), "reason: {reason}");
        }
        other => panic!("expected the poll to report the failure, got {other:?}"),
    }
}

// ── Finishing ──

#[skyzen::test]
async fn finishing_links_the_account_the_repository_was_ensured_on(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token).await;
    client
        .get(&callback_path(CLOUD_CODE, &state_of(&started)))
        .send()
        .await
        .assert_status(303);

    let response = client
        .post(&finish_path(&started))
        .bearer(&token)
        .json(&FinishCodespacesOauth {})
        .send()
        .await;

    response.assert_status(201);
    let view: ProviderAccountView = response.json();
    assert_eq!(view.kind, CloudProviderKind::Codespaces);
    assert_eq!(
        view.label, GITHUB_LOGIN,
        "the card is named after the one account this credential acts as"
    );

    let listed = client.get("/v1/providers").bearer(&token).send().await;
    listed.assert_status(200);
    let accounts: Vec<ProviderAccountView> = listed.json();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].id, view.id);

    // The sealed credential names the repository the finish prepared, and
    // the token inside it never reaches the database in the clear.
    let user_id = user.id;
    let sealed: String = sql!(
        db,
        "SELECT credentials_enc FROM provider_accounts WHERE user_id = {user_id}"
    )
    .fetch_scalar()
    .await
    .expect("read the sealed credential");
    assert!(!sealed.contains(GITHUB_ACCESS_TOKEN));

    let plain = test_config()
        .token_cipher()
        .open(&sealed)
        .expect("the credential opens under this deployment's key");
    let stored: StoredSecrets =
        serde_json::from_str(&plain).expect("the sealed document is stored secrets");
    let ProviderCredentials::Codespaces {
        env_repo,
        included_core_hours,
        ..
    } = &stored.credentials
    else {
        panic!("a Codespaces link stored {:?}", stored.credentials.kind());
    };
    assert_eq!(env_repo, CODESPACES_ENV_REPO);
    assert_eq!(*included_core_hours, 180, "the fixture account is on Pro");

    // And the sign-in is spent.
    client
        .get(&poll_path(&started))
        .bearer(&token)
        .send()
        .await
        .assert_status(404);
}

#[skyzen::test]
async fn a_finish_github_refuses_links_nothing(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in_with(
        &ctx,
        &kv,
        &db,
        TestGithub::codespaces_authorized(),
        // An existing *public* flyco-sessions is the refusal that matters:
        // writing sessions into it would hand their bootstrap to anyone.
        TestCodespaces::refusing(),
    )
    .await;
    let started = begin(&client, &token).await;
    client
        .get(&callback_path(CLOUD_CODE, &state_of(&started)))
        .send()
        .await
        .assert_status(303);

    let response = client
        .post(&finish_path(&started))
        .bearer(&token)
        .json(&FinishCodespacesOauth {})
        .send()
        .await;
    response.assert_status(422);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/provider-rejected-credentials"
    );

    let listed = client.get("/v1/providers").bearer(&token).send().await;
    assert!(
        listed.json::<Vec<ProviderAccountView>>().is_empty(),
        "a refused environment is not a linked account"
    );
}

#[skyzen::test]
async fn finishing_before_the_browser_comes_back_is_refused(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token).await;

    let response = client
        .post(&finish_path(&started))
        .bearer(&token)
        .json(&FinishCodespacesOauth {})
        .send()
        .await;
    response.assert_status(409);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/provider-oauth-not-authorized"
    );
}

// ── Linking from the sign-in grant ──

const LINK: &str = "/v1/providers/codespaces/link";

#[skyzen::test]
async fn a_sign_in_grant_carrying_the_scope_links_without_an_attempt(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, user, token) = signed_in(&ctx, &kv, &db).await;

    let response = client.post(LINK).bearer(&token).send().await;
    response.assert_status(201);
    let view: ProviderAccountView = response.json();
    assert_eq!(view.kind, CloudProviderKind::Codespaces);
    assert_eq!(view.label, GITHUB_LOGIN);

    // The sealed credential is the sign-in grant itself — the same token,
    // refresh and expiry the OAuth finish would have stored from an attempt.
    let user_id = user.id;
    let sealed: String = sql!(
        db,
        "SELECT credentials_enc FROM provider_accounts WHERE user_id = {user_id}"
    )
    .fetch_scalar()
    .await
    .expect("read the sealed credential");
    assert!(!sealed.contains(GITHUB_ACCESS_TOKEN));
    let plain = test_config()
        .token_cipher()
        .open(&sealed)
        .expect("the credential opens under this deployment's key");
    let stored: StoredSecrets =
        serde_json::from_str(&plain).expect("the sealed document is stored secrets");
    let ProviderCredentials::Codespaces {
        token: stored_token,
        env_repo,
        owner_id,
        included_core_hours,
        ..
    } = &stored.credentials
    else {
        panic!("a Codespaces link stored {:?}", stored.credentials.kind());
    };
    assert_eq!(stored_token, GITHUB_ACCESS_TOKEN);
    assert_eq!(env_repo, CODESPACES_ENV_REPO);
    assert_eq!(*owner_id, GITHUB_ID);
    assert_eq!(*included_core_hours, 180, "the fixture account is on Pro");
}

#[skyzen::test]
async fn a_grant_without_the_scope_is_sent_to_the_oauth_flow(ctx: TestContext, kv: Kv, db: Db) {
    // A sign-in grant written before flyco asked for `codespace`.
    let (client, _user, token) = signed_in_with(
        &ctx,
        &kv,
        &db,
        TestGithub::default(),
        TestCodespaces::succeeding(),
    )
    .await;

    let response = client.post(LINK).bearer(&token).send().await;
    response.assert_status(403);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/github-scope-missing"
    );

    let listed = client.get("/v1/providers").bearer(&token).send().await;
    assert!(
        listed.json::<Vec<ProviderAccountView>>().is_empty(),
        "a grant that cannot drive a codespace links nothing"
    );
}

#[skyzen::test]
async fn a_direct_link_github_refuses_links_nothing(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in_with(
        &ctx,
        &kv,
        &db,
        TestGithub::codespaces_authorized(),
        // The public flyco-sessions refusal again: the environment is where
        // a credential's honesty is proven, whichever door it came through.
        TestCodespaces::refusing(),
    )
    .await;

    let response = client.post(LINK).bearer(&token).send().await;
    response.assert_status(422);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/provider-rejected-credentials"
    );
}

// ── The bootstrap a running codespace calls ──

/// Writes a codespaces machine the way a provision leaves it: the row the
/// claim reserved, the name GitHub gave it, and the sealed configuration —
/// or not, for the leg that has not stored one yet.
async fn seed_codespace(db: &Db, with_bootstrap: bool) {
    let user = seed_user(db).await;
    let account = seed_codespaces_account(db, user.id).await;
    let session = seed_session(db, &user).await;
    let machine = crate::machines::reserve(
        db,
        session,
        account,
        &MachineSpec {
            provider: CloudProviderKind::Codespaces,
            machine_type: "basicLinux32gb".to_owned(),
            runtime: flyco_core::Runtime::Vm,
            region: "EuropeWest".to_owned(),
            spot: false,
            disk_gib: flyco_core::DEFAULT_DISK_GIB,
        },
    )
    .await
    .expect("reserve the machine row");

    let name = CODESPACE_NAME;
    sql!(
        db,
        "UPDATE machines SET native_id = {name} WHERE id = {machine}"
    )
    .execute()
    .await
    .expect("record the codespace's name");

    if with_bootstrap {
        let sealed = test_config()
            .token_cipher()
            .seal(CONFIG_TOML)
            .expect("seal the configuration");
        crate::machines::store_bootstrap(db, machine, &sealed)
            .await
            .expect("store the bootstrap");
    }
}

#[skyzen::test]
async fn a_codespace_fetches_the_configuration_its_provision_stored(
    ctx: TestContext,
    _kv: Kv,
    db: Db,
) {
    migrate(&db).await;
    seed_codespace(&db, true).await;
    let client = ctx.client(crate::testing::test_router(
        db.clone(),
        Queue::new(InMemoryQueue::new()),
    ));

    let response = client
        .post(BOOTSTRAP)
        .bearer("the-injected-github-token")
        .json(&flyco_core::CodespacesBootstrapRequest {
            codespace_name: CODESPACE_NAME.to_owned(),
        })
        .send()
        .await;

    response.assert_status(200);
    assert_eq!(
        response.json::<CodespacesBootstrap>().config_toml,
        CONFIG_TOML,
        "the document comes back exactly as the provision sealed it"
    );
}

#[skyzen::test]
async fn a_token_that_cannot_read_the_environment_repository_is_denied(
    ctx: TestContext,
    _kv: Kv,
    db: Db,
) {
    migrate(&db).await;
    seed_codespace(&db, true).await;
    // GitHub answers 404 for a repository this token cannot read — the
    // shape a foreign codespace's token, or a user's own PAT, takes.
    let client = ctx.client(crate::testing::test_router_full(
        db.clone(),
        Queue::new(InMemoryQueue::new()),
        TestGithub {
            unreadable: Some(CODESPACES_ENV_REPO),
            ..TestGithub::codespaces_authorized()
        },
        crate::testing::test_vendors(),
        TestCodespaces::succeeding(),
    ));

    let response = client
        .post(BOOTSTRAP)
        .bearer("a-token-that-is-not-this-codespaces")
        .json(&flyco_core::CodespacesBootstrapRequest {
            codespace_name: CODESPACE_NAME.to_owned(),
        })
        .send()
        .await;

    response.assert_status(403);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/codespaces-bootstrap-denied"
    );
}

#[skyzen::test]
async fn a_name_nothing_claims_is_a_not_found(ctx: TestContext, _kv: Kv, db: Db) {
    migrate(&db).await;
    seed_codespace(&db, true).await;
    let client = ctx.client(crate::testing::test_router(
        db.clone(),
        Queue::new(InMemoryQueue::new()),
    ));

    let response = client
        .post(BOOTSTRAP)
        .bearer("the-injected-github-token")
        .json(&flyco_core::CodespacesBootstrapRequest {
            codespace_name: "some-other-codespace-1a2b3c4d5e6f7g".to_owned(),
        })
        .send()
        .await;

    response.assert_status(404);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/codespaces-machine-unknown"
    );
}

#[skyzen::test]
async fn a_codespace_that_asks_before_the_provision_stores_is_told_to_wait(
    ctx: TestContext,
    _kv: Kv,
    db: Db,
) {
    migrate(&db).await;
    // The name is recorded but bootstrap_enc is still NULL — the window
    // the entrypoint's own retry is written for.
    seed_codespace(&db, false).await;
    let client = ctx.client(crate::testing::test_router(
        db.clone(),
        Queue::new(InMemoryQueue::new()),
    ));

    let response = client
        .post(BOOTSTRAP)
        .bearer("the-injected-github-token")
        .json(&flyco_core::CodespacesBootstrapRequest {
            codespace_name: CODESPACE_NAME.to_owned(),
        })
        .send()
        .await;

    response.assert_status(409);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/machine-not-ready"
    );
}

#[skyzen::test]
async fn a_bootstrap_without_a_token_is_a_challenge(ctx: TestContext, _kv: Kv, db: Db) {
    migrate(&db).await;
    let client = ctx.client(crate::testing::test_router(
        db.clone(),
        Queue::new(InMemoryQueue::new()),
    ));

    let response = client
        .post(BOOTSTRAP)
        .json(&flyco_core::CodespacesBootstrapRequest {
            codespace_name: CODESPACE_NAME.to_owned(),
        })
        .send()
        .await;

    response.assert_status(401);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/missing-credential"
    );
}

// ── The reconcile sweep ──

/// A codespace machine flyco believes it holds, with the session on it in
/// the state the caller chose.
///
/// The machine's billing columns are deliberately unset: the writes under
/// test are the state moves, and metering's own tests cover the cursors.
async fn held_codespace(
    db: &Db,
    session_state: SessionState,
    machine_state: MachineState,
) -> (SessionId, MachineId) {
    let user = seed_user(db).await;
    let account = seed_codespaces_account(db, user.id).await;
    let session = seed_session(db, &user).await;
    let machine = machines::reserve(
        db,
        session,
        account,
        &MachineSpec {
            provider: CloudProviderKind::Codespaces,
            machine_type: "basicLinux32gb".to_owned(),
            runtime: flyco_core::Runtime::Vm,
            region: "EuropeWest".to_owned(),
            spot: false,
            disk_gib: flyco_core::DEFAULT_DISK_GIB,
        },
    )
    .await
    .expect("reserve the machine row");
    let name = CODESPACE_NAME;
    sql!(
        db,
        "UPDATE machines SET native_id = {name}, state = {machine_state} WHERE id = {machine}"
    )
    .execute()
    .await
    .expect("record the codespace's name and state");

    match session_state {
        SessionState::Provisioning => {}
        SessionState::Active => {
            sessions::daemon_arrived(db, &test_rooms(), session)
                .await
                .expect("the daemon arrived");
        }
        SessionState::Interrupted | SessionState::Paused => {
            sessions::daemon_arrived(db, &test_rooms(), session)
                .await
                .expect("the daemon arrived");
            match session_state {
                SessionState::Interrupted => {
                    sessions::interrupted(db, session, InterruptedReason::Suspended)
                        .await
                        .expect("the session is interrupted");
                }
                SessionState::Paused => {
                    sessions::pause_for_budget(db, session)
                        .await
                        .expect("the session is paused");
                }
                _ => unreachable!(),
            }
        }
        _ => panic!("the held set never contains a {session_state:?} session"),
    }
    (session, machine)
}

/// Runs the sweep against the GitHub the caller chose.
async fn reconcile(db: &Db, github: TestCodespaces) {
    crate::codespaces::reconcile(
        db,
        &test_config(),
        &TestGithub::default(),
        &test_rooms(),
        &crate::codespaces::Codespaces::Fake(github),
    )
    .await
    .expect("the sweep runs");
}

/// The session's own words for where it is.
async fn session_state(db: &Db, session: SessionId) -> (SessionState, Option<InterruptedReason>) {
    let state: SessionState = sql!(db, "SELECT state FROM sessions WHERE id = {session}")
        .fetch_scalar()
        .await
        .expect("read the session");
    let reason: Option<InterruptedReason> = sql!(
        db,
        "SELECT interrupted_reason FROM sessions WHERE id = {session}"
    )
    .fetch_scalar()
    .await
    .expect("read the session's reason");
    (state, reason)
}

#[skyzen::test]
async fn a_codespace_github_suspended_interrupts_its_session(ctx: TestContext, _kv: Kv, db: Db) {
    let _ = ctx;
    migrate(&db).await;
    let (session, machine) = held_codespace(&db, SessionState::Active, MachineState::Running).await;

    reconcile(
        &db,
        TestCodespaces {
            reported_state: Some("Shutdown"),
            ..TestCodespaces::succeeding()
        },
    )
    .await;

    let row = machines::find(&db, machine)
        .await
        .expect("read the machine")
        .expect("the row is still there");
    assert_eq!(row.state, MachineState::Deallocated);
    assert_eq!(
        row.native_id.as_deref(),
        Some(CODESPACE_NAME),
        "a suspended codespace keeps its name: it is started again by it"
    );
    assert_eq!(
        session_state(&db, session).await,
        (
            SessionState::Interrupted,
            Some(InterruptedReason::Suspended)
        ),
        "the session reads `Interrupted · suspended`, waiting to be spoken to"
    );
}

#[skyzen::test]
async fn a_deleted_codespace_marks_its_session_machine_lost(ctx: TestContext, _kv: Kv, db: Db) {
    let _ = ctx;
    migrate(&db).await;
    let (session, machine) = held_codespace(&db, SessionState::Active, MachineState::Running).await;

    reconcile(
        &db,
        TestCodespaces {
            reported_state: None,
            ..TestCodespaces::succeeding()
        },
    )
    .await;

    let row = machines::find(&db, machine)
        .await
        .expect("read the machine")
        .expect("the row is still there");
    assert_eq!(row.state, MachineState::Destroyed);
    assert_eq!(
        row.native_id, None,
        "the name is cleared: the next resume provisions rather than starting a 404"
    );
    assert_eq!(
        session_state(&db, session).await,
        (
            SessionState::Interrupted,
            Some(InterruptedReason::MachineLost)
        ),
        "suspended was true when it was written; it is not true now"
    );
}

#[skyzen::test]
async fn a_failed_codespace_is_deleted_before_its_row_is_released(
    ctx: TestContext,
    _kv: Kv,
    db: Db,
) {
    let _ = ctx;
    migrate(&db).await;
    let (session, machine) = held_codespace(&db, SessionState::Active, MachineState::Running).await;
    let github = TestCodespaces {
        reported_state: Some("Failed"),
        ..TestCodespaces::succeeding()
    };
    let destroyed = github.destroyed.clone();

    reconcile(&db, github).await;

    assert_eq!(
        destroyed.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "a `Failed` codespace still bills storage until it is deleted"
    );
    let row = machines::find(&db, machine)
        .await
        .expect("read the machine")
        .expect("the row is still there");
    assert_eq!(row.state, MachineState::Destroyed);
    assert_eq!(
        session_state(&db, session).await,
        (
            SessionState::Interrupted,
            Some(InterruptedReason::MachineLost)
        )
    );
}

#[skyzen::test]
async fn a_running_codespace_leaves_a_healthy_session_alone(ctx: TestContext, _kv: Kv, db: Db) {
    let _ = ctx;
    migrate(&db).await;
    let (session, machine) = held_codespace(&db, SessionState::Active, MachineState::Running).await;

    reconcile(&db, TestCodespaces::succeeding()).await;

    let row = machines::find(&db, machine)
        .await
        .expect("read the machine")
        .expect("the row is still there");
    assert_eq!(row.state, MachineState::Running);
    assert_eq!(
        session_state(&db, session).await,
        (SessionState::Active, None),
        "a codespace that is fine reconciles to nothing"
    );
}

#[skyzen::test]
async fn a_codespace_started_out_of_band_marks_the_machine_running_only(
    ctx: TestContext,
    _kv: Kv,
    db: Db,
) {
    let _ = ctx;
    migrate(&db).await;
    // Suspended, then the user opened it on github.com: GitHub reports it
    // running before its daemon has had a chance to attach.
    let (session, machine) =
        held_codespace(&db, SessionState::Interrupted, MachineState::Deallocated).await;

    reconcile(&db, TestCodespaces::succeeding()).await;

    let row = machines::find(&db, machine)
        .await
        .expect("read the machine")
        .expect("the row is still there");
    assert_eq!(
        row.state,
        MachineState::Running,
        "the compute meter restarts from the sweep that learned it"
    );
    assert_eq!(
        session_state(&db, session).await,
        (
            SessionState::Interrupted,
            Some(InterruptedReason::Suspended)
        ),
        "the session's move back is the daemon's attach to make — it alone proves serving"
    );
}

#[skyzen::test]
async fn a_lost_codespace_leaves_a_paused_session_paused(ctx: TestContext, _kv: Kv, db: Db) {
    let _ = ctx;
    migrate(&db).await;
    // Waiting out a plan window, machine suspended by flyco — and deleted
    // on GitHub while the session waited.
    let (session, machine) =
        held_codespace(&db, SessionState::Paused, MachineState::Deallocated).await;

    reconcile(
        &db,
        TestCodespaces {
            reported_state: None,
            ..TestCodespaces::succeeding()
        },
    )
    .await;

    let row = machines::find(&db, machine)
        .await
        .expect("read the machine")
        .expect("the row is still there");
    assert_eq!(row.state, MachineState::Destroyed);
    assert_eq!(row.native_id, None);
    assert_eq!(
        session_state(&db, session).await.0,
        SessionState::Paused,
        "the pause reason is still true; its wake provisions around the gap"
    );
}

#[skyzen::test]
async fn a_suspension_the_sweep_already_knew_is_not_rewritten(ctx: TestContext, _kv: Kv, db: Db) {
    let _ = ctx;
    migrate(&db).await;
    // Already reconciled once: the machine is deallocated and the session
    // is waiting on its wake. A second sweep with the same answer is a
    // no-op, which is what makes an every-minute cron safe to overlap.
    let (session, machine) =
        held_codespace(&db, SessionState::Interrupted, MachineState::Deallocated).await;

    reconcile(
        &db,
        TestCodespaces {
            reported_state: Some("Shutdown"),
            ..TestCodespaces::succeeding()
        },
    )
    .await;

    let row = machines::find(&db, machine)
        .await
        .expect("read the machine")
        .expect("the row is still there");
    assert_eq!(row.state, MachineState::Deallocated);
    assert_eq!(
        session_state(&db, session).await,
        (
            SessionState::Interrupted,
            Some(InterruptedReason::Suspended)
        )
    );
}
