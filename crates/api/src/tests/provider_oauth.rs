//! Linking a cloud account by signing in with it, end to end through the
//! router.
//!
//! The wire format of the vendor calls is pinned separately, against
//! recorded exchanges, in [`crate::microsoft`] and [`crate::google`]. What is
//! asserted here is everything around them: that the authorize URL carries
//! this deployment's own client and callback, that a state redeems once and
//! only for the sign-in that minted it, that a poll is not a consumption,
//! that an attempt is bound to the user who started it, that the callback
//! answers a browser with a navigation whatever happens, and that the finish
//! ends in the same store path `POST /v1/providers` does.

use flyco_core::{
    CloudProviderKind, CurrentUser, FinishAzureOauth, FinishGcpOauth, Problem, ProviderAccountView,
    ProviderOauthProgress, ProviderOauthStart,
};
use skyzen::routing::Router;
use skyzen_services::{Db, Kv, Queue};
use skyzen_test::mock::InMemoryQueue;
use skyzen_test::{TestClient, TestContext, TestResponse};
use url::Url;

use crate::anthropic::ClaudeClient;
use crate::google::GoogleClient;
use crate::microsoft::MicrosoftClient;
use crate::openai::CodexClient;
use crate::session;
use crate::testing::{
    AZURE_APP_CLIENT_SECRET, AZURE_CLIENT_ID, AZURE_SUBSCRIPTION_ID, AZURE_SUBSCRIPTION_NAME,
    CLOUD_CODE, GCP_ACCOUNT, GCP_PROJECT_ID, GCP_PROJECT_NAME, GCP_SERVICE_ACCOUNT_JSON,
    GOOGLE_CLIENT_ID, REDIRECT_URI, TestClaude, TestCodex, TestGithub, TestGoogle, TestMicrosoft,
    migrate, seed_other_user, seed_user, test_router_with,
};
use crate::vendors::Vendors;

const AZURE_START: &str = "/v1/providers/azure/oauth/start";
const GCP_START: &str = "/v1/providers/gcp/oauth/start";
const PROVIDERS: &str = "/v1/providers";

fn poll_path(provider: &str, started: &ProviderOauthStart) -> String {
    format!("/v1/providers/{provider}/oauth/{}", started.attempt_id)
}

fn finish_path(provider: &str, started: &ProviderOauthStart) -> String {
    format!(
        "/v1/providers/{provider}/oauth/{}/finish",
        started.attempt_id
    )
}

fn callback_path(provider: &str, code: &str, state: &str) -> String {
    format!("/v1/providers/{provider}/oauth/callback?code={code}&state={state}")
}

/// A signed-in caller and the router they call, with both cloud vendors
/// standing in for whatever this test needs them to be.
async fn signed_in_with(
    ctx: &TestContext,
    kv: &Kv,
    db: &Db,
    microsoft: TestMicrosoft,
    google: TestGoogle,
) -> (TestClient<Router>, CurrentUser, String) {
    migrate(db).await;
    let router = test_router_with(
        db.clone(),
        Queue::new(InMemoryQueue::new()),
        TestGithub::default(),
        Vendors::new(
            ClaudeClient::Fake(TestClaude),
            CodexClient::Fake(TestCodex::approved()),
            MicrosoftClient::Fake(microsoft),
            GoogleClient::Fake(google),
        ),
    );
    let user = seed_user(db).await;
    let token = session::issue(kv, user.id).await.expect("issue a session");
    (ctx.client(router), user, token)
}

/// The same, with both vendors signing the user in.
async fn signed_in(
    ctx: &TestContext,
    kv: &Kv,
    db: &Db,
) -> (TestClient<Router>, CurrentUser, String) {
    signed_in_with(
        ctx,
        kv,
        db,
        TestMicrosoft::succeeding(),
        TestGoogle::succeeding(),
    )
    .await
}

/// Begins a sign-in and hands back what the page was given.
async fn begin(client: &TestClient<Router>, token: &str, path: &str) -> ProviderOauthStart {
    let response = client.post(path).bearer(token).send().await;
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

// ── Starting ──

#[skyzen::test]
async fn an_azure_sign_in_starts_at_microsoft_with_this_deployments_own_client(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, AZURE_START).await;

    let url = Url::parse(&started.authorize_url).expect("the authorize URL is absolute");
    assert_eq!(url.host_str(), Some("login.microsoftonline.com"));
    assert_eq!(
        query(&url, "client_id").as_deref(),
        Some(AZURE_CLIENT_ID),
        "the URL presents the client this deployment registered"
    );

    let redirect = query(&url, "redirect_uri").expect("the URL names a callback");
    assert_eq!(
        redirect, "https://flyco.test/v1/providers/azure/oauth/callback",
        "the callback shares the configured origin, so the browser comes back to flyco"
    );

    let scope = query(&url, "scope").expect("the URL names its scopes");
    assert!(
        scope.contains("management.azure.com"),
        "listing subscriptions and assigning a role need ARM: {scope}"
    );
    assert!(
        scope.contains("graph.microsoft.com"),
        "creating the service principal needs Graph: {scope}"
    );
    assert!(!state_of(&started).is_empty());
}

