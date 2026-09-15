//! End-to-end coverage of daemon pairing, the daemon-scoped routes, and the
//! two ways into a session's relay.

use flyco_core::{
    ApprovalState, ApprovalView, CreateSession, CurrentUser, DaemonToken, HarnessKind, Problem,
    SessionDetail, SessionId, Usd, wire::ApprovalPayload,
};
use skyzen::routing::Router;
use skyzen_services::{Db, Kv, Storage};
use skyzen_test::{TestClient, TestContext};

use crate::session;
use crate::testing::{
    machine_choice, migrated_router, seed_other_user, seed_provider_account, seed_user,
};
use crate::transcripts::BATCH_COUNT_HEADER;

const REPO: &str = "lexoliu/flyco";

/// The opening instruction every test session is created with.
const PROMPT: &str = "audit the relay for dropped frames";

fn problem_kind(slug: &str) -> String {
    let mut kind = String::from("https://flyco.dev/problems/");
    kind.push_str(slug);
    kind
}

/// A signed-in caller: the bearer token their browser would hold, and the
/// provider account their sessions are provisioned onto.
struct Caller {
    token: String,
    account: flyco_core::ProviderAccountId,
}

async fn sign_in(kv: &Kv, db: &Db, user: CurrentUser) -> Caller {
    let token = session::issue(kv, user.id).await.expect("issue a session");
    let account = seed_provider_account(db, user.id).await;
    Caller { token, account }
}

async fn open_session(client: &TestClient<Router>, caller: &Caller, repo: &str) -> SessionId {
    let response = client
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(&CreateSession {
            source: None,
            prompt: PROMPT.to_owned(),
            harness: HarnessKind::ClaudeCode,
            repo: repo.to_owned(),
            branch: None,
            budget_limit: Usd::from_dollars(10),
            machine: Some(machine_choice(caller.account)),
            spot: true,
            model: None,
            permission_mode: None,
        })
        .send()
        .await;
    response.assert_status(201);
    response.json::<SessionDetail>().summary.id
}

async fn pair(client: &TestClient<Router>, caller: &Caller, session: SessionId) -> String {
    let response = client
        .post(&format!("/v1/sessions/{session}/daemon-token"))
        .bearer(&caller.token)
        .send()
        .await;
    response.assert_status(200);
    let token: DaemonToken = response.json();
    assert_eq!(token.session, session);
    token.token
}

// ── Pairing ──

#[skyzen::test]
async fn a_daemon_token_is_minted_once_and_scoped_to_its_session(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let first = open_session(&client, &caller, REPO).await;
    let second = open_session(&client, &caller, "lexoliu/skyzen").await;

    let token = pair(&client, &caller, first).await;
    assert!(token.starts_with(flyco_core::DAEMON_TOKEN_PREFIX));

    // The daemon-scoped route of its own session accepts it …
    client
        .put(&format!("/v1/sessions/{first}/transcript/main/batches/0"))
        .bearer(&token)
        .body("{}\n")
        .send()
        .await
        .assert_status(204);

    // … and the same route of another session does not.
    let refused = client
        .put(&format!("/v1/sessions/{second}/transcript/main/batches/0"))
        .bearer(&token)
        .body("{}\n")
        .send()
        .await;
    refused.assert_status(401);
    refused.assert_header("www-authenticate", "Bearer error=\"invalid_token\"");
    assert_eq!(
        refused.json::<Problem>().kind,
        problem_kind("invalid-daemon-credential")
    );
}

#[skyzen::test]
async fn a_daemon_announces_the_stage_that_cannot_ride_the_relay(ctx: TestContext, kv: Kv, db: Db) {
    // The checkout happens before the harness exists, and therefore before
    // there is a command stream to send a frame down. `Cloning` is the one
    // stage of the timeline that has to be a REST call (docs/ux.md §9.2).
    let client = ctx.client(migrated_router(&db).await);
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let session = open_session(&client, &caller, REPO).await;
    let token = pair(&client, &caller, session).await;

    client
        .post(&format!("/v1/sessions/{session}/provisioning-stage"))
        .bearer(&token)
        .json(&flyco_core::ReportProvisioningStage {
            stage: flyco_core::ProvisioningStage::Cloning,
        })
        .send()
        .await
        .assert_status(204);

    // A user's own credential is not a daemon's, on this route as on every
    // other one under the daemon middleware.
    client
        .post(&format!("/v1/sessions/{session}/provisioning-stage"))
        .bearer(&caller.token)
        .json(&flyco_core::ReportProvisioningStage {
            stage: flyco_core::ProvisioningStage::Cloning,
        })
        .send()
        .await
        .assert_status(401);
}

