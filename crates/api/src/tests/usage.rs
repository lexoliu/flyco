//! The two usage panels, and the daemon route that fills one of them.
//!
//! What is worth pinning here is the *claim* the panel makes. It reports
//! what flyco observed, so an account nothing has been observed about must
//! answer "nothing observed" rather than "$0.00 spent" — one is a fact and
//! the other is an assertion flyco cannot support. The rest is scope: an
//! observation reaches exactly the account of the session that posted it.

use flyco_core::{
    CloudUsageView, HarnessAccountId, HarnessAccountView, HarnessKind, HarnessObservation,
    LlmUsageView, OBSERVATION_WINDOW_SECONDS, Problem, ProviderAccountView, RateLimitObservation,
    ReportUsage, SessionId, UsageWindow, Usd, UserId,
};
use skyzen::routing::Router;
use skyzen::sql;
use skyzen_services::{Db, Kv};
use skyzen_test::{TestClient, TestContext};

use crate::clock::now_unix;
use crate::testing::{
    migrated_router, seed_other_user, seed_provider_account, seed_session, seed_user,
};
use crate::{daemon_tokens, session};

const PATH: &str = "/v1/usage/llm";

/// Links a harness account directly, matching the authenticated link route.
async fn link(db: &Db, user: UserId, harness: HarnessKind, label: &str) -> HarnessAccountId {
    let id = HarnessAccountId::generate();
    sql!(
        db,
        "INSERT INTO harness_accounts \
         (id, user_id, harness, label, credential_enc, linked_at_unix, expires_at_unix) \
         VALUES ({id}, {user}, {harness}, {label}, {\"sealed\"}, {1_800_000_000_u64}, NULL)"
    )
    .execute()
    .await
    .expect("link a harness account");
    id
}

/// Records an observation the way a daemon would, at a chosen moment.
///
/// Used only for the rows that must sit outside the window: everything else
/// goes in over the route, which is what the daemon actually calls.
async fn observed_at(db: &Db, account: HarnessAccountId, cost: Usd, at_unix: u64) {
    let user: UserId = sql!(
        db,
        "SELECT user_id FROM harness_accounts WHERE id = {account}"
    )
    .fetch_scalar()
    .await
    .expect("the account exists");
    let session = crate::testing::seed_session(
        db,
        &crate::users::find(db, user)
            .await
            .expect("read the user")
            .expect("the user exists"),
    )
    .await;

    sql!(
        db,
        "INSERT INTO harness_observations \
         (id, user_id, harness_account_id, session_id, observed_cost_micros, \
          rate_limited_at_unix, resets_at_unix, at_unix) \
         VALUES ({flyco_core::HarnessObservationId::generate()}, {user}, {account}, {session}, \
                 {cost}, NULL, NULL, {at_unix})"
    )
    .execute()
    .await
    .expect("record an observation");
}

async fn pair(db: &Db, user: UserId, session: SessionId) -> String {
    daemon_tokens::issue(db, user, session)
        .await
        .expect("mint a daemon token")
        .token
}

async fn post(
    client: &TestClient<Router>,
    token: &str,
    session: SessionId,
    observation: &HarnessObservation,
) -> skyzen_test::TestResponse {
    client
        .post(&format!("/v1/sessions/{session}/harness-observations"))
        .bearer(token)
        .json(observation)
        .send()
        .await
}

async fn panel(client: &TestClient<Router>, token: &str) -> Vec<LlmUsageView> {
    let response = client.get(PATH).bearer(token).send().await;
    response.assert_status(200);
    response.json()
}

fn cost(micros: u64) -> HarnessObservation {
    HarnessObservation {
        observed_cost: Some(Usd::from_micros(micros)),
        rate_limit: None,
    }
}

#[skyzen::test]
async fn an_account_nothing_is_known_about_reports_nothing_rather_than_zero(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    let account = link(&db, user.id, HarnessKind::ClaudeCode, "personal").await;

    let rows = panel(&client, &token).await;
    let [row] = rows.as_slice() else {
        panic!("one linked account is one row: {rows:?}");
    };
    assert_eq!(row.account, account);
    assert_eq!(row.label, "personal");
    assert_eq!(
        row.observed_cost, None,
        "flyco has observed no cost, which is not the same claim as no cost"
    );
    assert_eq!(row.rate_limited_at_unix, None);
}

#[skyzen::test]
async fn a_user_with_no_linked_account_has_an_empty_panel(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");

    assert_eq!(panel(&client, &token).await, [] as [LlmUsageView; 0]);
}

