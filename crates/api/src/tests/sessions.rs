//! End-to-end coverage of sessions, budgets, and approvals.

use flyco_core::wire::{ApprovalDecision, ApprovalPayload};
use flyco_core::{
    ApprovalId, ApprovalState, ApprovalView, BudgetStage, CreateSession, CurrentUser,
    DecideApproval, EnvDocument, EnvEntry, HarnessKind, MachineOrigin, MachineState, ModelChoice,
    ModelOption, Problem, ProviderAccountId, Runtime, SessionDetail, SessionId, SessionState,
    SessionSummary, SpendKind, UpdateEnv, UpdateMe, UpdateSession, Usd, builtin_models,
};
use skyzen::routing::Router;
use skyzen::sql;
use skyzen_services::sql::Row;
use skyzen_services::{Db, Kv};
use skyzen_test::{TestClient, TestContext};

use crate::rooms::{NativeRooms, Rooms};
use crate::testing::{
    SSH_HOST, TestGithub, machine_choice, migrated_router, seed_other_user, seed_provider_account,
    seed_user,
};
use crate::{
    app, approvals, budgets, harness_accounts, machines, metering, session, sessions, testing,
    usage_limits,
};

const REPO: &str = "lexoliu/flyco";

/// The branch a session works on when it names one itself.
const BRANCH: &str = "dev";

/// The opening instruction every test session is created with.
const PROMPT: &str = "audit the relay for dropped frames";

/// A `PATCH /v1/sessions/{id}` body that only renames.
fn rename(title: &str) -> UpdateSession {
    UpdateSession {
        title: Some(title.to_owned()),
        ..UpdateSession::default()
    }
}

/// A `PATCH /v1/sessions/{id}` body that only changes the budget.
fn rebudget(dollars: u64) -> UpdateSession {
    UpdateSession {
        budget_limit: Some(Usd::from_dollars(dollars)),
        ..UpdateSession::default()
    }
}

fn problem_kind(slug: &str) -> String {
    let mut kind = String::from("https://flyco.dev/problems/");
    kind.push_str(slug);
    kind
}

/// A signed-in caller: their identity, a live session token, and the
/// provider account their sessions are provisioned onto.
struct Caller {
    user: CurrentUser,
    token: String,
    account: ProviderAccountId,
}

async fn sign_in(kv: &Kv, db: &Db, user: CurrentUser) -> Caller {
    let token = session::issue(kv, user.id).await.expect("issue a session");
    let account = seed_provider_account(db, user.id).await;
    Caller {
        user,
        token,
        account,
    }
}

fn open(caller: &Caller, repo: &str, dollars: u64) -> CreateSession {
    CreateSession {
        source: None,
        prompt: PROMPT.to_owned(),
        harness: HarnessKind::ClaudeCode,
        repos: vec![flyco_core::RepoSelection {
            repo: repo.to_owned(),
            branch: None,
        }],
        budget_limit: Usd::from_dollars(dollars),
        machine: Some(machine_choice(caller.account)),
        spot: true,
        model: None,
        permission_mode: None,
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
async fn a_session_records_the_runtime_the_catalog_offers_its_type_as(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    // The only entry the test catalog offers is the enrolled machine, and a
    // session there has always been a Podman container. The row says so, so
    // the machine's own `flycod` is told what a stop does to its disk.
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;

    let session = create(&ctx.client(router), &caller, &open(&caller, REPO, 10)).await;

    let session_id = session.summary.id;
    let runtime: Runtime = sql!(
        db,
        "SELECT runtime FROM machines WHERE session_id = {session_id}"
    )
    .fetch_scalar()
    .await
    .expect("the runtime was recorded");
    assert_eq!(runtime, Runtime::Container);
}

#[skyzen::test]
async fn a_choice_that_contradicts_the_catalog_is_refused_rather_than_reshaped(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    // The type is on offer and the runtime sent with it is not the runtime
    // it is offered as: a picker working from a catalog that has since
    // changed. Provisioning the runtime on offer would hand a session that
    // asked for a disk one that loses its working tree on every stop.
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;

    let mut body = open(&caller, REPO, 10);
    body.machine.as_mut().expect("a chosen machine").runtime = Runtime::Vm;

    let response = ctx
        .client(router)
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(&body)
        .send()
        .await;

    response.assert_status(400);
    let problem: Problem = response.json();
    assert!(
        problem.kind.ends_with("/machine-runtime-mismatch"),
        "unexpected problem: {problem:?}"
    );
    assert!(
        problem.detail.contains(SSH_HOST),
        "the refusal names the type both halves disagree about: {problem:?}"
    );
}

#[skyzen::test]
async fn creating_a_session_returns_it_provisioning_with_its_budget(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;

    let session = create(&ctx.client(router), &caller, &open(&caller, REPO, 10)).await;

    assert_eq!(session.summary.repos[0].slug.to_string(), REPO);
    assert_eq!(session.summary.harness, HarnessKind::ClaudeCode);
    assert_eq!(session.summary.state, SessionState::Provisioning);
    assert_eq!(
        session.summary.title, PROMPT,
        "a session opens named by the prompt that opened it"
    );
    assert_eq!(
        session.summary.machine_origin,
        MachineOrigin::User,
        "this request named a machine, so the user chose it"
    );
    assert_eq!(session.budget.limit, Usd::from_dollars(10));
    assert_eq!(session.budget.spent, Usd::ZERO);
    assert_eq!(session.budget.remaining, Usd::from_dollars(10));
    assert_eq!(session.budget.stage, BudgetStage::Ok);
}

#[skyzen::test]
async fn omitting_the_machine_lets_flyco_pick_one(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);

    let session = create(
        &client,
        &caller,
        &CreateSession {
            source: None,
            prompt: PROMPT.to_owned(),
            harness: HarnessKind::ClaudeCode,
            repos: vec![flyco_core::RepoSelection {
                repo: REPO.to_owned(),
                branch: None,
            }],
            budget_limit: Usd::from_dollars(10),
            machine: None,
            spot: true,
            model: None,
            permission_mode: None,
        },
    )
    .await;

    assert_eq!(session.summary.state, SessionState::Provisioning);
    let session_id = session.summary.id;
    let machine_type: String = sql!(
        db,
        "SELECT machine_type FROM machines WHERE session_id = {session_id}"
    )
    .fetch_scalar()
    .await
    .expect("the chosen type was recorded");
    assert_eq!(
        machine_type, SSH_HOST,
        "the only deployable Linux type in tests is the registered host"
    );
    assert_eq!(
        session.summary.machine_origin,
        MachineOrigin::Auto,
        "a session that named no machine records that flyco chose it"
    );
}

#[skyzen::test]
async fn the_prompt_is_recorded_as_the_session_s_first_user_message(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);

    let session = create(&client, &caller, &open(&caller, REPO, 10)).await;

    // No machine exists yet, let alone a daemon: the prompt is written to
    // the session's room, which holds it until one greets the control plane.
    let events = client
        .get(&format!("/v1/sessions/{}/events", session.summary.id))
        .bearer(&caller.token)
        .send()
        .await;
    events.assert_status(200);
    let page: flyco_core::wire::EventPage = events.json();
    assert_eq!(
        page.events
            .into_iter()
            .map(|stored| stored.event)
            .collect::<Vec<_>>(),
        vec![
            serde_json::to_value(flyco_core::ClientEvent::UserMessage {
                text: PROMPT.to_owned(),
                origin: flyco_core::MessageOrigin::User,
            })
            .expect("serialize")
        ]
    );
}

#[skyzen::test]
async fn a_session_cannot_be_opened_with_a_blank_prompt(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;

    let response = ctx
        .client(router)
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(&CreateSession {
            source: None,
            prompt: "   \n ".to_owned(),
            ..open(&caller, REPO, 10)
        })
        .send()
        .await;

    response.assert_status(422);
    assert_eq!(
        response.json::<Problem>().kind,
        problem_kind("empty-message")
    );
}

#[skyzen::test]
async fn a_long_prompt_is_shortened_into_the_title(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;

    let session = create(
        &ctx.client(router),
        &caller,
        &CreateSession {
            source: None,
            prompt: "x".repeat(flyco_core::MAX_SESSION_TITLE_CHARS * 3),
            ..open(&caller, REPO, 10)
        },
    )
    .await;

    assert_eq!(
        session.summary.title.chars().count(),
        flyco_core::MAX_SESSION_TITLE_CHARS
    );
    assert!(session.summary.title.ends_with('…'));
}