#[skyzen::test]
async fn a_live_session_whose_agent_died_fails_with_what_the_agent_said(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    // The other half of the rule below. A daemon that reports a failure
    // *after* the session went live is a daemon whose agent process is
    // gone, and one that reaches that point exits cleanly — so systemd does
    // not restart it and nothing else is coming. Recording the reason and
    // waiting would leave the page spinning for fifteen minutes over a
    // session that is already over (issue #193).
    let client = ctx.client(migrated_router(&db).await);
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let session = open_session(&client, &caller, REPO).await;
    let token = pair(&client, &caller, session).await;
    crate::sessions::daemon_arrived(&db, &crate::testing::test_rooms(), session)
        .await
        .expect("the session goes live when its daemon greets");

    client
        .post(&format!("/v1/sessions/{session}/startup-failure"))
        .bearer(&token)
        .json(&flyco_core::ReportStartupFailure {
            message: "the agent process exited (exit status: 3). Its last output was:\nTypeError"
                .to_owned(),
        })
        .send()
        .await
        .assert_status(204);

    let detail: flyco_core::SessionDetail = client
        .get(&format!("/v1/sessions/{session}"))
        .bearer(&caller.token)
        .send()
        .await
        .json();

    assert_eq!(detail.summary.state, flyco_core::SessionState::Failed);
    assert!(
        detail
            .failure
            .as_ref()
            .is_some_and(|failure| failure.contains("exit status: 3")),
        "the session says what the agent said, not that something went wrong: {:?}",
        detail.failure
    );

    // And it is not still paying for the machine it can no longer use
    // (issue #199).
    let machine: flyco_core::MachineView = client
        .get(&format!("/v1/sessions/{session}/machine"))
        .bearer(&caller.token)
        .send()
        .await
        .json();
    assert_eq!(machine.state, flyco_core::MachineState::Destroyed);
}

#[skyzen::test]
async fn a_daemon_that_cannot_start_says_why_and_the_session_says_it_later(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    // `flycod` is restarted on failure, so a machine whose daemon cannot
    // start would otherwise say nothing at all: the relay is never opened,
    // and the page waits on a timeline that will not advance (issue #186).
    let client = ctx.client(migrated_router(&db).await);
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let session = open_session(&client, &caller, REPO).await;
    let token = pair(&client, &caller, session).await;

    client
        .post(&format!("/v1/sessions/{session}/startup-failure"))
        .bearer(&token)
        .json(&flyco_core::ReportStartupFailure {
            message: "could not materialize the sidecar at /var/lib/flyco/sidecar".to_owned(),
        })
        .send()
        .await
        .assert_status(204);

    // Recorded, not acted on: the next restart may succeed, so the session
    // is still being built and says nothing of this yet.
    let detail: flyco_core::SessionDetail = client
        .get(&format!("/v1/sessions/{session}"))
        .bearer(&caller.token)
        .send()
        .await
        .json();
    assert_eq!(detail.summary.state, flyco_core::SessionState::Provisioning);
    assert_eq!(detail.failure, None);

    // And it is the sentence the session gives when the machine never does
    // come up, rather than the control plane guessing from silence.
    assert_eq!(
        crate::sessions::startup_failure(&db, session)
            .await
            .expect("read the recorded failure")
            .as_deref(),
        Some("could not materialize the sidecar at /var/lib/flyco/sidecar")
    );

    // A user's own credential is not a daemon's, here as everywhere else.
    client
        .post(&format!("/v1/sessions/{session}/startup-failure"))
        .bearer(&caller.token)
        .json(&flyco_core::ReportStartupFailure {
            message: "not mine to send".to_owned(),
        })
        .send()
        .await
        .assert_status(401);
}

#[skyzen::test]
async fn a_user_credential_does_not_open_a_daemon_route(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let session = open_session(&client, &caller, REPO).await;

    let refused = client
        .put(&format!("/v1/sessions/{session}/transcript/main/batches/0"))
        .bearer(&caller.token)
        .body("{}\n")
        .send()
        .await;
    refused.assert_status(401);
}

