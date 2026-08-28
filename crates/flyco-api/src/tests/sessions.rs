//! End-to-end coverage of sessions, budgets, and approvals.

use flyco_core::wire::{ApprovalDecision, ApprovalPayload};
use flyco_core::{
    ApprovalState, ApprovalView, BudgetStage, CreateSession, CurrentUser, DecideApproval,
    HarnessKind, Problem, SessionDetail, SessionId, SessionState, SessionSummary, SpendKind,
    UpdateMe, Usd,
};
use skyzen::routing::Router;
use skyzen_services::{Db, Kv};
use skyzen_test::{TestClient, TestContext};

use crate::testing::{migrated_router, seed_other_user, seed_user};
use crate::{approvals, budgets, session, sessions};

const REPO: &str = "lexoliu/flyco";

fn problem_kind(slug: &str) -> String {
    let mut kind = String::from("https://flyco.dev/problems/");
    kind.push_str(slug);
    kind
}

/// A signed-in caller: their identity plus a live session token.
struct Caller {
    user: CurrentUser,
    token: String,
}

async fn sign_in(kv: &Kv, user: CurrentUser) -> Caller {
    let token = session::issue(kv, user.id).await.expect("issue a session");
    Caller { user, token }
}

fn open(repo: &str, dollars: u64) -> CreateSession {
    CreateSession {
        harness: HarnessKind::ClaudeCode,
        repo: repo.to_owned(),
        budget_limit: Usd::from_dollars(dollars),
        spot: true,
    }
}

async fn create(
    client: &TestClient<Router>,
    caller: &Caller,
    body: &CreateSession,
) -> SessionDetail {
    let response = client
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(body)
        .send()
        .await;
    response.assert_status(201);
    response.json()
}

// ── Creation, validation, and the concurrent-session cap ──

#[skyzen::test]
async fn creating_a_session_returns_it_provisioning_with_its_budget(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, seed_user(&db).await).await;

    let session = create(&ctx.client(router), &caller, &open(REPO, 10)).await;

    assert_eq!(session.summary.repo.to_string(), REPO);
    assert_eq!(session.summary.harness, HarnessKind::ClaudeCode);
    assert_eq!(session.summary.state, SessionState::Provisioning);
    assert_eq!(session.budget.limit, Usd::from_dollars(10));
    assert_eq!(session.budget.spent, Usd::ZERO);
    assert_eq!(session.budget.remaining, Usd::from_dollars(10));
    assert_eq!(session.budget.stage, BudgetStage::Ok);
}

#[skyzen::test]
async fn a_repo_that_is_not_owner_slash_name_is_unprocessable(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, seed_user(&db).await).await;

    let response = ctx
        .client(router)
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(&open("not-a-repo", 10))
        .send()
        .await;

    response.assert_status(422);
    assert_eq!(
        response.json::<Problem>().kind,
        problem_kind("invalid-repo")
    );
}

#[skyzen::test]
async fn a_zero_budget_is_unprocessable(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, seed_user(&db).await).await;

    let response = ctx
        .client(router)
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(&open(REPO, 0))
        .send()
        .await;

    response.assert_status(422);
    assert_eq!(
        response.json::<Problem>().kind,
        problem_kind("invalid-budget")
    );
}

#[skyzen::test]
async fn the_session_cap_is_enforced_and_can_be_raised_or_freed(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, seed_user(&db).await).await;
    let client = ctx.client(router);
    assert_eq!(caller.user.session_cap, 5, "the default cap is five");

    for _ in 0..caller.user.session_cap {
        create(&client, &caller, &open(REPO, 10)).await;
    }

    let refused = client
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(&open(REPO, 10))
        .send()
        .await;
    refused.assert_status(409);
    let problem: Problem = refused.json();
    assert_eq!(problem.kind, problem_kind("session-cap-reached"));
    assert!(
        problem.detail.contains('5'),
        "the detail names the cap: {}",
        problem.detail
    );

    // Raising the cap admits one more.
    let updated = client
        .patch("/v1/me")
        .bearer(&caller.token)
        .json(&UpdateMe {
            session_cap: Some(6),
        })
        .send()
        .await;
    updated.assert_status(200);
    assert_eq!(updated.json::<CurrentUser>().session_cap, 6);
    let sixth = create(&client, &caller, &open(REPO, 10)).await;

    // Back at the cap, archiving frees a slot.
    client
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(&open(REPO, 10))
        .send()
        .await
        .assert_status(409);

    let archived: SessionDetail = {
        let response = client
            .post(&format!("/v1/sessions/{}/archive", sixth.summary.id))
            .bearer(&caller.token)
            .send()
            .await;
        response.assert_status(200);
        response.json()
    };
    assert_eq!(archived.summary.state, SessionState::Archived);

    create(&client, &caller, &open(REPO, 10)).await;
}