#[skyzen::test]
async fn a_session_can_be_renamed_and_refuses_a_title_nobody_could_read(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = create(&client, &caller, &open(&caller, REPO, 10)).await;
    let path = format!("/v1/sessions/{}", session.summary.id);

    let renamed = client
        .patch(&path)
        .bearer(&caller.token)
        .json(&rename("  Rework the relay mailbox  "))
        .send()
        .await;
    renamed.assert_status(200);
    assert_eq!(
        renamed.json::<SessionDetail>().summary.title,
        "Rework the relay mailbox",
        "a title is stored trimmed"
    );

    for title in [
        String::new(),
        "   ".to_owned(),
        "t".repeat(flyco_core::MAX_SESSION_TITLE_CHARS + 1),
    ] {
        let refused = client
            .patch(&path)
            .bearer(&caller.token)
            .json(&rename(&title))
            .send()
            .await;
        refused.assert_status(422);
        assert_eq!(
            refused.json::<Problem>().kind,
            problem_kind("invalid-title")
        );
    }

    // The rename is the owner's to make, and nobody else's.
    let stranger = sign_in(&kv, &db, seed_other_user(&db).await).await;
    client
        .patch(&path)
        .bearer(&stranger.token)
        .json(&rename("mine now"))
        .send()
        .await
        .assert_status(404);
}

#[skyzen::test]
async fn the_default_machine_is_the_one_a_session_would_be_given(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;

    let response = ctx
        .client(router)
        .get("/v1/machines/default?spot=true")
        .bearer(&caller.token)
        .send()
        .await;
    response.assert_status(200);

    let answer: flyco_core::MachineDefault = response.json();
    assert_eq!(answer.choice.machine_type, SSH_HOST);
    assert_eq!(answer.entry.machine_type, SSH_HOST);
    assert_eq!(
        answer.choice.provider_account, caller.account,
        "the choice names the account it would be provisioned through"
    );
    // The caller asked for spot; a host quotes no spot price, so the choice
    // is on demand rather than a capacity mode the machine cannot be held
    // in (the same guard keeps a cloud type whose spot quota is short from
    // being asked for as spot and failing minutes later in the queue).
    assert!(!answer.choice.spot);
}

#[skyzen::test]
async fn a_session_cannot_be_opened_on_an_unlinked_account(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    let body = open(&caller, REPO, 10);

    client
        .delete(&format!("/v1/providers/{}", caller.account))
        .bearer(&caller.token)
        .send()
        .await
        .assert_status(204);

    // The account row survives the unlink (issue #153) so the machines that
    // ran there still name it — but naming it in a new session is naming an
    // account that no longer exists to provision through.
    let refused = client
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(&body)
        .send()
        .await;
    refused.assert_status(404);
    assert_eq!(
        refused.json::<Problem>().kind,
        problem_kind("provider-account-not-found")
    );
}

#[skyzen::test]
async fn there_is_no_default_machine_without_a_linked_account(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");

    let response = ctx
        .client(router)
        .get("/v1/machines/default")
        .bearer(&token)
        .send()
        .await;

    response.assert_status(422);
    assert_eq!(
        response.json::<Problem>().kind,
        problem_kind("no-deployable-linux-machine")
    );
}

#[skyzen::test]
async fn flyco_cannot_choose_a_machine_without_a_deployable_linux_type(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let router = migrated_router(&db).await;
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");

    let response = ctx
        .client(router)
        .post("/v1/sessions")
        .bearer(&token)
        .json(&CreateSession {
            source: None,
            prompt: PROMPT.to_owned(),
            harness: HarnessKind::ClaudeCode,
            repos: vec![flyco_core::RepoSelection {
                repo: REPO.to_owned(),
                branch: None,
            }],
            budget_limit: Usd::from_dollars(10),
            machine: None,
            spot: true,
            model: None,
            permission_mode: None,
        })
        .send()
        .await;

    response.assert_status(422);
    assert_eq!(
        response.json::<Problem>().kind,
        problem_kind("no-deployable-linux-machine")
    );
}

#[skyzen::test]
async fn a_repo_that_is_not_owner_slash_name_is_unprocessable(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;

    let response = ctx
        .client(router)
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(&open(&caller, "not-a-repo", 10))
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
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;

    let response = ctx
        .client(router)
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(&open(&caller, REPO, 0))
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
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    assert_eq!(caller.user.session_cap, 5, "the default cap is five");

    for _ in 0..caller.user.session_cap {
        create(&client, &caller, &open(&caller, REPO, 10)).await;
    }

    let refused = client
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(&open(&caller, REPO, 10))
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
    let sixth = create(&client, &caller, &open(&caller, REPO, 10)).await;

    // Back at the cap, archiving frees a slot.
    client
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(&open(&caller, REPO, 10))
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

    create(&client, &caller, &open(&caller, REPO, 10)).await;
}

#[skyzen::test]
async fn a_session_cap_outside_the_allowed_range_is_unprocessable(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
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

// ── The repositories a session works in ──

#[skyzen::test]
async fn a_session_can_work_across_several_repositories(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);

    let mut body = open(&caller, REPO, 10);
    body.repos = vec![
        flyco_core::RepoSelection {
            repo: REPO.to_owned(),
            branch: None,
        },
        flyco_core::RepoSelection {
            repo: "zen-rs/skyzen".to_owned(),
            branch: Some("main".to_owned()),
        },
    ];
    let session = create(&client, &caller, &body).await;

    let repos = &session.summary.repos;
    assert_eq!(repos.len(), 2);
    // The first selection is the primary checkout, and the branch each
    // repository works on is resolved at creation — flyco's default is
    // `dev`, skyzen's named `main` (testing.rs's fixtures).
    assert_eq!(repos[0].slug.as_str(), REPO);
    assert_eq!(
        repos[0].branch.as_ref().map(flyco_core::BranchName::as_str),
        Some("dev")
    );
    assert_eq!(repos[0].dir, "flyco");
    assert_eq!(repos[0].added_by, flyco_core::RepoAddedBy::User);
    assert_eq!(repos[1].slug.as_str(), "zen-rs/skyzen");
    assert_eq!(
        repos[1].branch.as_ref().map(flyco_core::BranchName::as_str),
        Some("main")
    );
    assert_eq!(repos[1].dir, "skyzen");
}

#[skyzen::test]
async fn two_repositories_sharing_a_name_get_their_own_directories(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);

    let mut body = open(&caller, REPO, 10);
    body.repos = vec![
        flyco_core::RepoSelection {
            repo: REPO.to_owned(),
            branch: None,
        },
        flyco_core::RepoSelection {
            repo: "zen-rs/flyco".to_owned(),
            branch: None,
        },
    ];
    let session = create(&client, &caller, &body).await;

    // The repository's own name is taken first; the second checkout of a
    // `flyco` qualifies with its owner rather than shadowing the first.
    let dirs: Vec<&str> = session
        .summary
        .repos
        .iter()
        .map(|repo| repo.dir.as_str())
        .collect();
    assert_eq!(dirs, ["flyco", "zen-rs--flyco"]);
}

#[skyzen::test]
async fn a_session_naming_no_repository_or_too_many_is_refused(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);

    let mut body = open(&caller, REPO, 10);
    body.repos.clear();
    let none = client
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(&body)
        .send()
        .await;
    none.assert_status(422);
    assert_eq!(none.json::<Problem>().kind, problem_kind("no-repositories"));

    body.repos = (0..=flyco_core::MAX_SESSION_REPOS)
        .map(|n| flyco_core::RepoSelection {
            repo: format!("owner-{n}/repo-{n}"),
            branch: None,
        })
        .collect();
    let over = client
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(&body)
        .send()
        .await;
    over.assert_status(422);
    assert_eq!(
        over.json::<Problem>().kind,
        problem_kind("session-repo-cap-reached")
    );
}

#[skyzen::test]
async fn a_repository_can_be_added_to_a_running_session(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = create(&client, &caller, &open(&caller, REPO, 10)).await;
    let path = format!("/v1/sessions/{}/repos", session.summary.id);

    let added = client
        .post(&path)
        .bearer(&caller.token)
        .json(&flyco_core::RepoSelection {
            repo: "zen-rs/skyzen".to_owned(),
            branch: None,
        })
        .send()
        .await;
    added.assert_status(201);
    let detail: SessionDetail = added.json();
    assert_eq!(detail.summary.repos.len(), 2);
    let skyzen = &detail.summary.repos[1];
    assert_eq!(skyzen.slug.as_str(), "zen-rs/skyzen");
    assert_eq!(skyzen.dir, "skyzen");
    assert_eq!(
        skyzen.branch.as_ref().map(flyco_core::BranchName::as_str),
        Some("main")
    );
    assert_eq!(skyzen.added_by, flyco_core::RepoAddedBy::User);

    // The repository the session already carries cannot be attached twice.
    let again = client
        .post(&path)
        .bearer(&caller.token)
        .json(&flyco_core::RepoSelection {
            repo: "zen-rs/skyzen".to_owned(),
            branch: None,
        })
        .send()
        .await;
    again.assert_status(409);
    assert_eq!(
        again.json::<Problem>().kind,
        problem_kind("repo-already-attached")
    );

    // A slug that is not `owner/name` is refused before GitHub is asked.
    let malformed = client
        .post(&path)
        .bearer(&caller.token)
        .json(&flyco_core::RepoSelection {
            repo: "skyzen".to_owned(),
            branch: None,
        })
        .send()
        .await;
    malformed.assert_status(422);
    assert_eq!(
        malformed.json::<Problem>().kind,
        problem_kind("invalid-repo")
    );
}