#[skyzen::test]
async fn only_the_owner_may_pair_a_daemon(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let owner = sign_in(&kv, &db, seed_user(&db).await).await;
    let stranger = sign_in(&kv, &db, seed_other_user(&db).await).await;
    let session = open_session(&client, &owner, REPO).await;

    let refused = client
        .post(&format!("/v1/sessions/{session}/daemon-token"))
        .bearer(&stranger.token)
        .send()
        .await;
    refused.assert_status(404);
    assert_eq!(
        refused.json::<Problem>().kind,
        problem_kind("session-not-found")
    );
}

// ── Transcripts ──

#[skyzen::test]
async fn transcript_batches_round_trip_through_storage(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    _storage: Storage,
) {
    let client = ctx.client(migrated_router(&db).await);
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let session = open_session(&client, &caller, REPO).await;
    let token = pair(&client, &caller, session).await;

    for (seq, line) in ["{\"n\":0}\n", "{\"n\":1}\n", "{\"n\":2}\n"]
        .into_iter()
        .enumerate()
    {
        client
            .put(&format!(
                "/v1/sessions/{session}/transcript/main/batches/{seq}"
            ))
            .bearer(&token)
            .body(line)
            .send()
            .await
            .assert_status(204);
    }

    let read = client
        .get(&format!("/v1/sessions/{session}/transcript/main"))
        .bearer(&token)
        .send()
        .await;
    read.assert_status(200);
    read.assert_header("content-type", crate::transcripts::CONTENT_TYPE);
    read.assert_header(BATCH_COUNT_HEADER, "3");
    assert_eq!(read.body_text(), "{\"n\":0}\n{\"n\":1}\n{\"n\":2}\n");
}

#[skyzen::test]
async fn a_transcript_stream_that_was_never_written_reads_empty(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    _storage: Storage,
) {
    let client = ctx.client(migrated_router(&db).await);
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let session = open_session(&client, &caller, REPO).await;
    let token = pair(&client, &caller, session).await;

    let read = client
        .get(&format!("/v1/sessions/{session}/transcript/main"))
        .bearer(&token)
        .send()
        .await;
    read.assert_status(200);
    read.assert_header(BATCH_COUNT_HEADER, "0");
    assert_eq!(read.body_text(), "");
}

#[skyzen::test]
async fn rewriting_a_transcript_batch_is_a_conflict(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    _storage: Storage,
) {
    let client = ctx.client(migrated_router(&db).await);
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let session = open_session(&client, &caller, REPO).await;
    let token = pair(&client, &caller, session).await;
    let path = format!("/v1/sessions/{session}/transcript/main/batches/0");

    client
        .put(&path)
        .bearer(&token)
        .body("first")
        .send()
        .await
        .assert_status(204);
    let again = client.put(&path).bearer(&token).body("second").send().await;
    again.assert_status(409);
    assert_eq!(
        again.json::<Problem>().kind,
        problem_kind("batch-already-stored")
    );
}

#[skyzen::test]
async fn a_stream_key_that_is_not_one_path_segment_is_unprocessable(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    _storage: Storage,
) {
    let client = ctx.client(migrated_router(&db).await);
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let session = open_session(&client, &caller, REPO).await;
    let token = pair(&client, &caller, session).await;

    let refused = client
        .put(&format!("/v1/sessions/{session}/transcript/../batches/0"))
        .bearer(&token)
        .body("x")
        .send()
        .await;
    refused.assert_status(422);
    assert_eq!(
        refused.json::<Problem>().kind,
        problem_kind("invalid-stream-key")
    );
}

// ── Approvals raised by the daemon ──