#[skyzen::test]
async fn a_gcp_sign_in_starts_at_google_with_this_deployments_own_client(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, GCP_START).await;

    let url = Url::parse(&started.authorize_url).expect("the authorize URL is absolute");
    assert_eq!(url.host_str(), Some("accounts.google.com"));
    assert_eq!(query(&url, "client_id").as_deref(), Some(GOOGLE_CLIENT_ID));
    assert_eq!(
        query(&url, "redirect_uri").as_deref(),
        Some("https://flyco.test/v1/providers/gcp/oauth/callback")
    );
    assert!(
        query(&url, "scope")
            .expect("the URL names its scopes")
            .contains("cloud-platform")
    );
}

#[skyzen::test]
async fn a_sign_in_cannot_be_started_without_a_flyco_session(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, _token) = signed_in(&ctx, &kv, &db).await;
    client.post(AZURE_START).send().await.assert_status(401);
}

// ── The callback ──

#[skyzen::test]
async fn a_callback_with_an_unknown_state_sends_the_browser_back_with_the_problem(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, _user, _token) = signed_in(&ctx, &kv, &db).await;

    let response = client
        .get(&callback_path("azure", CLOUD_CODE, "never-issued"))
        .send()
        .await;

    // A browser mid-navigation, so the refusal is a navigation too.
    response.assert_status(303);
    let landed = location(&response);
    assert_eq!(landed.path(), "/connect/return");
    assert_eq!(query(&landed, "provider").as_deref(), Some("azure"));
    assert_eq!(
        query(&landed, "problem").as_deref(),
        Some("unknown-oauth-state")
    );
}

#[skyzen::test]
async fn a_callback_records_what_the_vendor_said_and_sends_the_browser_home(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, AZURE_START).await;

    let response = client
        .get(&callback_path("azure", CLOUD_CODE, &state_of(&started)))
        .send()
        .await;
    response.assert_status(303);

    let landed = location(&response);
    let origin = Url::parse(REDIRECT_URI)
        .expect("the test redirect URI is absolute")
        .origin();
    assert_eq!(
        landed.origin(),
        origin,
        "the SPA that reads this is flyco's"
    );
    assert_eq!(landed.path(), "/connect/return");
    assert_eq!(query(&landed, "provider").as_deref(), Some("azure"));
    assert!(query(&landed, "problem").is_none());

    // And the poll the page was already making now has an answer.
    let polled = client
        .get(&poll_path("azure", &started))
        .bearer(&token)
        .send()
        .await;
    polled.assert_status(200);
    assert_eq!(
        polled.json::<ProviderOauthProgress>(),
        ProviderOauthProgress::Authorized {
            account: "me@lexo.cool".to_owned(),
            choices: vec![flyco_core::ProviderOauthChoice {
                id: AZURE_SUBSCRIPTION_ID.to_owned(),
                name: AZURE_SUBSCRIPTION_NAME.to_owned(),
            }],
        }
    );
}

#[skyzen::test]
async fn a_state_redeems_exactly_once(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, AZURE_START).await;
    let path = callback_path("azure", CLOUD_CODE, &state_of(&started));

    client.get(&path).send().await.assert_status(303);

    let again = client.get(&path).send().await;
    again.assert_status(303);
    assert_eq!(
        query(&location(&again), "problem").as_deref(),
        Some("unknown-oauth-state"),
        "a replayed callback resolves to nothing"
    );
}

#[skyzen::test]
async fn one_vendors_state_cannot_complete_the_others_sign_in(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, AZURE_START).await;

    let response = client
        .get(&callback_path("gcp", CLOUD_CODE, &state_of(&started)))
        .send()
        .await;
    response.assert_status(303);
    assert_eq!(
        query(&location(&response), "problem").as_deref(),
        Some("unknown-oauth-state")
    );
}

#[skyzen::test]
async fn a_vendor_that_refuses_reaches_the_return_page_as_its_own_problem(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, _user, token) = signed_in_with(
        &ctx,
        &kv,
        &db,
        TestMicrosoft::refusing(),
        TestGoogle::succeeding(),
    )
    .await;
    let started = begin(&client, &token, AZURE_START).await;

    let response = client
        .get(&callback_path("azure", CLOUD_CODE, &state_of(&started)))
        .send()
        .await;
    response.assert_status(303);
    assert_eq!(
        query(&location(&response), "problem").as_deref(),
        Some("microsoft-rejected")
    );
}