#[skyzen::test]
async fn observed_costs_sum_over_the_window(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    let account = link(&db, user.id, HarnessKind::ClaudeCode, "personal").await;
    let session = seed_session(&db, &user).await;
    let daemon = pair(&db, user.id, session).await;

    post(&client, &daemon, session, &cost(1_500_000))
        .await
        .assert_status(204);
    post(&client, &daemon, session, &cost(250_000))
        .await
        .assert_status(204);

    // A turn from before the window opened is not this window's spend.
    observed_at(
        &db,
        account,
        Usd::from_dollars(99),
        now_unix() - OBSERVATION_WINDOW_SECONDS - 60,
    )
    .await;

    // The server states the window as of when it answered, so the only
    // honest assertion brackets it: a wall-clock second may tick between
    // this read and the one the handler made.
    let earliest = now_unix() - OBSERVATION_WINDOW_SECONDS;
    let rows = panel(&client, &token).await;
    let latest = now_unix() - OBSERVATION_WINDOW_SECONDS;
    assert_eq!(rows[0].observed_cost, Some(Usd::from_micros(1_750_000)));
    assert!(
        (earliest..=latest).contains(&rows[0].period_start_unix),
        "the panel states the window it summed"
    );
}

#[skyzen::test]
async fn the_limit_still_in_force_is_the_one_reported(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    link(&db, user.id, HarnessKind::ClaudeCode, "personal").await;
    let session = seed_session(&db, &user).await;
    let daemon = pair(&db, user.id, session).await;

    for resets_at_unix in [Some(1_800_003_600), None, Some(1_800_007_200)] {
        post(
            &client,
            &daemon,
            session,
            &HarnessObservation {
                observed_cost: None,
                rate_limit: Some(RateLimitObservation { resets_at_unix }),
            },
        )
        .await
        .assert_status(204);
    }

    let rows = panel(&client, &token).await;
    assert!(rows[0].rate_limited_at_unix.is_some());
    // All three land in the same second, which is a real tie: the limit
    // that runs longest is the one still in force.
    assert_eq!(
        rows[0].resets_at_unix,
        Some(1_800_007_200),
        "the limit that lasts longest is the one that still binds"
    );
    assert_eq!(
        rows[0].observed_cost, None,
        "a rate limit says nothing about cost"
    );
}

#[skyzen::test]
async fn an_observation_lands_on_the_account_its_session_runs(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    let claude = link(&db, user.id, HarnessKind::ClaudeCode, "personal").await;
    let codex = link(&db, user.id, HarnessKind::Codex, "work").await;

    // `seed_session` opens a Claude Code session, so its daemon's
    // observations must reach the Claude account and only that one.
    let session = seed_session(&db, &user).await;
    let daemon = pair(&db, user.id, session).await;
    post(&client, &daemon, session, &cost(750_000))
        .await
        .assert_status(204);

    let rows = panel(&client, &token).await;
    let claude_row = rows
        .iter()
        .find(|row| row.account == claude)
        .expect("the Claude account is listed");
    let codex_row = rows
        .iter()
        .find(|row| row.account == codex)
        .expect("the Codex account is listed");

    assert_eq!(claude_row.observed_cost, Some(Usd::from_micros(750_000)));
    assert_eq!(codex_row.observed_cost, None);
}

#[skyzen::test]
async fn a_session_whose_user_linked_no_account_has_nowhere_to_record(
    ctx: TestContext,
    _kv: Kv,
    db: Db,
) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let session = seed_session(&db, &user).await;
    let daemon = pair(&db, user.id, session).await;

    let response = post(&client, &daemon, session, &cost(1_000)).await;
    response.assert_status(404);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/harness-account-not-found"
    );
}

#[skyzen::test]
async fn an_observation_of_nothing_is_refused(ctx: TestContext, _kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    link(&db, user.id, HarnessKind::ClaudeCode, "personal").await;
    let session = seed_session(&db, &user).await;
    let daemon = pair(&db, user.id, session).await;

    let response = post(
        &client,
        &daemon,
        session,
        &HarnessObservation {
            observed_cost: None,
            rate_limit: None,
        },
    )
    .await;

    response.assert_status(422);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/empty-observation"
    );
}

#[skyzen::test]
async fn one_session_cannot_record_against_another(ctx: TestContext, _kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    link(&db, user.id, HarnessKind::ClaudeCode, "personal").await;
    let mine = seed_session(&db, &user).await;
    let other = seed_session(&db, &user).await;
    let daemon = pair(&db, user.id, mine).await;

    // The daemon token is the session's, so presenting it against another
    // session's route is refused by the lookup itself.
    post(&client, &daemon, other, &cost(1_000))
        .await
        .assert_status(401);
}

#[skyzen::test]
async fn a_panel_shows_only_the_callers_own_accounts(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let owner = seed_user(&db).await;
    let stranger = seed_other_user(&db).await;
    link(&db, owner.id, HarnessKind::ClaudeCode, "mine").await;
    link(&db, stranger.id, HarnessKind::ClaudeCode, "theirs").await;

    let owner_token = session::issue(&kv, owner.id)
        .await
        .expect("issue a session");
    let rows = panel(&client, &owner_token).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].label, "mine");
}

// ── The cloud panel ──

async fn cloud(client: &TestClient<Router>, token: &str) -> Vec<CloudUsageView> {
    let response = client.get("/v1/usage/cloud").bearer(token).send().await;
    response.assert_status(200);
    response.json()
}

#[skyzen::test]
async fn a_user_with_no_linked_provider_has_an_empty_cloud_panel(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");

    assert_eq!(cloud(&client, &token).await, [] as [CloudUsageView; 0]);
}

