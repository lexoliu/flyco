//! `flyco handoff`'s server half, end to end: the pending row gates
//! provisioning, the uploads land in storage, `complete` frees the
//! machine only when the manifest matches what the routes received, and
//! the daemon reads the result through its own scope.

use flyco_core::{
    CreateSession, CurrentUser, HandoffManifest, HandoffView, HarnessKind, LocalHandoff, Problem,
    ProviderAccountId, SessionDetail, SessionId, SessionSource, SessionState, Usd,
};
use sha2::{Digest as _, Sha256};
use skyzen::routing::Router;
use skyzen::sql;
use skyzen_services::queue::ReceiveOptions;
use skyzen_services::{Db, Kv, Queue, Storage};
use skyzen_test::{TestClient, TestContext};

use crate::provisioning_queue::ProvisioningJob;
use crate::testing::{
    machine_choice, migrated_router_on, seed_other_user, seed_provider_account, seed_user,
    test_config, test_host_rooms, test_rooms,
};
use crate::{daemon_tokens, session};

const REPO: &str = "lexoliu/flyco";
const PROMPT: &str = "pick up the local session's work";
const PATCH: &[u8] = b"diff --git a/x b/x\n+handed off\n";
const TRANSCRIPT: &[u8] = b"{\"type\":\"summary\"}\n";

/// A signed-in caller with a linked host to provision onto.
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

/// The `local_handoff` source every create below declares.
fn source() -> SessionSource {
    SessionSource::LocalHandoff(LocalHandoff {
        harness: HarnessKind::Codex,
        session_id: "rollout-abc".to_owned(),
        base_commit: "deadbeef".to_owned(),
        local_workdir: "/home/u/flyco".to_owned(),
    })
}