#[skyzen::test]
async fn a_user_who_declined_consent_is_told_so_rather_than_shown_an_extractor_failure(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, GCP_START).await;

    // What Google appends when the user closes the consent screen: an
    // error, and no code at all.
    let response = client
        .get(&format!(
            "/v1/providers/gcp/oauth/callback?error=access_denied&error_description=denied&state={}",
            state_of(&started)
        ))
        .send()
        .await;

    response.assert_status(303);
    let landed = location(&response);
    assert_eq!(query(&landed, "provider").as_deref(), Some("gcp"));
    assert_eq!(
        query(&landed, "problem").as_deref(),
        Some("google-rejected")
    );
    // The vendor's own words travel with the slug, so the return page can
    // say what happened rather than that something did.
    assert!(
        query(&landed, "reason").is_some_and(|reason| reason.contains("denied")),
        "the reason should carry the vendor's text"
    );

    // The page that opened the vendor's tab is still polling; it is told
    // the sign-in is over rather than left to wait out the attempt.
    let polled = client
        .get(&poll_path("gcp", &started))
        .bearer(&token)
        .send()
        .await;
    polled.assert_status(200);
    match polled.json::<ProviderOauthProgress>() {
        ProviderOauthProgress::Failed { problem, reason } => {
            assert_eq!(problem, "google-rejected");
            assert!(reason.contains("denied"), "reason: {reason}");
        }
        other => panic!("expected the poll to report the failure, got {other:?}"),
    }
}

// ── Polling ──

#[skyzen::test]
async fn a_poll_before_the_browser_comes_back_is_pending(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, AZURE_START).await;

    for _ in 0..3 {
        let response = client
            .get(&poll_path("azure", &started))
            .bearer(&token)
            .send()
            .await;
        response.assert_status(200);
        assert_eq!(
            response.json::<ProviderOauthProgress>(),
            ProviderOauthProgress::Pending
        );
    }
}

#[skyzen::test]
async fn one_users_sign_in_is_not_another_users_to_poll(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, AZURE_START).await;

    let other = seed_other_user(&db).await;
    let other_token = session::issue(&kv, other.id)
        .await
        .expect("issue a session for the other user");

    let stolen = client
        .get(&poll_path("azure", &started))
        .bearer(&other_token)
        .send()
        .await;
    stolen.assert_status(404);
    assert_eq!(
        stolen.json::<Problem>().kind,
        "https://flyco.dev/problems/provider-oauth-attempt-expired"
    );

    // And the owner's own sign-in is untouched by the refusal.
    client
        .get(&poll_path("azure", &started))
        .bearer(&token)
        .send()
        .await
        .assert_status(200);
}

#[skyzen::test]
async fn an_unknown_attempt_is_indistinguishable_from_an_expired_one(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;

    let response = client
        .get("/v1/providers/azure/oauth/11111111-2222-4333-8444-555555555555")
        .bearer(&token)
        .send()
        .await;
    response.assert_status(404);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/provider-oauth-attempt-expired"
    );
}

// ── Finishing ──

#[skyzen::test]
async fn finishing_before_the_browser_comes_back_is_refused(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, AZURE_START).await;

    let response = client
        .post(&finish_path("azure", &started))
        .bearer(&token)
        .json(&FinishAzureOauth {
            subscription_id: AZURE_SUBSCRIPTION_ID.to_owned(),
        })
        .send()
        .await;

    response.assert_status(409);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/provider-oauth-not-authorized"
    );
}

#[skyzen::test]
async fn finishing_an_azure_sign_in_links_the_subscription_the_user_chose(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, AZURE_START).await;
    client
        .get(&callback_path("azure", CLOUD_CODE, &state_of(&started)))
        .send()
        .await
        .assert_status(303);

    let response = client
        .post(&finish_path("azure", &started))
        .bearer(&token)
        .json(&FinishAzureOauth {
            subscription_id: AZURE_SUBSCRIPTION_ID.to_owned(),
        })
        .send()
        .await;

    response.assert_status(201);
    let view: ProviderAccountView = response.json();
    assert_eq!(view.kind, CloudProviderKind::Azure);
    assert_eq!(
        view.label, AZURE_SUBSCRIPTION_NAME,
        "the card is named after the subscription the user recognised"
    );

    // The same list `POST /v1/providers` writes into.
    let listed = client.get(PROVIDERS).bearer(&token).send().await;
    listed.assert_status(200);
    let accounts: Vec<ProviderAccountView> = listed.json();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].id, view.id);

    // The service principal flyco just created is sealed like any other
    // credential: the client secret never reaches the database in the clear.
    let user_id = user.id;
    let sealed: String = skyzen::sql!(
        db,
        "SELECT credentials_enc FROM provider_accounts WHERE user_id = {user_id}"
    )
    .fetch_scalar()
    .await
    .expect("read the sealed credential");
    assert!(!sealed.contains(AZURE_APP_CLIENT_SECRET));

    // And the sign-in is spent.
    let polled = client
        .get(&poll_path("azure", &started))
        .bearer(&token)
        .send()
        .await;
    polled.assert_status(404);
    assert_eq!(
        polled.json::<Problem>().kind,
        "https://flyco.dev/problems/provider-oauth-attempt-expired"
    );
}