#[skyzen::test]
async fn a_repo_add_the_agent_raised_waits_on_the_users_decision(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = create(&client, &caller, &open(&caller, REPO, 10)).await;

    let token: flyco_core::DaemonToken = client
        .post(&format!("/v1/sessions/{}/daemon-token", session.summary.id))
        .bearer(&caller.token)
        .send()
        .await
        .json();

    // A repository the session already carries is refused at raise — a card
    // asking the user to decide on a checkout that exists would be noise.
    let already = client
        .post(&format!("/v1/sessions/{}/approvals", session.summary.id))
        .bearer(&token.token)
        .json(&ApprovalPayload::RepoAdd {
            repo: REPO.to_owned(),
            branch: None,
            reason: "it is right there".to_owned(),
        })
        .send()
        .await;
    already.assert_status(409);
    assert_eq!(
        already.json::<Problem>().kind,
        problem_kind("repo-already-attached")
    );

    let raised = client
        .post(&format!("/v1/sessions/{}/approvals", session.summary.id))
        .bearer(&token.token)
        .json(&ApprovalPayload::RepoAdd {
            repo: "zen-rs/skyzen".to_owned(),
            branch: None,
            reason: "need the router crate".to_owned(),
        })
        .send()
        .await;
    raised.assert_status(201);
    let raised: ApprovalView = raised.json();
    assert_eq!(raised.state, ApprovalState::Pending);
    // The branch the agent left unnamed is resolved at raise, so approving
    // later performs the clone the card described without asking GitHub.
    let ApprovalPayload::RepoAdd { branch, .. } = &raised.payload else {
        panic!("the raise kept its RepoAdd payload");
    };
    assert_eq!(branch.as_deref(), Some("main"));

    let decided = client
        .post(&format!("/v1/approvals/{}/decision", raised.id))
        .bearer(&caller.token)
        .json(&DecideApproval {
            decision: ApprovalDecision::Approved,
        })
        .send()
        .await;
    decided.assert_status(200);

    let detail = client
        .get(&format!("/v1/sessions/{}", session.summary.id))
        .bearer(&caller.token)
        .send()
        .await;
    detail.assert_status(200);
    let repos = detail.json::<SessionDetail>().summary.repos;
    assert_eq!(repos.len(), 2);
    let skyzen = &repos[1];
    assert_eq!(skyzen.slug.as_str(), "zen-rs/skyzen");
    assert_eq!(skyzen.dir, "skyzen");
    assert_eq!(skyzen.added_by, flyco_core::RepoAddedBy::Agent);
}

// ── Lifecycle ──

