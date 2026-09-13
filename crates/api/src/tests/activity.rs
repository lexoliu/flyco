//! What a session is doing, as the home list reads it (docs/ux.md §6).
//!
//! Every transition of [`SessionActivity`] is driven here through the route
//! that causes it — a daemon's turn events, the user's own messages, an
//! approval raised and decided — and read back off the list, because the
//! list is what the fact exists for. Asserting on the column instead would
//! test a write and leave the projection, where the pending-approval
//! overlay lives, uncovered.

use flyco_core::{
    ApprovalState, ApprovalView, CreateSession, CurrentUser, DaemonToken, DecideApproval,
    HarnessKind, SendMessage, SessionActivity, SessionDetail, SessionId, SessionState,
    SessionSummary, Usd, wire::ApprovalDecision, wire::ApprovalPayload,
};
use skyzen::routing::Router;
use skyzen_services::{Db, Kv};
use skyzen_test::{TestClient, TestContext};

use crate::session;
use crate::testing::{machine_choice, migrated_router, seed_provider_account, seed_user};

const REPO: &str = "lexoliu/flyco";

/// The opening instruction every session in this module is created with.
const PROMPT: &str = "audit the relay for dropped frames";

/// A signed-in caller and the session they are driving, with the daemon
/// token that session's machine would hold.
struct Running {
    user: CurrentUser,
    token: String,
    session: SessionId,
    daemon: String,
}

async fn running(client: &TestClient<Router>, kv: &Kv, db: &Db) -> Running {
    let user = seed_user(db).await;
    let token = session::issue(kv, user.id).await.expect("issue a session");
    let account = seed_provider_account(db, user.id).await;

    let opened = client
        .post("/v1/sessions")
        .bearer(&token)
        .json(&CreateSession {
            prompt: PROMPT.to_owned(),
            harness: HarnessKind::ClaudeCode,
            repo: REPO.to_owned(),
            branch: None,
            budget_limit: Usd::from_dollars(10),
            machine: Some(machine_choice(account)),
            spot: true,
            model: None,
            permission_mode: None,
        })
        .send()
        .await;
    opened.assert_status(201);
    let session = opened.json::<SessionDetail>().summary.id;

    // The daemon's arrival is what makes a session active; until M4 puts a
    // real machine under it, the transition is made here so the routes that
    // require a running session can be driven at all.
    crate::sessions::transition(db, user.id, session, SessionState::Active)
        .await
        .expect("activate the session");

    let paired = client
        .post(&format!("/v1/sessions/{session}/daemon-token"))
        .bearer(&token)
        .send()
        .await;
    paired.assert_status(200);
    let daemon = paired.json::<DaemonToken>().token;

    Running {
        user,
        token,
        session,
        daemon,
    }
}

impl Running {
    /// What the daemon reports when the harness starts or ends a turn.
    async fn turn(&self, client: &TestClient<Router>, event: &str) {
        client
            .post(&format!("/v1/sessions/{}/{event}", self.session))
            .bearer(&self.daemon)
            .send()
            .await
            .assert_status(204);
    }

    /// What the user sends when it is their move.
    async fn say(&self, client: &TestClient<Router>, text: &str) {
        client
            .post(&format!("/v1/sessions/{}/messages", self.session))
            .bearer(&self.token)
            .json(&SendMessage {
                text: text.to_owned(),
            })
            .send()
            .await
            .assert_status(202);
    }

    /// What the daemon raises when the agent needs a decision.
    async fn ask(&self, client: &TestClient<Router>) -> ApprovalView {
        let raised = client
            .post(&format!("/v1/sessions/{}/approvals", self.session))
            .bearer(&self.daemon)
            .json(&ApprovalPayload::AgentsMdChange {
                find: "old".to_owned(),
                replace: "new".to_owned(),
            })
            .send()
            .await;
        raised.assert_status(201);
        raised.json()
    }

    /// What the user answers with.
    async fn decide(&self, client: &TestClient<Router>, approval: &ApprovalView) {
        client
            .post(&format!("/v1/approvals/{}/decision", approval.id))
            .bearer(&self.token)
            .json(&DecideApproval {
                decision: ApprovalDecision::Approved,
            })
            .send()
            .await
            .assert_status(200);
    }

    /// The session as the home list renders it.
    async fn listed(&self, client: &TestClient<Router>) -> SessionSummary {
        let listed = client.get("/v1/sessions").bearer(&self.token).send().await;
        listed.assert_status(200);
        listed
            .json::<Vec<SessionSummary>>()
            .into_iter()
            .find(|summary| summary.id == self.session)
            .expect("the caller's own session is in their list")
    }

    /// The same session, as the session page reads it.
    async fn detail(&self, client: &TestClient<Router>) -> SessionDetail {
        let read = client
            .get(&format!("/v1/sessions/{}", self.session))
            .bearer(&self.token)
            .send()
            .await;
        read.assert_status(200);
        read.json()
    }
}