/// Creates a session whose `source` is a local handoff.
async fn open(client: &TestClient<Router>, caller: &Caller) -> SessionDetail {
    let response = client
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(&CreateSession {
            source: Some(source()),
            prompt: PROMPT.to_owned(),
            harness: HarnessKind::Codex,
            repo: REPO.to_owned(),
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
    response.json()
}

/// The manifest a sender computes over what it uploaded.
fn manifest(patch: &[u8], transcript: &[u8]) -> HandoffManifest {
    HandoffManifest {
        patch_sha256: hex::encode(Sha256::digest(patch)),
        patch_bytes: patch.len() as u64,
        transcript_sha256: hex::encode(Sha256::digest(transcript)),
        transcript_bytes: transcript.len() as u64,
    }
}

/// Uploads both payloads and completes, as the CLI does once the session
/// exists.
async fn upload_and_complete(client: &TestClient<Router>, caller: &Caller, session: SessionId) {
    client
        .put(&format!("/v1/sessions/{session}/handoff/patch"))
        .bearer(&caller.token)
        .body(PATCH)
        .send()
        .await
        .assert_status(204);
    client
        .put(&format!("/v1/sessions/{session}/handoff/transcript"))
        .bearer(&caller.token)
        .body(TRANSCRIPT)
        .send()
        .await
        .assert_status(204);
    client
        .post(&format!("/v1/sessions/{session}/handoff/complete"))
        .bearer(&caller.token)
        .json(&manifest(PATCH, TRANSCRIPT))
        .send()
        .await
        .assert_status(204);
}

/// Every provisioning job the queue is holding.
async fn queued(queue: &Queue) -> Vec<ProvisioningJob> {
    queue
        .receive_json::<ProvisioningJob>(ReceiveOptions::new().with_max_messages(16))
        .await
        .expect("read the provisioning queue")
        .into_iter()
        .map(|message| message.body)
        .collect()
}

#[skyzen::test]
async fn a_handoff_session_reserves_its_machine_without_queueing_it(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);

    let session = open(&client, &caller).await;
    assert_eq!(session.summary.state, SessionState::Provisioning);

    // The machine row exists — the reservation is held — but no job may
    // be on the queue: a daemon that booted now would clone without the
    // patch it is meant to apply.
    assert!(
        crate::machines::for_session(&db, session.summary.id)
            .await
            .expect("read the machine row")
            .is_some(),
        "a handoff reserves its machine at create"
    );
    assert!(
        queued(&queue).await.is_empty(),
        "a pending handoff queues nothing"
    );

    let session_id = session.summary.id;
    let pending: Option<Option<i64>> = sql!(
        db,
        "SELECT completed_at_unix FROM handoffs WHERE session_id = {session_id}"
    )
    .fetch_scalar_optional()
    .await
    .expect("read the handoff row");
    assert_eq!(pending, Some(None), "the row is recorded pending");
}

#[skyzen::test]
async fn complete_frees_provisioning_and_the_daemon_reads_the_result(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
    storage: Storage,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = open(&client, &caller).await.summary.id;

    upload_and_complete(&client, &caller, session).await;

    // The patch rode the workdir object; the transcript has its own key.
    assert!(
        storage
            .get(&format!("workdirs/{session}/uncommitted.patch"))
            .await
            .expect("read the patch object")
            .is_some(),
        "the patch is stored where the daemon's replay path reads it"
    );
    assert_eq!(
        storage
            .get(&format!("handoffs/{session}/transcript"))
            .await
            .expect("read the transcript object")
            .map(|object| object.body),
        Some(TRANSCRIPT.to_vec())
    );

    // Completion is what enqueued the build, and exactly once.
    let jobs = queued(&queue).await;
    assert_eq!(jobs.len(), 1, "one provisioning job after complete");

    // The daemon's view names the base commit and the patch checksum, and
    // the transcript streams back the bytes that were uploaded.
    let daemon = daemon_tokens::issue(&db, caller.user.id, session)
        .await
        .expect("mint a daemon token")
        .token;
    let view: HandoffView = client
        .get(&format!("/v1/sessions/{session}/handoff"))
        .bearer(&daemon)
        .send()
        .await
        .json();
    assert_eq!(view.base_commit, "deadbeef");
    assert_eq!(view.patch_sha256, hex::encode(Sha256::digest(PATCH)));
    assert!(view.has_transcript);
    let body = client
        .get(&format!("/v1/sessions/{session}/handoff/transcript"))
        .bearer(&daemon)
        .send()
        .await;
    body.assert_status(200);
    assert_eq!(body.body_bytes(), TRANSCRIPT);
}

#[skyzen::test]
async fn a_manifest_that_disagrees_with_the_upload_is_refused(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = open(&client, &caller).await.summary.id;
    upload_patch_and_transcript(&client, &caller, session).await;

    let mut wrong = manifest(PATCH, TRANSCRIPT);
    wrong.transcript_sha256 = hex::encode(Sha256::digest(b"not the transcript"));
    let refused = client
        .post(&format!("/v1/sessions/{session}/handoff/complete"))
        .bearer(&caller.token)
        .json(&wrong)
        .send()
        .await;
    refused.assert_status(422);
    assert_eq!(
        refused.json::<Problem>().kind,
        "https://flyco.dev/problems/handoff-checksum-mismatch"
    );

    // A refused complete neither finishes the row nor frees the machine.
    assert!(queued(&queue).await.is_empty());
    let session_id = session;
    let pending: Option<Option<i64>> = sql!(
        db,
        "SELECT completed_at_unix FROM handoffs WHERE session_id = {session_id}"
    )
    .fetch_scalar_optional()
    .await
    .expect("read the handoff row");
    assert_eq!(pending, Some(None));
}

async fn upload_patch_and_transcript(
    client: &TestClient<Router>,
    caller: &Caller,
    session: SessionId,
) {
    client
        .put(&format!("/v1/sessions/{session}/handoff/patch"))
        .bearer(&caller.token)
        .body(PATCH)
        .send()
        .await
        .assert_status(204);
    client
        .put(&format!("/v1/sessions/{session}/handoff/transcript"))
        .bearer(&caller.token)
        .body(TRANSCRIPT)
        .send()
        .await
        .assert_status(204);
}

#[skyzen::test]
async fn an_object_that_never_landed_is_named_at_complete(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = open(&client, &caller).await.summary.id;

    // Neither upload happened: the patch is named first.
    let refused = client
        .post(&format!("/v1/sessions/{session}/handoff/complete"))
        .bearer(&caller.token)
        .json(&manifest(PATCH, TRANSCRIPT))
        .send()
        .await;
    refused.assert_status(422);
    assert_eq!(
        refused.json::<Problem>().kind,
        "https://flyco.dev/problems/handoff-object-missing"
    );

    // With only the patch stored, the transcript is what is missing.
    client
        .put(&format!("/v1/sessions/{session}/handoff/patch"))
        .bearer(&caller.token)
        .body(PATCH)
        .send()
        .await
        .assert_status(204);
    let refused = client
        .post(&format!("/v1/sessions/{session}/handoff/complete"))
        .bearer(&caller.token)
        .json(&manifest(PATCH, TRANSCRIPT))
        .send()
        .await;
    refused.assert_status(422);
    let problem: Problem = refused.json();
    assert_eq!(
        problem.kind,
        "https://flyco.dev/problems/handoff-object-missing"
    );
    assert!(
        problem.detail.contains("transcript"),
        "the refusal names the object that never landed: {}",
        problem.detail
    );
}

#[skyzen::test]
async fn complete_is_idempotent_on_the_same_manifest_and_immutable_after(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = open(&client, &caller).await.summary.id;
    upload_patch_and_transcript(&client, &caller, session).await;

    let declared = manifest(PATCH, TRANSCRIPT);
    for _ in 0..2 {
        client
            .post(&format!("/v1/sessions/{session}/handoff/complete"))
            .bearer(&caller.token)
            .json(&declared)
            .send()
            .await
            .assert_status(204);
    }
    assert_eq!(
        queued(&queue).await.len(),
        1,
        "a replayed complete does not queue a second machine"
    );

    // A different manifest against a finished row is a conflict, and so
    // is a late re-upload that would swap the payload under it.
    let mut other = manifest(PATCH, TRANSCRIPT);
    other.patch_bytes = other.patch_bytes.saturating_add(1);
    let refused = client
        .post(&format!("/v1/sessions/{session}/handoff/complete"))
        .bearer(&caller.token)
        .json(&other)
        .send()
        .await;
    refused.assert_status(409);
    assert_eq!(
        refused.json::<Problem>().kind,
        "https://flyco.dev/problems/handoff-not-pending"
    );
    client
        .put(&format!("/v1/sessions/{session}/handoff/patch"))
        .bearer(&caller.token)
        .body(b"different".as_slice())
        .send()
        .await
        .assert_status(409);
}

#[skyzen::test]
async fn another_users_handoff_is_not_found(ctx: TestContext, kv: Kv, db: Db, queue: Queue) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = open(&client, &caller).await.summary.id;

    let stranger = sign_in(&kv, &db, seed_other_user(&db).await).await;
    for response in [
        client
            .put(&format!("/v1/sessions/{session}/handoff/patch"))
            .bearer(&stranger.token)
            .body(PATCH)
            .send()
            .await,
        client
            .post(&format!("/v1/sessions/{session}/handoff/complete"))
            .bearer(&stranger.token)
            .json(&manifest(PATCH, TRANSCRIPT))
            .send()
            .await,
    ] {
        response.assert_status(404);
        assert_eq!(
            response.json::<Problem>().kind,
            "https://flyco.dev/problems/session-not-found"
        );
    }
}