#[skyzen::test]
async fn a_session_can_be_archived_once(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = create(&client, &caller, &open(&caller, REPO, 10)).await;
    let path = format!("/v1/sessions/{}/archive", session.summary.id);

    let archived = client.post(&path).bearer(&caller.token).send().await;
    archived.assert_status(200);
    assert_eq!(
        archived.json::<SessionDetail>().summary.state,
        SessionState::Archived
    );
    let session_id = session.summary.id;
    let machine_state: MachineState = sql!(
        db,
        "SELECT state FROM machines WHERE session_id = {session_id}"
    )
    .fetch_scalar()
    .await
    .expect("read the archived machine");
    assert_eq!(machine_state, MachineState::Destroyed);

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
async fn an_idle_session_is_archived_automatically(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = create(&client, &caller, &open(&caller, REPO, 10)).await;
    let id = session.summary.id;

    sessions::daemon_arrived(&db, &crate::testing::test_rooms(), id)
        .await
        .expect("the session is live");
    let cutoff = crate::clock::now_unix() - flyco_core::ARCHIVE_AFTER_IDLE_SECS - 1;
    sql!(
        db,
        "UPDATE sessions SET last_active_unix = {cutoff} WHERE id = {id}"
    )
    .execute()
    .await
    .expect("backdate idle time");

    let rooms = crate::rooms::Rooms::from_native(
        crate::rooms::NativeRooms::new(),
        crate::rooms::NativeUserStreams::new(),
    );
    app::archive_idle(
        &db,
        &testing::test_config(),
        &TestGithub::default(),
        &rooms,
        &testing::test_host_rooms(),
        crate::clock::now_unix(),
    )
    .await
    .expect("archive idle sessions");

    let archived: SessionDetail = client
        .get(&format!("/v1/sessions/{id}"))
        .bearer(&caller.token)
        .send()
        .await
        .json();
    assert_eq!(archived.summary.state, SessionState::Archived);
}

// ── The idle suspension sweep ──
//
// `archive_idle` gives a session a week; `suspend_idle` gives its machine
// thirty minutes. A codespace is suspended by GitHub on roughly that clock
// already — every other machine is suspended by flyco, here, through the
// same lifecycle operation the Stop button performs.

/// A live session on a machine the provider already built, the shape
/// [`sessions::suspendable`] selects.
async fn on_a_machine(
    db: &Db,
    hosts: &crate::rooms::HostRooms,
    rooms: &Rooms,
    state: MachineState,
) -> (CurrentUser, SessionId) {
    testing::migrate(db).await;
    let user = seed_user(db).await;
    let session = testing::seed_session(db, &user).await;
    let (host, account) = testing::seed_host_account(db, user.id).await;
    let choice = machine_choice(account);
    let machine = machines::reserve(
        db,
        session,
        account,
        &flyco_core::MachineSpec {
            provider: flyco_core::CloudProviderKind::Host,
            machine_type: choice.machine_type,
            runtime: choice.runtime,
            region: choice.region,
            spot: choice.spot,
            disk_gib: choice.disk_gib,
        },
    )
    .await
    .expect("reserve the machine");
    let native = format!("flyco-{machine}");
    sql!(
        db,
        "UPDATE machines SET state = {state}, native_id = {native} WHERE id = {machine}"
    )
    .execute()
    .await
    .expect("record what the provider built");
    sessions::daemon_arrived(db, rooms, session)
        .await
        .expect("the daemon is live");
    // The stop is a job posted to the host's room, which has to be
    // connected to take it — the attach is what `require_host_online`
    // sees.
    hosts
        .attach(
            host,
            &flyco_provider::host::HostAttach {
                facts: Box::new(testing::host_facts()),
            },
        )
        .await
        .expect("the host is attached to its room");
    (user, session)
}

/// Backdates a session's activity clock past the suspend threshold.
async fn idle(db: &Db, session: SessionId) {
    let cutoff = crate::clock::now_unix() - flyco_core::SUSPEND_AFTER_IDLE_SECS - 1;
    sql!(
        db,
        "UPDATE sessions SET last_active_unix = {cutoff} WHERE id = {session}"
    )
    .execute()
    .await
    .expect("backdate idle time");
}

#[skyzen::test]
async fn an_idle_sessions_machine_is_suspended_with_its_disk_kept(_ctx: TestContext, db: Db) {
    let hosts = testing::test_host_rooms();
    let rooms = crate::testing::test_rooms();
    let (user, session) = on_a_machine(&db, &hosts, &rooms, MachineState::Running).await;
    idle(&db, session).await;

    let config = testing::test_config();
    app::suspend_idle(
        &db,
        &config,
        &TestGithub::default(),
        &rooms,
        &hosts,
        crate::clock::now_unix(),
    )
    .await
    .expect("the sweep runs");

    let machine = machines::for_session(&db, session)
        .await
        .expect("read the machine")
        .expect("the session has one");
    assert_eq!(machine.state, MachineState::Deallocated, "compute released");
    assert!(
        machine.native_id.is_some(),
        "the disk — and the name that starts it again — is kept"
    );

    let detail = sessions::find(&db, user.id, session)
        .await
        .expect("read the session");
    assert_eq!(detail.summary.state, SessionState::Interrupted);
    assert_eq!(
        detail.summary.interrupted_reason,
        Some(flyco_core::InterruptedReason::Suspended),
        "the reason is what a message or a resume reads to wake it"
    );

    let events = rooms
        .events(session, 0)
        .await
        .expect("read the room's stream")
        .events;
    assert!(
        events.iter().any(|stored| matches!(
            serde_json::from_value::<flyco_core::ClientEvent>(stored.event.clone()),
            Ok(flyco_core::ClientEvent::SessionStateChanged {
                state: SessionState::Interrupted
            })
        )),
        "the session's watchers are told: {events:?}"
    );

    // And a second pass — Cloudflare overlaps crons — finds nothing to do:
    // an interrupted session is not in the set.
    app::suspend_idle(
        &db,
        &config,
        &TestGithub::default(),
        &rooms,
        &hosts,
        crate::clock::now_unix(),
    )
    .await
    .expect("the sweep runs again");
    assert_eq!(
        sessions::find(&db, user.id, session)
            .await
            .expect("read the session")
            .summary
            .state,
        SessionState::Interrupted
    );
}

#[skyzen::test]
async fn a_machine_held_awake_survives_the_sweep_until_the_hold_runs_out(
    _ctx: TestContext,
    db: Db,
) {
    // The sweep cannot see a build, a soak test or a watch loop: the
    // session is idle because the work is the machine's, not the user's.
    // The hold is the only evidence there is, and it expires on its own so
    // a machine is never awake because somebody forgot it.
    let hosts = testing::test_host_rooms();
    let rooms = crate::testing::test_rooms();
    let (user, session) = on_a_machine(&db, &hosts, &rooms, MachineState::Running).await;
    idle(&db, session).await;

    let held = sessions::keep_awake(&db, user.id, session, Some(60))
        .await
        .expect("hold the machine awake");
    assert!(
        held.summary
            .awake_until_unix
            .is_some_and(|until| until > crate::clock::now_unix()),
        "the hold is an instant, not a flag: {:?}",
        held.summary.awake_until_unix
    );

    let config = testing::test_config();
    app::suspend_idle(
        &db,
        &config,
        &TestGithub::default(),
        &rooms,
        &hosts,
        crate::clock::now_unix(),
    )
    .await
    .expect("the sweep runs");
    assert_eq!(
        sessions::find(&db, user.id, session)
            .await
            .expect("read the session")
            .summary
            .state,
        SessionState::Active,
        "a held machine is left alone"
    );

    // An hour later the hold is over and the machine is the sweep's again.
    app::suspend_idle(
        &db,
        &config,
        &TestGithub::default(),
        &rooms,
        &hosts,
        crate::clock::now_unix() + 61 * 60,
    )
    .await
    .expect("the sweep runs after the hold");
    assert_eq!(
        sessions::find(&db, user.id, session)
            .await
            .expect("read the session")
            .summary
            .state,
        SessionState::Interrupted
    );
}

#[skyzen::test]
async fn a_hold_that_has_run_out_is_not_reported_as_one(_ctx: TestContext, db: Db) {
    // Cleared where it is read rather than swept: a row still carrying
    // yesterday's instant would put "kept awake" in the menu of a machine
    // the sweep is already free to stop.
    testing::migrate(&db).await;
    let user = seed_user(&db).await;
    let session = testing::seed_session(&db, &user).await;
    let past = crate::clock::now_unix() - 1;
    sql!(
        db,
        "UPDATE sessions SET awake_until_unix = {past} WHERE id = {session}"
    )
    .execute()
    .await
    .expect("write a hold that has passed");

    assert_eq!(
        sessions::find(&db, user.id, session)
            .await
            .expect("read the session")
            .summary
            .awake_until_unix,
        None
    );
}

#[skyzen::test]
async fn ending_the_hold_gives_the_machine_back_to_the_sweep(_ctx: TestContext, db: Db) {
    let hosts = testing::test_host_rooms();
    let rooms = crate::testing::test_rooms();
    let (user, session) = on_a_machine(&db, &hosts, &rooms, MachineState::Running).await;
    idle(&db, session).await;
    sessions::keep_awake(&db, user.id, session, Some(60))
        .await
        .expect("hold the machine awake");

    let released = sessions::keep_awake(&db, user.id, session, None)
        .await
        .expect("end the hold");
    assert_eq!(released.summary.awake_until_unix, None);

    app::suspend_idle(
        &db,
        &testing::test_config(),
        &TestGithub::default(),
        &rooms,
        &hosts,
        crate::clock::now_unix(),
    )
    .await
    .expect("the sweep runs");
    assert_eq!(
        sessions::find(&db, user.id, session)
            .await
            .expect("read the session")
            .summary
            .state,
        SessionState::Interrupted
    );
}

#[skyzen::test]
async fn a_hold_longer_than_a_working_day_is_refused(_ctx: TestContext, db: Db) {
    testing::migrate(&db).await;
    let user = seed_user(&db).await;
    let session = testing::seed_session(&db, &user).await;

    let refused = sessions::keep_awake(
        &db,
        user.id,
        session,
        Some(flyco_core::KEEP_AWAKE_MAX_MINUTES + 1),
    )
    .await
    .expect_err("a hold past the maximum");
    assert_eq!(
        refused.problem().kind,
        "https://flyco.dev/problems/keep-awake-too-long"
    );
    assert_eq!(
        sessions::find(&db, user.id, session)
            .await
            .expect("read the session")
            .summary
            .awake_until_unix,
        None,
        "a refused hold writes nothing"
    );
}

/// Backdates a pending approval so it has waited as long as the idle
/// threshold: the blocked turn's own clock (issue #355).
async fn waiting_since_the_cutoff(db: &Db, approval: ApprovalId) {
    let cutoff = crate::clock::now_unix() - flyco_core::SUSPEND_AFTER_IDLE_SECS - 1;
    sql!(
        db,
        "UPDATE approvals SET created_at_unix = {cutoff} WHERE id = {approval}"
    )
    .execute()
    .await
    .expect("backdate the approval");
}

async fn blocked_on_an_approval(db: &Db, session: SessionId) -> ApprovalId {
    sessions::record_activity(db, session, flyco_core::SessionActivity::Working)
        .await
        .expect("a turn is in flight");
    approvals::raise(
        db,
        session,
        &ApprovalPayload::ToolUse {
            tool: "Bash".to_owned(),
            input: serde_json::json!({ "command": "git push" }),
        },
    )
    .await
    .expect("the turn asks")
}

#[skyzen::test]
async fn a_turn_blocked_on_an_approval_past_the_idle_threshold_is_suspended(
    _ctx: TestContext,
    db: Db,
) {
    let hosts = testing::test_host_rooms();
    let rooms = crate::testing::test_rooms();
    let (user, session) = on_a_machine(&db, &hosts, &rooms, MachineState::Running).await;
    let approval = blocked_on_an_approval(&db, session).await;
    idle(&db, session).await;
    waiting_since_the_cutoff(&db, approval).await;

    app::suspend_idle(
        &db,
        &testing::test_config(),
        &TestGithub::default(),
        &rooms,
        &hosts,
        crate::clock::now_unix(),
    )
    .await
    .expect("the sweep runs");

    let detail = sessions::find(&db, user.id, session)
        .await
        .expect("read the session");
    assert_eq!(
        detail.summary.state,
        SessionState::Interrupted,
        "an agent that has waited on the user this long is not working"
    );
    assert_eq!(
        detail.summary.interrupted_reason,
        Some(flyco_core::InterruptedReason::Suspended)
    );
    assert_eq!(
        machines::for_session(&db, session)
            .await
            .expect("read the machine")
            .expect("the session has one")
            .state,
        MachineState::Deallocated,
        "compute released, disk kept"
    );
}

#[skyzen::test]
async fn a_turn_blocked_on_a_fresh_approval_keeps_its_machine(_ctx: TestContext, db: Db) {
    let hosts = testing::test_host_rooms();
    let rooms = crate::testing::test_rooms();
    let (user, session) = on_a_machine(&db, &hosts, &rooms, MachineState::Running).await;
    blocked_on_an_approval(&db, session).await;
    // The turn is old, the question is not.
    idle(&db, session).await;

    app::suspend_idle(
        &db,
        &testing::test_config(),
        &TestGithub::default(),
        &rooms,
        &hosts,
        crate::clock::now_unix(),
    )
    .await
    .expect("the sweep runs");

    assert_eq!(
        sessions::find(&db, user.id, session)
            .await
            .expect("read the session")
            .summary
            .state,
        SessionState::Active,
        "the blocked turn's clock is the approval's age, not the turn's"
    );
}

#[skyzen::test]
async fn a_session_mid_turn_keeps_its_machine(_ctx: TestContext, db: Db) {
    let hosts = testing::test_host_rooms();
    let rooms = crate::testing::test_rooms();
    let (user, session) = on_a_machine(&db, &hosts, &rooms, MachineState::Running).await;
    sessions::record_activity(&db, session, flyco_core::SessionActivity::Working)
        .await
        .expect("a turn is in flight");
    idle(&db, session).await;

    app::suspend_idle(
        &db,
        &testing::test_config(),
        &TestGithub::default(),
        &rooms,
        &hosts,
        crate::clock::now_unix(),
    )
    .await
    .expect("the sweep runs");

    let detail = sessions::find(&db, user.id, session)
        .await
        .expect("read the session");
    assert_eq!(
        detail.summary.state,
        SessionState::Active,
        "a suspension that killed the agent mid-answer would lose the work"
    );
    assert_eq!(
        machines::for_session(&db, session)
            .await
            .expect("read the machine")
            .expect("the session has one")
            .state,
        MachineState::Running
    );
}

#[skyzen::test]
async fn a_session_still_in_conversation_keeps_its_machine(_ctx: TestContext, db: Db) {
    let hosts = testing::test_host_rooms();
    let rooms = crate::testing::test_rooms();
    let (user, session) = on_a_machine(&db, &hosts, &rooms, MachineState::Running).await;

    app::suspend_idle(
        &db,
        &testing::test_config(),
        &TestGithub::default(),
        &rooms,
        &hosts,
        crate::clock::now_unix(),
    )
    .await
    .expect("the sweep runs");

    assert_eq!(
        sessions::find(&db, user.id, session)
            .await
            .expect("read the session")
            .summary
            .state,
        SessionState::Active,
        "thirty minutes has not passed"
    );
    assert_eq!(
        machines::for_session(&db, session)
            .await
            .expect("read the machine")
            .expect("the session has one")
            .state,
        MachineState::Running
    );
}

/// The crash gap and the Stop button are the same shape — an active
/// session on a machine already off — and the sweep owes it the session
/// write the stop never made.
#[skyzen::test]
async fn a_machine_stopped_underneath_an_idle_session_is_recorded_suspended(
    _ctx: TestContext,
    db: Db,
) {
    let hosts = testing::test_host_rooms();
    let rooms = crate::testing::test_rooms();
    let (user, session) = on_a_machine(&db, &hosts, &rooms, MachineState::Deallocated).await;
    idle(&db, session).await;

    app::suspend_idle(
        &db,
        &testing::test_config(),
        &TestGithub::default(),
        &rooms,
        &hosts,
        crate::clock::now_unix(),
    )
    .await
    .expect("the sweep runs");

    let detail = sessions::find(&db, user.id, session)
        .await
        .expect("read the session");
    assert_eq!(detail.summary.state, SessionState::Interrupted);
    assert_eq!(
        detail.summary.interrupted_reason,
        Some(flyco_core::InterruptedReason::Suspended)
    );
}

/// A pause is its own mechanism with its own machine decision — a kept
/// machine is kept because the reset is near, and the suspend sweep is
/// not a second opinion about it.
#[skyzen::test]
async fn a_paused_sessions_machine_is_the_usage_limit_sweeps_call(_ctx: TestContext, db: Db) {
    let hosts = testing::test_host_rooms();
    let rooms = crate::testing::test_rooms();
    let (user, session) = on_a_machine(&db, &hosts, &rooms, MachineState::Running).await;
    let config = testing::test_config();
    let resets_at = crate::clock::now_unix() + 12 * 60;
    usage_limits::pause(
        &db,
        &config,
        &rooms,
        session,
        &flyco_core::UsageWindow::new(
            Some(300),
            None,
            100,
            Some(i64::try_from(resets_at).expect("a reset time fits")),
        ),
        crate::clock::now_unix(),
    )
    .await
    .expect("the limit is recorded");
    idle(&db, session).await;

    app::suspend_idle(
        &db,
        &config,
        &TestGithub::default(),
        &rooms,
        &hosts,
        crate::clock::now_unix(),
    )
    .await
    .expect("the sweep runs");

    assert_eq!(
        sessions::find(&db, user.id, session)
            .await
            .expect("read the session")
            .summary
            .state,
        SessionState::Paused
    );
    assert_eq!(
        machines::for_session(&db, session)
            .await
            .expect("read the machine")
            .expect("the session has one")
            .state,
        MachineState::Running,
        "its reset is twelve minutes out — stopping it buys nothing"
    );
}

/// A machine that cannot be asked — its host is not connected — leaves
/// the session alone, and the next pass asks again.
#[skyzen::test]
async fn an_offline_hosts_machine_is_left_for_the_next_pass(_ctx: TestContext, db: Db) {
    let rooms = crate::testing::test_rooms();
    // The host is seeded and the machine built on one rooms instance, then
    // the sweep runs against one that never saw the attach — what a room
    // holding no connection reports.
    let attached = testing::test_host_rooms();
    let (user, session) = on_a_machine(&db, &attached, &rooms, MachineState::Running).await;
    idle(&db, session).await;

    app::suspend_idle(
        &db,
        &testing::test_config(),
        &TestGithub::default(),
        &rooms,
        &testing::test_host_rooms(),
        crate::clock::now_unix(),
    )
    .await
    .expect("one unreachable host does not stop the sweep");

    assert_eq!(
        sessions::find(&db, user.id, session)
            .await
            .expect("read the session")
            .summary
            .state,
        SessionState::Active
    );
    assert_eq!(
        machines::for_session(&db, session)
            .await
            .expect("read the machine")
            .expect("the session has one")
            .state,
        MachineState::Running
    );
}

/// An approval answered on a suspended session is a wake, because the
/// daemon that re-reads it has no machine until one is started.
#[skyzen::test]
async fn a_decision_on_a_suspended_sessions_approval_wakes_its_machine(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let hosts = testing::test_host_rooms();
    let rooms = crate::testing::test_rooms();
    let (user, session) = on_a_machine(&db, &hosts, &rooms, MachineState::Running).await;
    idle(&db, session).await;
    app::suspend_idle(
        &db,
        &testing::test_config(),
        &TestGithub::default(),
        &rooms,
        &hosts,
        crate::clock::now_unix(),
    )
    .await
    .expect("the sweep suspends it");

    let approval = approvals::raise(
        &db,
        session,
        &flyco_core::wire::ApprovalPayload::HistoryRewrite {
            repo: REPO.to_owned(),
            branch: BRANCH.to_owned(),
            description: "rebase the three commits".to_owned(),
        },
    )
    .await
    .expect("raise an approval");

    let backend = skyzen_test::mock::InMemoryQueue::new();
    let client = ctx.client(testing::test_router(
        db.clone(),
        skyzen_services::Queue::new(backend.clone()),
    ));
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    client
        .post(&format!("/v1/approvals/{approval}/decision"))
        .bearer(&token)
        .json(&flyco_core::DecideApproval {
            decision: flyco_core::wire::ApprovalDecision::Approved,
        })
        .send()
        .await
        .assert_status(200);

    let jobs: Vec<crate::provisioning_queue::ProvisioningJob> = backend
        .messages()
        .iter()
        .map(|body| serde_json::from_slice(body).expect("a queued provisioning job"))
        .collect();
    assert!(
        jobs.iter().any(|job| matches!(
            job,
            crate::provisioning_queue::ProvisioningJob::Recover {
                session: queued,
                cause: crate::provisioning_queue::RecoveryCause::Suspended,
                ..
            } if *queued == session
        )),
        "the decision started the machine back: {jobs:?}"
    );
}

#[skyzen::test]
async fn an_active_session_cannot_be_archived_before_the_daemon_reports_its_tree(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = create(&client, &caller, &open(&caller, REPO, 10)).await;
    let id = session.summary.id;
    sessions::daemon_arrived(&db, &crate::testing::test_rooms(), id)
        .await
        .expect("the session is live");

    let refused = client
        .post(&format!("/v1/sessions/{id}/archive"))
        .bearer(&caller.token)
        .send()
        .await;
    refused.assert_status(404);
    assert_eq!(
        refused.json::<Problem>().kind,
        problem_kind("repo-status-unknown")
    );
}

#[skyzen::test]
async fn sessions_are_listed_newest_first(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);

    let older = create(&client, &caller, &open(&caller, REPO, 10)).await;
    let newer = create(&client, &caller, &open(&caller, "lexoliu/skyzen", 10)).await;

    // Both were created inside the same second, so backdate one: the
    // assertion is about the ordering contract, not about how fast the test
    // machine is.
    let older_id = older.summary.id;
    sql!(
        db,
        "UPDATE sessions SET created_at_unix = created_at_unix - 60 WHERE id = {older_id}"
    )
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
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = create(&client, &caller, &open(&caller, REPO, 10)).await;
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
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    // Under $5, so the engine stays silent below 90% — the stage still moves.
    let session = create(&client, &caller, &open(&caller, REPO, 4)).await;
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
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = create(&client, &caller, &open(&caller, REPO, 10)).await;
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

    let row: Row = sql!(
        db,
        "SELECT spent_micros, stage FROM budgets WHERE id = {budget}"
    )
    .fetch_one()
    .await
    .expect("read the budget row");

    assert_eq!(
        row.get::<i64>("spent_micros").expect("spent_micros"),
        9_000_000
    );
    assert_eq!(row.get::<String>("stage").expect("stage"), "final90");
}