#[skyzen::test]
async fn a_daemon_raises_an_approval_against_its_own_session(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let session = open_session(&client, &caller, REPO).await;
    let token = pair(&client, &caller, session).await;

    let raised = client
        .post(&format!("/v1/sessions/{session}/approvals"))
        .bearer(&token)
        .json(&ApprovalPayload::AgentsMdChange {
            find: "old".to_owned(),
            replace: "new".to_owned(),
        })
        .send()
        .await;
    raised.assert_status(201);
    let raised: ApprovalView = raised.json();
    assert_eq!(raised.session, session);
    assert_eq!(raised.state, ApprovalState::Pending);

    // The user sees the same approval through their own credential, which is
    // what makes the daemon's REST-before-relay ordering worth anything.
    let listed = client
        .get(&format!("/v1/approvals?session={session}"))
        .bearer(&caller.token)
        .send()
        .await;
    listed.assert_status(200);
    let listed: Vec<ApprovalView> = listed.json();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, raised.id);

    // Deciding it still works, and the room notification is best-effort:
    // this build has no live room, and the decision stands regardless.
    let decided = client
        .post(&format!("/v1/approvals/{}/decision", raised.id))
        .bearer(&caller.token)
        .json(&flyco_core::DecideApproval {
            decision: flyco_core::ApprovalDecision::Approved,
        })
        .send()
        .await;
    decided.assert_status(200);
    assert_eq!(
        decided.json::<ApprovalView>().state,
        ApprovalState::Approved
    );
}

// ── The relay hops ──

/// The attach body every daemon POSTs.
fn attach_body(version: u32) -> flyco_core::wire::DaemonAttach {
    flyco_core::wire::DaemonAttach {
        protocol_version: version,
    }
}

#[skyzen::test]
async fn a_daemon_attach_is_authenticated_before_it_reaches_a_room(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let client = ctx.client(migrated_router(&db).await);
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let session = open_session(&client, &caller, REPO).await;
    let token = pair(&client, &caller, session).await;
    let path = format!("/v1/sessions/{session}/relay/attach");

    // No credential.
    let anonymous = client
        .post(&path)
        .json(&attach_body(flyco_core::WIRE_PROTOCOL_VERSION))
        .send()
        .await;
    anonymous.assert_status(401);
    anonymous.assert_header("www-authenticate", "Bearer");

    // A user credential is not a daemon credential.
    let wrong_kind = client
        .post(&path)
        .bearer(&caller.token)
        .json(&attach_body(flyco_core::WIRE_PROTOCOL_VERSION))
        .send()
        .await;
    wrong_kind.assert_status(401);
    assert_eq!(
        wrong_kind.json::<Problem>().kind,
        problem_kind("invalid-daemon-credential")
    );

    // A daemon speaking another protocol is refused before either side
    // moves a frame.
    let mismatched = client
        .post(&path)
        .bearer(&token)
        .json(&attach_body(flyco_core::WIRE_PROTOCOL_VERSION + 1))
        .send()
        .await;
    mismatched.assert_status(409);
    assert_eq!(
        mismatched.json::<Problem>().kind,
        problem_kind("protocol-mismatch")
    );

    // The right credential attaches, and the session goes live on the
    // strength of it: a machine that exists is not an agent that is ready.
    let attached = client
        .post(&path)
        .bearer(&token)
        .json(&attach_body(flyco_core::WIRE_PROTOCOL_VERSION))
        .send()
        .await;
    attached.assert_status(200);
    let attached: flyco_core::wire::DaemonAttached = attached.json();
    assert_eq!(attached.epoch, 1, "the first attach is epoch one");

    let detail: flyco_core::SessionDetail = client
        .get(&format!("/v1/sessions/{session}"))
        .bearer(&caller.token)
        .send()
        .await
        .json();
    assert_eq!(detail.summary.state, flyco_core::SessionState::Active);
}

#[skyzen::test]
async fn a_daemon_command_stream_is_scoped_to_its_session_and_its_epoch(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let client = ctx.client(migrated_router(&db).await);
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let session = open_session(&client, &caller, REPO).await;
    let token = pair(&client, &caller, session).await;

    let attached = client
        .post(&format!("/v1/sessions/{session}/relay/attach"))
        .bearer(&token)
        .json(&attach_body(flyco_core::WIRE_PROTOCOL_VERSION))
        .send()
        .await;
    attached.assert_status(200);
    let epoch = attached.json::<flyco_core::wire::DaemonAttached>().epoch;

    // No credential, a user credential, another session's daemon: all
    // refused before the room is asked anything.
    let path = format!("/v1/sessions/{session}/relay/commands?epoch={epoch}");
    client.get(&path).send().await.assert_status(401);
    client
        .get(&path)
        .bearer(&caller.token)
        .send()
        .await
        .assert_status(401);

    let other = open_session(&client, &caller, "lexoliu/skyzen").await;
    let other_token = pair(&client, &caller, other).await;
    let wrong_session = client.get(&path).bearer(&other_token).send().await;
    wrong_session.assert_status(401);
    assert_eq!(
        wrong_session.json::<Problem>().kind,
        problem_kind("invalid-daemon-credential")
    );

    // A stream that names a superseded attach is refused — and by the
    // time the room can say so, the token is already checked.
    let stale = client
        .get(&format!(
            "/v1/sessions/{session}/relay/commands?epoch={}",
            epoch + 1
        ))
        .bearer(&token)
        .send()
        .await;
    stale.assert_status(409);
    assert_eq!(
        stale.json::<Problem>().kind,
        problem_kind("relay-epoch-stale")
    );
}