#[skyzen::test]
async fn a_pending_handoff_is_no_stall_but_ages_out_on_its_own_clock(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);
    let session = open(&client, &caller).await.summary.id;
    let now = crate::clock::now_unix();

    // Older than a provisioning stall but younger than the upload
    // deadline: the sweep must leave the pending handoff alone.
    let stalled = now.saturating_sub(flyco_core::PROVISION_DEADLINE_SECS + 1);
    sql!(
        db,
        "UPDATE sessions SET created_at_unix = {stalled}, last_active_unix = {stalled} \
         WHERE id = {session}"
    )
    .execute()
    .await
    .expect("age the session past the provisioning deadline");
    crate::app::fail_stalled_provisions(
        &db,
        &test_config(),
        &test_rooms(),
        &test_host_rooms(),
        now,
    )
    .await
    .expect("sweep the stalled provisions");
    let response = client
        .get(&format!("/v1/sessions/{session}"))
        .bearer(&caller.token)
        .send()
        .await;
    assert_eq!(
        response.json::<SessionDetail>().summary.state,
        SessionState::Provisioning,
        "a pending handoff sits out the provisioning sweep"
    );

    // Older than the upload deadline, though, the sender is gone: the
    // sweep fails the session and releases the reservation.
    let abandoned = now.saturating_sub(crate::handoffs::HANDOFF_DEADLINE_SECS + 1);
    sql!(
        db,
        "UPDATE handoffs SET created_at_unix = {abandoned} WHERE session_id = {session}"
    )
    .execute()
    .await
    .expect("age the handoff past its upload deadline");
    crate::app::fail_stalled_provisions(
        &db,
        &test_config(),
        &test_rooms(),
        &test_host_rooms(),
        now,
    )
    .await
    .expect("sweep the abandoned handoffs");
    let response = client
        .get(&format!("/v1/sessions/{session}"))
        .bearer(&caller.token)
        .send()
        .await;
    let failed = response.json::<SessionDetail>();
    assert_eq!(failed.summary.state, SessionState::Failed);
    assert!(
        failed
            .failure
            .expect("a failed session says why")
            .contains("handoff"),
        "the failure names the handoff that was never finished"
    );
}

#[skyzen::test]
async fn an_ordinary_session_answers_no_handoff(ctx: TestContext, kv: Kv, db: Db, queue: Queue) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db, seed_user(&db).await).await;
    let client = ctx.client(router);

    let response = client
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(&CreateSession {
            source: None,
            prompt: PROMPT.to_owned(),
            harness: HarnessKind::ClaudeCode,
            repo: REPO.to_owned(),
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
    let session = response.json::<SessionDetail>().summary.id;

    let daemon = daemon_tokens::issue(&db, caller.user.id, session)
        .await
        .expect("mint a daemon token")
        .token;
    client
        .get(&format!("/v1/sessions/{session}/handoff"))
        .bearer(&daemon)
        .send()
        .await
        .assert_status(404);
}