// ── Approvals ──

#[skyzen::test]
async fn approvals_are_listed_filtered_and_decided_once(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = create(&client, &caller, &open(&caller, REPO, 10)).await;
    let other_session = create(&client, &caller, &open(&caller, "lexoliu/skyzen", 10)).await;

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
    let owner = sign_in(&kv, &db, seed_user(&db).await).await;
    let stranger = sign_in(&kv, &db, seed_other_user(&db).await).await;
    let client = ctx.client(router);

    let session = create(&client, &owner, &open(&owner, REPO, 10)).await;
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
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;

    let response = ctx
        .client(router)
        .get(&format!("/v1/sessions/{}", SessionId::generate()))
        .bearer(&caller.token)
        .send()
        .await;

    response.assert_status(404);
}

// ── Raising an exhausted budget ──

/// Spends a session's whole budget the way the meter does, and lets the
/// outbox deliver the pause — so the session under test is paused for the
/// one reason a session is ever paused.
async fn exhaust_the_budget(db: &Db, caller: &Caller, session: SessionId, dollars: u64) {
    sessions::transition(db, caller.user.id, session, SessionState::Active)
        .await
        .expect("a provisioning session becomes active when its daemon arrives");
    budgets::record(
        db,
        budget_id(db, session).await,
        SpendKind::Compute,
        Usd::from_dollars(dollars),
        "machine time",
    )
    .await
    .expect("record spend");
    metering::deliver(
        db,
        &Rooms::from_native(NativeRooms::new(), crate::rooms::NativeUserStreams::new()),
    )
    .await
    .expect("deliver the pause");
}