#[skyzen::test]
async fn a_daemon_frames_batch_is_authenticated_then_forwarded(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let session = open_session(&client, &caller, REPO).await;
    let token = pair(&client, &caller, session).await;
    let path = format!("/v1/sessions/{session}/relay/frames");

    let attached = client
        .post(&format!("/v1/sessions/{session}/relay/attach"))
        .bearer(&token)
        .json(&attach_body(flyco_core::WIRE_PROTOCOL_VERSION))
        .send()
        .await;
    let epoch = attached.json::<flyco_core::wire::DaemonAttached>().epoch;

    let batch = |from_seq| flyco_core::wire::DaemonFrames {
        epoch,
        from_seq,
        ack_through: 0,
        frames: vec![flyco_core::DaemonToControl::Harness {
            event: flyco_core::HarnessEvent::AssistantDelta {
                turn_id: "turn-1".to_owned(),
                text: "hello".to_owned(),
            },
        }],
    };

    // No credential: refused before the room is asked.
    client
        .post(&path)
        .json(&batch(1))
        .send()
        .await
        .assert_status(401);

    // A gap in the daemon's numbering is a conflict, and the body says
    // where the stream must resume.
    let skipped = client
        .post(&path)
        .bearer(&token)
        .json(&batch(7))
        .send()
        .await;
    skipped.assert_status(409);
    assert_eq!(
        skipped.json::<Problem>().kind,
        problem_kind("relay-frames-gap")
    );

    // In order: stored, and the emitted event is published to the owner's
    // stream — readable back through the session's catch-up history.
    client
        .post(&path)
        .bearer(&token)
        .json(&batch(1))
        .send()
        .await
        .assert_status(204);

    let page = client
        .get(&format!("/v1/sessions/{session}/events"))
        .bearer(&caller.token)
        .send()
        .await;
    page.assert_status(200);
    let page: flyco_core::wire::EventPage = page.json();
    assert!(
        page.events.iter().any(|stored| {
            serde_json::from_value::<flyco_core::ClientEvent>(stored.event.clone())
                .is_ok_and(|event| matches!(event, flyco_core::ClientEvent::Harness { .. }))
        }),
        "the daemon's frame was recorded into the session's tail: {:?}",
        page.events
    );
}

// ── Catching up ──

#[skyzen::test]
async fn the_event_tail_is_readable_and_scoped_to_its_owner(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let owner = sign_in(&kv, &db, seed_user(&db).await).await;
    let stranger = sign_in(&kv, &db, seed_other_user(&db).await).await;
    let session = open_session(&client, &owner, REPO).await;

    // A session that has never had a daemon still has a tail, because the
    // prompt it was opened with is already conversation: it was recorded
    // when the session was created and is waiting in the room's mailbox.
    let page = client
        .get(&format!("/v1/sessions/{session}/events"))
        .bearer(&owner.token)
        .send()
        .await;
    page.assert_status(200);
    let page: flyco_core::wire::EventPage = page.json();
    assert_eq!(
        page.events
            .iter()
            .map(|stored| stored.event.clone())
            .collect::<Vec<_>>(),
        vec![
            serde_json::to_value(flyco_core::ClientEvent::UserMessage {
                text: PROMPT.to_owned(),
                origin: flyco_core::MessageOrigin::User,
            })
            .expect("serialize")
        ]
    );
    assert!(!page.more);

    // Ownership is settled in the Worker, because a room cannot reach D1.
    let refused = client
        .get(&format!("/v1/sessions/{session}/events"))
        .bearer(&stranger.token)
        .send()
        .await;
    refused.assert_status(404);
    assert_eq!(
        refused.json::<Problem>().kind,
        problem_kind("session-not-found")
    );

    client
        .get(&format!("/v1/sessions/{session}/events"))
        .send()
        .await
        .assert_status(401);
}