#[skyzen::test]
async fn a_host_the_user_owns_contributes_no_row(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");

    let account = seed_provider_account(&db, user.id).await;
    assert!(
        client
            .get("/v1/providers")
            .bearer(&token)
            .send()
            .await
            .json::<Vec<ProviderAccountView>>()
            .iter()
            .any(|row| row.id == account),
        "the account is linked"
    );

    // Flyco meters nothing on hardware the user already owns and already
    // pays for, so it says nothing. A `$0.00` row would assert the machine
    // is free, which is the one thing flyco knows it cannot claim.
    assert_eq!(
        cloud(&client, &token).await,
        [] as [CloudUsageView; 0],
        "an unmetered provider contributes no row rather than a zero"
    );
}

// ── The plan's own limits, as the harness reports them ──

/// One window, as a daemon files it.
fn window(minutes: u32, percent: u8) -> UsageWindow {
    UsageWindow::new(Some(minutes), None, percent, Some(1_789_002_000))
}

/// Files a plan-usage snapshot the way a session's daemon does.
async fn report(
    client: &TestClient<Router>,
    token: &str,
    session: SessionId,
    windows: Vec<UsageWindow>,
) -> skyzen_test::TestResponse {
    client
        .put(&format!("/v1/sessions/{session}/usage"))
        .bearer(token)
        .json(&ReportUsage { windows })
        .send()
        .await
}

/// The caller's linked accounts, as Settings lists them.
async fn accounts(client: &TestClient<Router>, token: &str) -> Vec<HarnessAccountView> {
    let response = client
        .get("/v1/harness-accounts")
        .bearer(token)
        .send()
        .await;
    response.assert_status(200);
    response.json()
}

#[skyzen::test]
async fn a_reported_snapshot_reaches_the_account_its_session_runs(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    let claude = link(&db, user.id, HarnessKind::ClaudeCode, "personal").await;
    let codex = link(&db, user.id, HarnessKind::Codex, "work").await;

    // `seed_session` opens a Claude Code session, so the snapshot belongs
    // to the Claude account and to that one only: the body says nothing
    // about whose plan it is, and the session row is what decides.
    let session = seed_session(&db, &user).await;
    let daemon = pair(&db, user.id, session).await;
    let reported = vec![window(300, 26), window(10_080, 15)];
    report(&client, &daemon, session, reported.clone())
        .await
        .assert_status(204);

    let listed = accounts(&client, &token).await;
    let claude_row = listed
        .iter()
        .find(|row| row.id == claude)
        .expect("the Claude account is listed");
    let codex_row = listed
        .iter()
        .find(|row| row.id == codex)
        .expect("the Codex account is listed");

    assert_eq!(claude_row.usage, reported);
    assert_eq!(
        codex_row.usage,
        [] as [UsageWindow; 0],
        "nothing has asked the Codex plan, so nothing is drawn for it"
    );
}

#[skyzen::test]
async fn the_newest_snapshot_replaces_the_last_one_whole(ctx: TestContext, kv: Kv, db: Db) {
    // The vendor answers the whole question every time it is asked, so a
    // window the plan no longer has must leave the screen rather than
    // surviving as the older half of a merge.
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    link(&db, user.id, HarnessKind::ClaudeCode, "personal").await;
    let session = seed_session(&db, &user).await;
    let daemon = pair(&db, user.id, session).await;

    report(
        &client,
        &daemon,
        session,
        vec![window(300, 26), window(10_080, 15)],
    )
    .await
    .assert_status(204);
    report(&client, &daemon, session, vec![window(300, 31)])
        .await
        .assert_status(204);

    let listed = accounts(&client, &token).await;
    assert_eq!(listed[0].usage, [window(300, 31)]);
}

#[skyzen::test]
async fn a_session_on_no_plan_at_all_reports_an_empty_snapshot(ctx: TestContext, kv: Kv, db: Db) {
    // What an API-key session answers. Storing it is what makes an account
    // that moved off a subscription stop showing yesterday's rings, so an
    // empty list is recorded rather than refused.
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    link(&db, user.id, HarnessKind::ClaudeCode, "personal").await;
    let session = seed_session(&db, &user).await;
    let daemon = pair(&db, user.id, session).await;

    report(&client, &daemon, session, vec![window(300, 26)])
        .await
        .assert_status(204);
    report(&client, &daemon, session, Vec::new())
        .await
        .assert_status(204);

    assert_eq!(
        accounts(&client, &token).await[0].usage,
        [] as [UsageWindow; 0]
    );
}

#[skyzen::test]
async fn a_snapshot_for_a_harness_the_user_has_no_account_for_is_refused(
    ctx: TestContext,
    _kv: Kv,
    db: Db,
) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let session = seed_session(&db, &user).await;
    let daemon = pair(&db, user.id, session).await;

    let response = report(&client, &daemon, session, vec![window(300, 26)]).await;
    response.assert_status(404);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/harness-account-not-found"
    );
}