#[skyzen::test]
async fn raising_the_budget_past_the_spend_releases_a_paused_session(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = create(&client, &caller, &open(&caller, REPO, 10)).await;
    let id = session.summary.id;
    let path = format!("/v1/sessions/{id}");
    exhaust_the_budget(&db, &caller, id, 10).await;

    // A raise that is still under the spend is a legitimate change to the
    // limit and nothing more: the money is spent either way, so the session
    // stays paused rather than being released onto a budget it has already
    // overrun.
    let under = client
        .patch(&path)
        .bearer(&caller.token)
        .json(&rebudget(9))
        .send()
        .await;
    under.assert_status(200);
    let under: SessionDetail = under.json();
    assert_eq!(under.summary.state, SessionState::Paused);
    assert_eq!(under.budget.limit, Usd::from_dollars(9));
    assert_eq!(under.budget.stage, BudgetStage::Exhausted);

    let raised = client
        .patch(&path)
        .bearer(&caller.token)
        .json(&rebudget(25))
        .send()
        .await;
    raised.assert_status(200);
    let raised: SessionDetail = raised.json();
    assert_eq!(
        raised.summary.state,
        SessionState::Active,
        "a budget with room left in it releases the session it paused"
    );
    assert_eq!(raised.budget.limit, Usd::from_dollars(25));
    assert_eq!(raised.budget.spent, Usd::from_dollars(10));
    assert_eq!(raised.budget.remaining, Usd::from_dollars(15));
    assert_eq!(
        raised.budget.stage,
        BudgetStage::Ok,
        "$10 of a $25 budget is below every threshold, so the replay says so"
    );

    // The ledger is untouched by any of it: a limit is re-read against the
    // same history, never an adjustment to it.
    let events: u32 = sql!(
        db,
        "SELECT COUNT(*) AS events FROM spend_events \
         WHERE budget_id = {budget_id(&db, id).await}"
    )
    .fetch_scalar()
    .await
    .expect("count the ledger");
    assert_eq!(events, 1);
}

#[skyzen::test]
async fn a_budget_raised_and_spent_again_pauses_again(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = create(&client, &caller, &open(&caller, REPO, 10)).await;
    let id = session.summary.id;
    exhaust_the_budget(&db, &caller, id, 10).await;

    client
        .patch(&format!("/v1/sessions/{id}"))
        .bearer(&caller.token)
        .json(&rebudget(20))
        .send()
        .await
        .assert_status(200);

    // The outbox is keyed `UNIQUE (budget_id, signal)`, so a pause row left
    // behind by the first exhaustion would silence the second one for good.
    budgets::record(
        &db,
        budget_id(&db, id).await,
        SpendKind::Compute,
        Usd::from_dollars(10),
        "machine time",
    )
    .await
    .expect("spend the raised budget too");
    assert!(
        budgets::pending_signals(&db)
            .await
            .expect("outbox")
            .iter()
            .any(|pending| pending.session_id == id
                && pending.signal == flyco_core::BudgetSignal::Pause),
        "exhausting a raised budget announces the pause again"
    );

    metering::deliver(
        &db,
        &Rooms::from_native(NativeRooms::new(), crate::rooms::NativeUserStreams::new()),
    )
    .await
    .expect("deliver the second pause");
    assert_eq!(
        sessions::state_of(&db, caller.user.id, id)
            .await
            .expect("state"),
        SessionState::Paused
    );
}

#[skyzen::test]
async fn an_update_refuses_a_budget_nothing_can_run_on_and_a_body_that_says_nothing(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = create(&client, &caller, &open(&caller, REPO, 10)).await;
    let path = format!("/v1/sessions/{}", session.summary.id);

    let zero = client
        .patch(&path)
        .bearer(&caller.token)
        .json(&rebudget(0))
        .send()
        .await;
    zero.assert_status(422);
    assert_eq!(zero.json::<Problem>().kind, problem_kind("invalid-budget"));

    let empty = client
        .patch(&path)
        .bearer(&caller.token)
        .json(&UpdateSession::default())
        .send()
        .await;
    empty.assert_status(422);
    assert_eq!(empty.json::<Problem>().kind, problem_kind("empty-update"));

    // One `PATCH` may carry both, and the answer describes both.
    let both = client
        .patch(&path)
        .bearer(&caller.token)
        .json(&UpdateSession {
            title: Some("Rework the relay mailbox".to_owned()),
            budget_limit: Some(Usd::from_dollars(30)),
            model: None,
            permission_mode: None,
            computer_use: Some(false),
        })
        .send()
        .await;
    both.assert_status(200);
    let both: SessionDetail = both.json();
    assert_eq!(both.summary.title, "Rework the relay mailbox");
    assert_eq!(both.budget.limit, Usd::from_dollars(30));

    // And a budget is the owner's to set, like every other thing about a
    // session.
    let stranger = sign_in(&kv, &db, seed_other_user(&db).await).await;
    client
        .patch(&path)
        .bearer(&stranger.token)
        .json(&rebudget(50))
        .send()
        .await
        .assert_status(404);
}