// ── Driving a session ──

/// Puts a session in the state a daemon would have moved it to.
///
/// M4 provisions a machine and the daemon's arrival is what makes a session
/// active; until then the transition is made here so the routes that require
/// a running session can be driven at all.
async fn activate(db: &Db, user: flyco_core::UserId, session: SessionId) {
    crate::sessions::transition(db, user, session, flyco_core::SessionState::Active)
        .await
        .expect("activate the session");
}

fn message(text: &str) -> flyco_core::SendMessage {
    flyco_core::SendMessage {
        text: text.to_owned(),
    }
}

#[skyzen::test]
async fn an_active_session_accepts_message_interrupt_and_compaction(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let caller = sign_in(&kv, &db, user.clone()).await;
    let session = open_session(&client, &caller, REPO).await;
    activate(&db, user.id, session).await;

    // `202`, because the answer arrives on the relay rather than here.
    client
        .post(&format!("/v1/sessions/{session}/messages"))
        .bearer(&caller.token)
        .json(&message("what does this crate do?"))
        .send()
        .await
        .assert_status(202);

    client
        .post(&format!("/v1/sessions/{session}/interrupt"))
        .bearer(&caller.token)
        .send()
        .await
        .assert_status(202);

    client
        .post(&format!("/v1/sessions/{session}/compact"))
        .bearer(&caller.token)
        .send()
        .await
        .assert_status(202);
}

#[skyzen::test]
async fn an_active_session_accepts_terminal_commands(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let caller = sign_in(&kv, &db, user.clone()).await;
    let session = open_session(&client, &caller, REPO).await;
    activate(&db, user.id, session).await;

    client
        .post(&format!("/v1/sessions/{session}/terminal/input"))
        .bearer(&caller.token)
        .json(&flyco_core::TerminalInput {
            data: "ls\n".to_owned(),
        })
        .send()
        .await
        .assert_status(202);

    client
        .post(&format!("/v1/sessions/{session}/terminal/resize"))
        .bearer(&caller.token)
        .json(&flyco_core::TerminalSize {
            cols: 120,
            rows: 32,
        })
        .send()
        .await
        .assert_status(202);

    for resume in [false, true] {
        client
            .post(&format!("/v1/sessions/{session}/terminal/harness"))
            .bearer(&caller.token)
            .json(&flyco_core::HarnessTui { resume })
            .send()
            .await
            .assert_status(202);
    }
}

#[skyzen::test]
async fn a_harness_launch_refuses_a_session_that_is_not_running(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let session = open_session(&client, &caller, REPO).await;

    let refused = client
        .post(&format!("/v1/sessions/{session}/terminal/harness"))
        .bearer(&caller.token)
        .json(&flyco_core::HarnessTui { resume: false })
        .send()
        .await;
    refused.assert_status(409);
    assert_eq!(
        refused.json::<Problem>().kind,
        problem_kind("session-not-active")
    );
}

#[skyzen::test]
async fn a_session_that_is_not_running_refuses_to_be_driven(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let session = open_session(&client, &caller, REPO).await;

    // Still provisioning: there is no daemon to hear an interrupt or a
    // compact, and a `202` would promise work nobody is going to do. A user
    // message is the exception — the room's command log holds it for the
    // daemon's first attach, which is what "the mailbox holds them" means.
    let held = client
        .post(&format!("/v1/sessions/{session}/messages"))
        .bearer(&caller.token)
        .json(&message("hello"))
        .send()
        .await;
    held.assert_status(202);
    for path in [
        format!("/v1/sessions/{session}/interrupt"),
        format!("/v1/sessions/{session}/compact"),
    ] {
        let refused = client.post(&path).bearer(&caller.token).send().await;
        refused.assert_status(409);
        let problem = refused.json::<Problem>();
        assert_eq!(problem.kind, problem_kind("session-not-active"));
        assert!(
            problem.detail.contains("Provisioning"),
            "the refusal names the state: {}",
            problem.detail
        );
    }
}

#[skyzen::test]
async fn a_message_with_nothing_in_it_is_unprocessable(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let caller = sign_in(&kv, &db, user.clone()).await;
    let session = open_session(&client, &caller, REPO).await;
    activate(&db, user.id, session).await;

    let refused = client
        .post(&format!("/v1/sessions/{session}/messages"))
        .bearer(&caller.token)
        .json(&message("   \n"))
        .send()
        .await;
    refused.assert_status(422);
    assert_eq!(
        refused.json::<Problem>().kind,
        problem_kind("empty-message")
    );
}