#[skyzen::test]
async fn a_session_cap_outside_the_allowed_range_is_unprocessable(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, seed_user(&db).await).await;
    let client = ctx.client(router);

    for cap in [0, 101] {
        let response = client
            .patch("/v1/me")
            .bearer(&caller.token)
            .json(&UpdateMe {
                session_cap: Some(cap),
            })
            .send()
            .await;
        response.assert_status(422);
        assert_eq!(
            response.json::<Problem>().kind,
            problem_kind("invalid-session-cap")
        );
    }
}

// ── Lifecycle ──

#[skyzen::test]
async fn a_session_can_be_archived_once(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = create(&client, &caller, &open(REPO, 10)).await;
    let path = format!("/v1/sessions/{}/archive", session.summary.id);

    let archived = client.post(&path).bearer(&caller.token).send().await;
    archived.assert_status(200);
    assert_eq!(
        archived.json::<SessionDetail>().summary.state,
        SessionState::Archived
    );

    let again = client.post(&path).bearer(&caller.token).send().await;
    again.assert_status(409);
    let problem: Problem = again.json();
    assert_eq!(problem.kind, problem_kind("invalid-session-transition"));
    assert!(
        problem.detail.contains("Archived"),
        "the detail names both states: {}",
        problem.detail
    );
}

#[skyzen::test]
async fn sessions_are_listed_newest_first(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, seed_user(&db).await).await;
    let client = ctx.client(router);

    let older = create(&client, &caller, &open(REPO, 10)).await;
    let newer = create(&client, &caller, &open("lexoliu/skyzen", 10)).await;

    // Both were created inside the same second, so backdate one: the
    // assertion is about the ordering contract, not about how fast the test
    // machine is.
    db.query("UPDATE sessions SET created_at_unix = created_at_unix - 60 WHERE id = ?")
        .bind(older.summary.id.to_string())
        .execute()
        .await
        .expect("backdate the older session");

    let listed = client
        .get("/v1/sessions")
        .bearer(&caller.token)
        .send()
        .await;
    listed.assert_status(200);
    let listed: Vec<SessionSummary> = listed.json();

    assert_eq!(
        listed.iter().map(|entry| entry.id).collect::<Vec<_>>(),
        vec![newer.summary.id, older.summary.id]
    );
}

// ── Budgets ──

#[skyzen::test]
async fn the_budget_view_replays_the_spend_ledger(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = create(&client, &caller, &open(REPO, 10)).await;
    let budget = budget_id(&db, session.summary.id).await;
    let path = format!("/v1/sessions/{}/budget", session.summary.id);

    // $10 budget: cross each threshold in turn and confirm the stage the
    // engine reports is the one the API serves.
    let steps = [
        (4_900_000, BudgetStage::Ok),
        (100_000, BudgetStage::Notice50),
        (3_000_000, BudgetStage::Warn80),
        (1_000_000, BudgetStage::Final90),
        (1_000_000, BudgetStage::Exhausted),
    ];

    let mut spent = 0_u64;
    for (micros, stage) in steps {
        budgets::record(
            &db,
            budget,
            SpendKind::Compute,
            Usd::from_micros(micros),
            "machine time",
        )
        .await
        .expect("record spend");
        spent += micros;

        let response = client.get(&path).bearer(&caller.token).send().await;
        response.assert_status(200);
        let view: flyco_core::BudgetView = response.json();

        assert_eq!(view.spent, Usd::from_micros(spent));
        assert_eq!(view.limit, Usd::from_dollars(10));
        assert_eq!(
            view.remaining,
            Usd::from_dollars(10).saturating_sub(Usd::from_micros(spent))
        );
        assert_eq!(view.stage, stage, "after spending {spent} micros");
    }
}