#[skyzen::test]
async fn a_session_that_has_run_nothing_is_idle(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let running = running(&client, &kv, &db).await;

    assert_eq!(
        running.listed(&client).await.activity,
        SessionActivity::Idle,
        "nothing is in flight and nobody is blocked"
    );
    assert_eq!(
        running.detail(&client).await.summary.activity,
        SessionActivity::Idle,
        "the list and the session page read one fact"
    );
}

#[skyzen::test]
async fn a_turn_in_flight_reads_as_working(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let running = running(&client, &kv, &db).await;

    running.turn(&client, "turn-started").await;

    assert_eq!(
        running.listed(&client).await.activity,
        SessionActivity::Working
    );
}

#[skyzen::test]
async fn a_turn_that_ended_reads_as_needing_the_user(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let running = running(&client, &kv, &db).await;

    running.turn(&client, "turn-started").await;
    running.turn(&client, "turn-completed").await;
    assert_eq!(
        running.listed(&client).await.activity,
        SessionActivity::NeedsInput,
        "the agent said its piece and it is the user's move"
    );

    // A turn that failed is the same answer for a different reason: the user
    // has to decide what happens next either way.
    running.turn(&client, "turn-started").await;
    running.turn(&client, "turn-failed").await;
    assert_eq!(
        running.listed(&client).await.activity,
        SessionActivity::NeedsInput
    );
}

#[skyzen::test]
async fn answering_a_finished_turn_puts_the_session_back_to_idle(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let running = running(&client, &kv, &db).await;

    running.turn(&client, "turn-started").await;
    running.turn(&client, "turn-completed").await;
    running.say(&client, "carry on").await;

    assert_eq!(
        running.listed(&client).await.activity,
        SessionActivity::Idle,
        "the user has spoken and no turn has started yet"
    );

    running.turn(&client, "turn-started").await;
    assert_eq!(
        running.listed(&client).await.activity,
        SessionActivity::Working,
        "and the turn that answers the message takes it back to working"
    );
}

#[skyzen::test]
async fn a_pending_approval_outranks_the_turn_it_interrupted(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let running = running(&client, &kv, &db).await;

    running.turn(&client, "turn-started").await;
    let approval = running.ask(&client).await;
    assert_eq!(approval.state, ApprovalState::Pending);

    assert_eq!(
        running.listed(&client).await.activity,
        SessionActivity::NeedsInput,
        "the agent is blocked on a decision, whatever the turn was doing"
    );

    running.decide(&client, &approval).await;
    assert_eq!(
        running.listed(&client).await.activity,
        SessionActivity::Working,
        "deciding it hands the turn back to the agent, and the row says so \
         without a compensating write"
    );
}

#[skyzen::test]
async fn a_decided_approval_with_no_turn_behind_it_is_idle(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let running = running(&client, &kv, &db).await;

    // No turn has started, so there is nothing for the decision to resume.
    let approval = running.ask(&client).await;
    assert_eq!(
        running.listed(&client).await.activity,
        SessionActivity::NeedsInput
    );

    running.decide(&client, &approval).await;
    assert_eq!(
        running.listed(&client).await.activity,
        SessionActivity::Idle
    );
}

#[skyzen::test]
async fn the_last_undecided_approval_is_the_one_that_frees_the_session(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let client = ctx.client(migrated_router(&db).await);
    let running = running(&client, &kv, &db).await;

    running.turn(&client, "turn-started").await;
    let first = running.ask(&client).await;
    let second = running.ask(&client).await;

    running.decide(&client, &first).await;
    assert_eq!(
        running.listed(&client).await.activity,
        SessionActivity::NeedsInput,
        "one of the two is still undecided"
    );

    running.decide(&client, &second).await;
    assert_eq!(
        running.listed(&client).await.activity,
        SessionActivity::Working
    );
}

#[skyzen::test]
async fn a_session_off_its_machine_keeps_the_activity_it_had(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let running = running(&client, &kv, &db).await;

    running.turn(&client, "turn-started").await;
    running.turn(&client, "turn-completed").await;

    crate::sessions::transition(&db, running.user.id, running.session, SessionState::Paused)
        .await
        .expect("pause the session");

    let summary = running.listed(&client).await;
    assert_eq!(summary.state, SessionState::Paused);
    assert_eq!(
        summary.activity,
        SessionActivity::NeedsInput,
        "a session that comes back reads as whatever it was doing when it \
         went; the UI ignores the field while it is not active"
    );
}

#[skyzen::test]
async fn a_daemon_cannot_report_a_turn_on_another_session(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let mine = running(&client, &kv, &db).await;
    let theirs = running(&client, &kv, &db).await;

    client
        .post(&format!("/v1/sessions/{}/turn-started", theirs.session))
        .bearer(&mine.daemon)
        .send()
        .await
        .assert_status(401);

    assert_eq!(
        theirs.listed(&client).await.activity,
        SessionActivity::Idle,
        "a refused report changes nothing"
    );
}