#[skyzen::test]
async fn finishing_a_gcp_sign_in_links_the_project_the_user_chose(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let (client, user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, GCP_START).await;
    client
        .get(&callback_path("gcp", CLOUD_CODE, &state_of(&started)))
        .send()
        .await
        .assert_status(303);

    let polled = client
        .get(&poll_path("gcp", &started))
        .bearer(&token)
        .send()
        .await;
    polled.assert_status(200);
    assert_eq!(
        polled.json::<ProviderOauthProgress>(),
        ProviderOauthProgress::Authorized {
            account: GCP_ACCOUNT.to_owned(),
            choices: vec![flyco_core::ProviderOauthChoice {
                id: GCP_PROJECT_ID.to_owned(),
                name: GCP_PROJECT_NAME.to_owned(),
            }],
        }
    );

    let response = client
        .post(&finish_path("gcp", &started))
        .bearer(&token)
        .json(&FinishGcpOauth {
            project_id: GCP_PROJECT_ID.to_owned(),
        })
        .send()
        .await;

    response.assert_status(201);
    let view: ProviderAccountView = response.json();
    assert_eq!(view.kind, CloudProviderKind::Gcp);
    assert_eq!(view.label, GCP_PROJECT_NAME);

    let listed = client.get(PROVIDERS).bearer(&token).send().await;
    listed.assert_status(200);
    assert_eq!(listed.json::<Vec<ProviderAccountView>>().len(), 1);

    // The minted service-account key is a private key: it is sealed before
    // the database sees it, exactly as a pasted one would be.
    let user_id = user.id;
    let sealed: String = skyzen::sql!(
        db,
        "SELECT credentials_enc FROM provider_accounts WHERE user_id = {user_id}"
    )
    .fetch_scalar()
    .await
    .expect("read the sealed credential");
    assert!(!sealed.contains("BEGIN PRIVATE KEY"));
    assert!(!sealed.contains(GCP_SERVICE_ACCOUNT_JSON.trim()));
}

#[skyzen::test]
async fn a_finished_sign_in_cannot_be_finished_again(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, GCP_START).await;
    client
        .get(&callback_path("gcp", CLOUD_CODE, &state_of(&started)))
        .send()
        .await
        .assert_status(303);

    let finish = finish_path("gcp", &started);
    let body = FinishGcpOauth {
        project_id: GCP_PROJECT_ID.to_owned(),
    };

    client
        .post(&finish)
        .bearer(&token)
        .json(&body)
        .send()
        .await
        .assert_status(201);

    let again = client.post(&finish).bearer(&token).json(&body).send().await;
    again.assert_status(404);
    assert_eq!(
        again.json::<Problem>().kind,
        "https://flyco.dev/problems/provider-oauth-attempt-expired"
    );
}

#[skyzen::test]
async fn one_users_sign_in_is_not_another_users_to_finish(ctx: TestContext, kv: Kv, db: Db) {
    let (client, _user, token) = signed_in(&ctx, &kv, &db).await;
    let started = begin(&client, &token, GCP_START).await;
    client
        .get(&callback_path("gcp", CLOUD_CODE, &state_of(&started)))
        .send()
        .await
        .assert_status(303);

    let other = seed_other_user(&db).await;
    let other_token = session::issue(&kv, other.id)
        .await
        .expect("issue a session for the other user");

    let stolen = client
        .post(&finish_path("gcp", &started))
        .bearer(&other_token)
        .json(&FinishGcpOauth {
            project_id: GCP_PROJECT_ID.to_owned(),
        })
        .send()
        .await;
    stolen.assert_status(404);
    assert_eq!(
        stolen.json::<Problem>().kind,
        "https://flyco.dev/problems/provider-oauth-attempt-expired"
    );

    // The owner can still finish: the refusal spent nothing.
    client
        .post(&finish_path("gcp", &started))
        .bearer(&token)
        .json(&FinishGcpOauth {
            project_id: GCP_PROJECT_ID.to_owned(),
        })
        .send()
        .await
        .assert_status(201);
}