#[skyzen::test]
async fn a_small_budget_advances_through_the_same_stages(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, seed_user(&db).await).await;
    let client = ctx.client(router);
    // Under $5, so the engine stays silent below 90% — the stage still moves.
    let session = create(&client, &caller, &open(REPO, 4)).await;
    let budget = budget_id(&db, session.summary.id).await;
    let path = format!("/v1/sessions/{}/budget", session.summary.id);

    for (micros, stage) in [
        (2_000_000, BudgetStage::Notice50),
        (1_200_000, BudgetStage::Warn80),
        (400_000, BudgetStage::Final90),
        (400_000, BudgetStage::Exhausted),
    ] {
        budgets::record(
            &db,
            budget,
            SpendKind::Compute,
            Usd::from_micros(micros),
            "machine time",
        )
        .await
        .expect("record spend");

        let response = client.get(&path).bearer(&caller.token).send().await;
        assert_eq!(response.json::<flyco_core::BudgetView>().stage, stage);
    }
}

#[skyzen::test]
async fn the_replay_refreshes_the_cached_budget_row(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = create(&client, &caller, &open(REPO, 10)).await;
    let budget = budget_id(&db, session.summary.id).await;

    budgets::record(
        &db,
        budget,
        SpendKind::Storage,
        Usd::from_dollars(9),
        "disk",
    )
    .await
    .expect("record spend");

    client
        .get(&format!("/v1/sessions/{}/budget", session.summary.id))
        .bearer(&caller.token)
        .send()
        .await
        .assert_status(200);

    let row: std::collections::BTreeMap<String, serde_json::Value> = db
        .query("SELECT spent_micros, stage FROM budgets WHERE id = ?")
        .bind(budget.to_string())
        .fetch_one()
        .await
        .expect("read the budget row");

    assert_eq!(row["spent_micros"], 9_000_000_i64);
    assert_eq!(row["stage"], "final90");
}

// ── Approvals ──

#[skyzen::test]
async fn approvals_are_listed_filtered_and_decided_once(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = create(&client, &caller, &open(REPO, 10)).await;
    let other_session = create(&client, &caller, &open("lexoliu/skyzen", 10)).await;

    let pending = approvals::raise(
        &db,
        session.summary.id,
        &ApprovalPayload::Merge {
            repo: REPO.to_owned(),
            from_branch: "agent/fix".to_owned(),
            into_branch: "main".to_owned(),
        },
    )
    .await
    .expect("raise an approval");
    approvals::raise(
        &db,
        other_session.summary.id,
        &ApprovalPayload::AgentsMdChange {
            find: "old".to_owned(),
            replace: "new".to_owned(),
        },
    )
    .await
    .expect("raise an approval");

    let all = client
        .get("/v1/approvals")
        .bearer(&caller.token)
        .send()
        .await;
    all.assert_status(200);
    assert_eq!(all.json::<Vec<ApprovalView>>().len(), 2);

    let scoped = client
        .get(&format!("/v1/approvals?session={}", session.summary.id))
        .bearer(&caller.token)
        .send()
        .await;
    scoped.assert_status(200);
    let scoped: Vec<ApprovalView> = scoped.json();
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].id, pending);
    assert_eq!(scoped[0].state, ApprovalState::Pending);

    let path = format!("/v1/approvals/{pending}/decision");
    let decided = client
        .post(&path)
        .bearer(&caller.token)
        .json(&DecideApproval {
            decision: ApprovalDecision::Approved,
        })
        .send()
        .await;
    decided.assert_status(200);
    assert_eq!(
        decided.json::<ApprovalView>().state,
        ApprovalState::Approved
    );

    // The pending filter no longer matches it.
    let still_pending = client
        .get("/v1/approvals?state=pending")
        .bearer(&caller.token)
        .send()
        .await;
    still_pending.assert_status(200);
    let still_pending: Vec<ApprovalView> = still_pending.json();
    assert_eq!(still_pending.len(), 1);
    assert_ne!(still_pending[0].id, pending);

    let again = client
        .post(&path)
        .bearer(&caller.token)
        .json(&DecideApproval {
            decision: ApprovalDecision::Denied,
        })
        .send()
        .await;
    again.assert_status(409);
    assert_eq!(
        again.json::<Problem>().kind,
        problem_kind("approval-already-decided")
    );
}