#[skyzen::test]
async fn another_users_session_cannot_be_driven_or_read(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let owner = sign_in(&kv, &db, user.clone()).await;
    let stranger = sign_in(&kv, &db, seed_other_user(&db).await).await;
    let session = open_session(&client, &owner, REPO).await;
    activate(&db, user.id, session).await;

    for path in [
        format!("/v1/sessions/{session}/messages"),
        format!("/v1/sessions/{session}/interrupt"),
        format!("/v1/sessions/{session}/compact"),
    ] {
        client
            .post(&path)
            .bearer(&stranger.token)
            .json(&message("mine now"))
            .send()
            .await
            .assert_status(404);
    }
    // The terminal routes take their own bodies, so they cannot share the
    // loop above.
    client
        .post(&format!("/v1/sessions/{session}/terminal/input"))
        .bearer(&stranger.token)
        .json(&flyco_core::TerminalInput {
            data: "ls\n".to_owned(),
        })
        .send()
        .await
        .assert_status(404);
    client
        .post(&format!("/v1/sessions/{session}/terminal/resize"))
        .bearer(&stranger.token)
        .json(&flyco_core::TerminalSize {
            cols: 120,
            rows: 32,
        })
        .send()
        .await
        .assert_status(404);
    client
        .post(&format!("/v1/sessions/{session}/terminal/harness"))
        .bearer(&stranger.token)
        .json(&flyco_core::HarnessTui { resume: false })
        .send()
        .await
        .assert_status(404);
    for path in [
        format!("/v1/sessions/{session}/turns"),
        format!("/v1/sessions/{session}/repo-status"),
        format!("/v1/sessions/{session}/files"),
        format!("/v1/sessions/{session}/files/content?path=README.md"),
        format!("/v1/sessions/{session}/diff"),
    ] {
        let refused = client.get(&path).bearer(&stranger.token).send().await;
        refused.assert_status(404);
        assert_eq!(
            refused.json::<Problem>().kind,
            problem_kind("session-not-found")
        );
    }
}

#[skyzen::test]
async fn a_session_with_no_turns_yet_has_an_empty_history(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let session = open_session(&client, &caller, REPO).await;

    let page = client
        .get(&format!("/v1/sessions/{session}/turns"))
        .bearer(&caller.token)
        .send()
        .await;
    page.assert_status(200);
    let page: flyco_core::TurnPage = page.json();
    assert_eq!(page.turns, [] as [flyco_core::TurnSummary; 0]);
    assert!(
        page.next_cursor.is_none(),
        "there is nothing recorded to come back for"
    );

    let refused = client
        .get(&format!(
            "/v1/sessions/{session}/turns?cursor=not-a-position"
        ))
        .bearer(&caller.token)
        .send()
        .await;
    refused.assert_status(400);
    assert_eq!(
        refused.json::<Problem>().kind,
        problem_kind("invalid-cursor")
    );
}

#[skyzen::test]
async fn reading_the_checkout_of_a_session_with_no_daemon_says_so(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let client = ctx.client(migrated_router(&db).await);
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let session = open_session(&client, &caller, REPO).await;

    // The `Files` and `Diff` tabs are answered live by the machine, so a
    // session that has none is told that plainly rather than being shown an
    // empty tree it would read as "the agent has changed nothing".
    for path in [
        format!("/v1/sessions/{session}/files"),
        format!("/v1/sessions/{session}/files/content?path=README.md"),
        format!("/v1/sessions/{session}/diff"),
    ] {
        let refused = client.get(&path).bearer(&caller.token).send().await;
        refused.assert_status(503);
        assert_eq!(
            refused.json::<Problem>().kind,
            problem_kind("session-daemon-offline"),
            "{path}"
        );
    }
}

#[skyzen::test]
async fn a_working_tree_nobody_has_reported_is_not_found(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let session = open_session(&client, &caller, REPO).await;

    let unknown = client
        .get(&format!("/v1/sessions/{session}/repo-status"))
        .bearer(&caller.token)
        .send()
        .await;
    unknown.assert_status(404);
    assert_eq!(
        unknown.json::<Problem>().kind,
        problem_kind("repo-status-unknown")
    );
}