async fn budget_id(db: &Db, session: SessionId) -> flyco_core::BudgetId {
    let budget_id: String = sql!(db, "SELECT budget_id FROM sessions WHERE id = {session}")
        .fetch_scalar()
        .await
        .expect("read the session row");
    budget_id.parse().expect("budget_id is a UUID")
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
        owner.session_cap,
        sessions::Opening {
            user: owner.id,
            title: "check ownership",
            harness: HarnessKind::Codex,
            repos: &[sessions::RepoOpening {
                slug: REPO.parse().expect("valid repo"),
                branch: BRANCH.parse().expect("valid branch"),
            }],
            machine_origin: flyco_core::MachineOrigin::Auto,
            budget: flyco_core::BudgetConfig::new(Usd::from_dollars(1)).expect("non-zero"),
            model: &flyco_core::ModelChoice::default_of(&flyco_core::builtin_models(
                HarnessKind::Codex,
            )),
            permission_mode: None,
        },
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

// ── A session's `.env` ──

const SECRET: &str = "gho_a-token-nobody-should-see";

fn env(entries: &[(&str, &str)]) -> UpdateEnv {
    UpdateEnv {
        entries: entries
            .iter()
            .map(|(key, value)| EnvEntry {
                key: (*key).to_owned(),
                value: (*value).to_owned(),
            })
            .collect(),
    }
}

#[skyzen::test]
async fn an_unconfigured_session_has_an_empty_environment(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let client = ctx.client(router);
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let session = create(&client, &caller, &open(&caller, REPO, 10))
        .await
        .summary
        .id;

    let response = client
        .get(&format!("/v1/sessions/{session}/env"))
        .bearer(&caller.token)
        .send()
        .await;
    response.assert_status(200);
    let document: EnvDocument = response.json();
    assert_eq!(document.entries, Vec::new());
    assert_eq!(document.warning, flyco_core::NETWORK_CONTROL_WARNING);
}

#[skyzen::test]
async fn a_replaced_environment_reads_back_in_the_order_it_was_given(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let router = migrated_router(&db).await;
    let client = ctx.client(router);
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let session = create(&client, &caller, &open(&caller, REPO, 10))
        .await
        .summary
        .id;

    let written = client
        .put(&format!("/v1/sessions/{session}/env"))
        .bearer(&caller.token)
        .json(&env(&[
            ("GITHUB_TOKEN", SECRET),
            ("DATABASE_URL", "sqlite://x"),
        ]))
        .send()
        .await;
    written.assert_status(200);
    assert_eq!(
        written.json::<EnvDocument>().warning,
        flyco_core::NETWORK_CONTROL_WARNING,
        "a write answers with the same caveat a read does"
    );

    let read = client
        .get(&format!("/v1/sessions/{session}/env"))
        .bearer(&caller.token)
        .send()
        .await;
    read.assert_status(200);
    let document: EnvDocument = read.json();
    assert_eq!(
        document
            .entries
            .iter()
            .map(|entry| entry.key.as_str())
            .collect::<Vec<_>>(),
        vec!["GITHUB_TOKEN", "DATABASE_URL"]
    );
    assert_eq!(document.entries[0].value, SECRET);

    // A second write replaces the document rather than merging into it.
    client
        .put(&format!("/v1/sessions/{session}/env"))
        .bearer(&caller.token)
        .json(&env(&[("ONLY", "one")]))
        .send()
        .await
        .assert_status(200);
    let after = client
        .get(&format!("/v1/sessions/{session}/env"))
        .bearer(&caller.token)
        .send()
        .await;
    assert_eq!(after.json::<EnvDocument>().entries.len(), 1);
}

#[skyzen::test]
async fn a_stored_environment_is_sealed_at_rest(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let client = ctx.client(router);
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let session = create(&client, &caller, &open(&caller, REPO, 10))
        .await
        .summary
        .id;

    client
        .put(&format!("/v1/sessions/{session}/env"))
        .bearer(&caller.token)
        .json(&env(&[("GITHUB_TOKEN", SECRET)]))
        .send()
        .await
        .assert_status(200);

    let stored: String = sql!(
        db,
        "SELECT entries_enc FROM session_env WHERE session_id = {session}"
    )
    .fetch_scalar()
    .await
    .expect("read the stored environment");
    let stored = &stored;
    assert!(!stored.contains(SECRET), "the value is stored in the clear");
    assert!(
        !stored.contains("GITHUB_TOKEN"),
        "the name is stored in the clear"
    );
}

#[skyzen::test]
async fn a_name_no_shell_can_export_is_unprocessable(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let client = ctx.client(router);
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let session = create(&client, &caller, &open(&caller, REPO, 10))
        .await
        .summary
        .id;

    let refused = client
        .put(&format!("/v1/sessions/{session}/env"))
        .bearer(&caller.token)
        .json(&env(&[("NOT A NAME", "x")]))
        .send()
        .await;
    refused.assert_status(422);
    assert_eq!(
        refused.json::<Problem>().kind,
        problem_kind("invalid-env-key")
    );

    // Nothing partial was written: the whole document is refused.
    let read = client
        .get(&format!("/v1/sessions/{session}/env"))
        .bearer(&caller.token)
        .send()
        .await;
    assert_eq!(
        read.json::<EnvDocument>().entries,
        Vec::new(),
        "nothing partial was written"
    );
}

#[skyzen::test]
async fn another_users_environment_is_not_found(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let client = ctx.client(router);
    let owner = sign_in(&kv, &db, seed_user(&db).await).await;
    let stranger = sign_in(&kv, &db, seed_other_user(&db).await).await;
    let session = create(&client, &owner, &open(&owner, REPO, 10))
        .await
        .summary
        .id;

    client
        .put(&format!("/v1/sessions/{session}/env"))
        .bearer(&owner.token)
        .json(&env(&[("GITHUB_TOKEN", SECRET)]))
        .send()
        .await
        .assert_status(200);

    let read = client
        .get(&format!("/v1/sessions/{session}/env"))
        .bearer(&stranger.token)
        .send()
        .await;
    read.assert_status(404);
    assert_eq!(
        read.json::<Problem>().kind,
        problem_kind("session-not-found")
    );

    client
        .put(&format!("/v1/sessions/{session}/env"))
        .bearer(&stranger.token)
        .json(&env(&[("MINE", "now")]))
        .send()
        .await
        .assert_status(404);
}

// ── The model a session runs on ──

/// A `PATCH /v1/sessions/{id}` body that only changes the model.
fn remodel(model: &str, effort: Option<&str>) -> UpdateSession {
    UpdateSession {
        model: Some(ModelChoice {
            model: model.to_owned(),
            effort: effort.map(str::to_owned),
        }),
        ..UpdateSession::default()
    }
}

#[skyzen::test]
async fn a_session_opened_without_a_model_carries_the_harnesss_default(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    // Resolved at creation rather than left blank: the header states the
    // model on every row, so a session nobody chose one for must still name
    // what it runs.
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);

    let session = create(&client, &caller, &open(&caller, REPO, 10)).await;
    assert_eq!(
        session.summary.model,
        ModelChoice::default_of(&builtin_models(HarnessKind::ClaudeCode))
    );
}

#[skyzen::test]
async fn a_session_opened_with_a_model_carries_exactly_that_one(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);

    let chosen = ModelChoice {
        model: "sonnet".to_owned(),
        effort: Some("high".to_owned()),
    };
    let session = create(
        &client,
        &caller,
        &CreateSession {
            source: None,
            model: Some(chosen.clone()),
            ..open(&caller, REPO, 10)
        },
    )
    .await;
    assert_eq!(session.summary.model, chosen);

    // And it survives the round trip through the row rather than only the
    // response the create built.
    let listed = client
        .get("/v1/sessions")
        .bearer(&caller.token)
        .send()
        .await;
    listed.assert_status(200);
    let listed: Vec<SessionSummary> = listed.json();
    assert_eq!(listed[0].model, chosen);
}

#[skyzen::test]
async fn a_model_the_harness_does_not_offer_is_refused_before_anything_is_written(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);

    let response = client
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(&CreateSession {
            source: None,
            model: Some(ModelChoice {
                model: "nope".to_owned(),
                effort: None,
            }),
            ..open(&caller, REPO, 10)
        })
        .send()
        .await;
    response.assert_status(400);
    let problem: Problem = response.json();
    assert_eq!(problem.kind, problem_kind("invalid-model"));
    assert!(problem.detail.contains("nope"), "{problem:?}");

    // Nothing was opened: the refusal happened before the row.
    let listed = client
        .get("/v1/sessions")
        .bearer(&caller.token)
        .send()
        .await;
    listed.assert_status(200);
    assert!(listed.json::<Vec<SessionSummary>>().is_empty());
}

#[skyzen::test]
async fn an_effort_the_model_does_not_accept_is_refused_too(ctx: TestContext, kv: Kv, db: Db) {
    // Haiku names no effort levels at all, so every effort is one it
    // refuses — and the refusal says which model, because "invalid model"
    // alone leaves the user guessing which half they got wrong.
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);

    let response = client
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(&CreateSession {
            source: None,
            model: Some(ModelChoice {
                model: "haiku".to_owned(),
                effort: Some("max".to_owned()),
            }),
            ..open(&caller, REPO, 10)
        })
        .send()
        .await;
    response.assert_status(400);
    let problem: Problem = response.json();
    assert_eq!(problem.kind, problem_kind("invalid-model"));
    assert!(problem.detail.contains("haiku"), "{problem:?}");
}

#[skyzen::test]
async fn changing_the_model_records_it_on_the_session(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = create(&client, &caller, &open(&caller, REPO, 10)).await;
    let path = format!("/v1/sessions/{}", session.summary.id);

    let changed = client
        .patch(&path)
        .bearer(&caller.token)
        .json(&remodel("opus[1m]", Some("max")))
        .send()
        .await;
    changed.assert_status(200);
    let changed: SessionDetail = changed.json();
    assert_eq!(
        changed.summary.model,
        ModelChoice {
            model: "opus[1m]".to_owned(),
            effort: Some("max".to_owned()),
        }
    );

    // And it is the row that changed, not just the answer: the next read
    // of the session names the new model.
    let reread = client.get(&path).bearer(&caller.token).send().await;
    reread.assert_status(200);
    assert_eq!(
        reread.json::<SessionDetail>().summary.model,
        ModelChoice {
            model: "opus[1m]".to_owned(),
            effort: Some("max".to_owned()),
        }
    );

    // The effort is cleared by naming the model without one, rather than
    // kept from the change before it: a choice is the pair, and half of an
    // old one is a combination nobody picked.
    let cleared = client
        .patch(&path)
        .bearer(&caller.token)
        .json(&remodel("opus[1m]", None))
        .send()
        .await;
    cleared.assert_status(200);
    assert_eq!(cleared.json::<SessionDetail>().summary.model.effort, None);
}