// ── Ownership ──

#[skyzen::test]
async fn one_user_cannot_reach_anothers_session_or_approval(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let owner = sign_in(&kv, seed_user(&db).await).await;
    let stranger = sign_in(&kv, seed_other_user(&db).await).await;
    let client = ctx.client(router);

    let session = create(&client, &owner, &open(REPO, 10)).await;
    let approval = approvals::raise(
        &db,
        session.summary.id,
        &ApprovalPayload::ToolUse {
            tool: "Bash".to_owned(),
            input: serde_json::json!({ "command": "rm -rf /" }),
        },
    )
    .await
    .expect("raise an approval");

    let id = session.summary.id;
    for path in [
        format!("/v1/sessions/{id}"),
        format!("/v1/sessions/{id}/budget"),
    ] {
        let response = client.get(&path).bearer(&stranger.token).send().await;
        response.assert_status(404);
        assert_eq!(
            response.json::<Problem>().kind,
            problem_kind("session-not-found")
        );
    }

    client
        .post(&format!("/v1/sessions/{id}/archive"))
        .bearer(&stranger.token)
        .send()
        .await
        .assert_status(404);

    let refused = client
        .post(&format!("/v1/approvals/{approval}/decision"))
        .bearer(&stranger.token)
        .json(&DecideApproval {
            decision: ApprovalDecision::Approved,
        })
        .send()
        .await;
    refused.assert_status(404);
    assert_eq!(
        refused.json::<Problem>().kind,
        problem_kind("approval-not-found")
    );

    // The stranger's own listings are empty, not the owner's.
    let listed = client
        .get("/v1/sessions")
        .bearer(&stranger.token)
        .send()
        .await;
    assert_eq!(
        listed.json::<Vec<SessionSummary>>(),
        [] as [SessionSummary; 0]
    );
    let listed = client
        .get("/v1/approvals")
        .bearer(&stranger.token)
        .send()
        .await;
    assert_eq!(listed.json::<Vec<ApprovalView>>(), [] as [ApprovalView; 0]);

    // The owner still sees both.
    client
        .get(&format!("/v1/sessions/{id}"))
        .bearer(&owner.token)
        .send()
        .await
        .assert_status(200);
}

#[skyzen::test]
async fn an_unknown_session_is_not_found(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, seed_user(&db).await).await;

    let response = ctx
        .client(router)
        .get(&format!("/v1/sessions/{}", SessionId::generate()))
        .bearer(&caller.token)
        .send()
        .await;

    response.assert_status(404);
}

async fn budget_id(db: &Db, session: SessionId) -> flyco_core::BudgetId {
    let row: std::collections::BTreeMap<String, String> = db
        .query("SELECT budget_id FROM sessions WHERE id = ?")
        .bind(session.to_string())
        .fetch_one()
        .await
        .expect("read the session row");
    row["budget_id"].parse().expect("budget_id is a UUID")
}

/// Guards the invariant the ownership tests rely on: `sessions::is_owned_by`
/// answers for the real owner only.
#[skyzen::test]
async fn ownership_is_answered_per_user(db: Db) {
    crate::testing::migrate(&db).await;
    let owner = seed_user(&db).await;
    let stranger = seed_other_user(&db).await;

    let session = sessions::create(
        &db,
        owner.id,
        owner.session_cap,
        HarnessKind::Codex,
        &REPO.parse().expect("valid repo"),
        flyco_core::BudgetConfig::new(Usd::from_dollars(1)).expect("non-zero"),
    )
    .await
    .expect("create a session");

    assert!(
        sessions::is_owned_by(&db, owner.id, session.summary.id)
            .await
            .expect("ownership check")
    );
    assert!(
        !sessions::is_owned_by(&db, stranger.id, session.summary.id)
            .await
            .expect("ownership check")
    );
}