#[skyzen::test]
async fn a_model_the_accounts_stored_list_dropped_is_refused_at_patch(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    // A picker rendered from yesterday's list is exactly the case this
    // guards: the account's harness has since reported what it offers, and
    // a model outside that list is one no session can run.
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = create(&client, &caller, &open(&caller, REPO, 10)).await;
    let path = format!("/v1/sessions/{}", session.summary.id);

    testing::seed_harness_account(&db, caller.user.id, HarnessKind::ClaudeCode).await;
    harness_accounts::record_models(
        &db,
        caller.user.id,
        HarnessKind::ClaudeCode,
        &[ModelOption {
            id: "claude-opus-6".to_owned(),
            label: "Opus 6".to_owned(),
            description: "The only model this build still offers.".to_owned(),
            is_default: true,
            efforts: vec!["high".to_owned()],
            default_effort: None,
        }],
    )
    .await
    .expect("record what the harness offers");

    let refused = client
        .patch(&path)
        .bearer(&caller.token)
        .json(&remodel("sonnet", None))
        .send()
        .await;
    refused.assert_status(400);
    assert_eq!(
        refused.json::<Problem>().kind,
        problem_kind("invalid-model")
    );

    // And the one it does offer is accepted.
    let accepted = client
        .patch(&path)
        .bearer(&caller.token)
        .json(&remodel("claude-opus-6", Some("high")))
        .send()
        .await;
    accepted.assert_status(200);
    assert_eq!(
        accepted.json::<SessionDetail>().summary.model.model,
        "claude-opus-6"
    );
}

#[skyzen::test]
async fn a_model_change_on_somebody_elses_session_is_a_404(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = create(&client, &caller, &open(&caller, REPO, 10)).await;

    let stranger = sign_in(&kv, &db, seed_other_user(&db).await).await;
    client
        .patch(&format!("/v1/sessions/{}", session.summary.id))
        .bearer(&stranger.token)
        .json(&remodel("sonnet", None))
        .send()
        .await
        .assert_status(404);
}

// ── The mode a session runs under ──

/// A `PATCH /v1/sessions/{id}` body that only changes the permission mode.
fn remode(mode: flyco_core::PermissionMode) -> UpdateSession {
    UpdateSession {
        permission_mode: Some(mode),
        ..UpdateSession::default()
    }
}

#[skyzen::test]
async fn a_session_opened_without_a_mode_runs_the_product_default(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    // Resolved at read rather than written at create: the default is a
    // product decision, and a row nobody chose a mode for must still state
    // what it is on — the composer's chip reads it from here.
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);

    let session = create(&client, &caller, &open(&caller, REPO, 10)).await;
    assert_eq!(
        session.summary.permission_mode,
        flyco_core::PermissionMode::PRODUCT_DEFAULT
    );
}

#[skyzen::test]
async fn changing_the_mode_records_it_on_the_session(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = create(&client, &caller, &open(&caller, REPO, 10)).await;
    let path = format!("/v1/sessions/{}", session.summary.id);

    let changed = client
        .patch(&path)
        .bearer(&caller.token)
        .json(&remode(flyco_core::PermissionMode::Plan))
        .send()
        .await;
    changed.assert_status(200);
    assert_eq!(
        changed.json::<SessionDetail>().summary.permission_mode,
        flyco_core::PermissionMode::Plan
    );

    // And it is the row that changed, not just the answer: the next read
    // of the session names the new mode, and so does the list the rail
    // renders from.
    let reread = client.get(&path).bearer(&caller.token).send().await;
    reread.assert_status(200);
    assert_eq!(
        reread.json::<SessionDetail>().summary.permission_mode,
        flyco_core::PermissionMode::Plan
    );
}

#[skyzen::test]
async fn a_mode_change_reaches_the_session_of_nobody_else(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = create(&client, &caller, &open(&caller, REPO, 10)).await;

    let stranger = sign_in(&kv, &db, seed_other_user(&db).await).await;
    client
        .patch(&format!("/v1/sessions/{}", session.summary.id))
        .bearer(&stranger.token)
        .json(&remode(flyco_core::PermissionMode::BypassPermissions))
        .send()
        .await
        .assert_status(404);
}

// ── Idempotent creation and permission mode at birth ──

#[skyzen::test]
async fn a_replayed_create_returns_the_same_session(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);

    let first = client
        .post("/v1/sessions")
        .bearer(&caller.token)
        .header("idempotency-key", "create-attempt-1")
        .json(&open(&caller, REPO, 10))
        .send()
        .await;
    first.assert_status(201);
    let first: SessionDetail = first.json();

    let replay = client
        .post("/v1/sessions")
        .bearer(&caller.token)
        .header("idempotency-key", "create-attempt-1")
        .json(&open(&caller, REPO, 10))
        .send()
        .await;
    replay.assert_status(201);
    assert_eq!(
        replay.json::<SessionDetail>().summary.id,
        first.summary.id,
        "the same key replays the same session instead of provisioning twice"
    );

    let listed: Vec<SessionSummary> = client
        .get("/v1/sessions")
        .bearer(&caller.token)
        .send()
        .await
        .json();
    assert_eq!(listed.len(), 1, "one create, however many retries");
}

#[skyzen::test]
async fn distinct_keys_create_distinct_sessions(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);

    let mut ids = Vec::new();
    for key in ["create-attempt-2", "create-attempt-3"] {
        let response = client
            .post("/v1/sessions")
            .bearer(&caller.token)
            .header("idempotency-key", key)
            .json(&open(&caller, REPO, 10))
            .send()
            .await;
        response.assert_status(201);
        ids.push(response.json::<SessionDetail>().summary.id);
    }
    assert_ne!(ids[0], ids[1]);
}

#[skyzen::test]
async fn an_unusable_idempotency_key_is_refused(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);

    let too_long = "k".repeat(crate::idempotency::MAX_KEY_CHARS + 1);
    client
        .post("/v1/sessions")
        .bearer(&caller.token)
        .header("idempotency-key", &too_long)
        .json(&open(&caller, REPO, 10))
        .send()
        .await
        .assert_status(422);
}

#[skyzen::test]
async fn a_failed_create_releases_its_key_for_retry(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);

    // Cap of one: the second create is refused *after* the key is claimed,
    // and the refusal must not poison the key — the retry goes through.
    client
        .patch("/v1/me")
        .bearer(&caller.token)
        .json(&UpdateMe {
            session_cap: Some(1),
        })
        .send()
        .await
        .assert_status(200);
    create(&client, &caller, &open(&caller, REPO, 10)).await;

    client
        .post("/v1/sessions")
        .bearer(&caller.token)
        .header("idempotency-key", "after-the-cap")
        .json(&open(&caller, REPO, 10))
        .send()
        .await
        .assert_status(409);

    client
        .patch("/v1/me")
        .bearer(&caller.token)
        .json(&UpdateMe {
            session_cap: Some(2),
        })
        .send()
        .await
        .assert_status(200);
    let retried = client
        .post("/v1/sessions")
        .bearer(&caller.token)
        .header("idempotency-key", "after-the-cap")
        .json(&open(&caller, REPO, 10))
        .send()
        .await;
    retried.assert_status(201);
}

#[skyzen::test]
async fn a_create_carries_the_permission_mode_it_named(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);

    let mut request = open(&caller, REPO, 10);
    request.permission_mode = Some(flyco_core::PermissionMode::Plan);
    let session = create(&client, &caller, &request).await;
    assert_eq!(
        session.summary.permission_mode,
        flyco_core::PermissionMode::Plan,
        "an agent's mode is born with the session, not patched in later"
    );

    // And the row agrees: the provisioning read resolves the stored mode.
    let target = sessions::provisioning_target(&db, session.summary.id)
        .await
        .expect("read the provisioning row")
        .expect("the session exists");
    assert_eq!(target.permission_mode(), flyco_core::PermissionMode::Plan);
}

#[skyzen::test]
async fn a_create_without_a_mode_opens_on_the_product_default(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);

    let session = create(&client, &caller, &open(&caller, REPO, 10)).await;
    assert_eq!(
        session.summary.permission_mode,
        flyco_core::PermissionMode::PRODUCT_DEFAULT
    );
}
