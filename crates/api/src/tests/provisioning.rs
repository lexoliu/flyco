//! What actually puts a session on a machine, end to end.
//!
//! Driven against the byo-ssh driver, which is the one flyco can exercise
//! without a cloud account: a "machine" is a podman container on a
//! registered host, and the only thing standing between this test and a real
//! one is the SSH connection. That connection is where the driver's own
//! tests already cut — [`CommandRunner`] exists so the rendered scripts are
//! assertable — so the provisioner below is the *real* `SshExecutor` over a
//! runner that answers instead of dialling. No cloud call, no cloud
//! resource, no credentials that could be live.

use core::future::Future;

use flyco_core::{
    CreateSession, HarnessKind, MachineId, MachineState, Problem, ProviderAccountId,
    ProviderCredentials, SessionDetail, SessionId, SessionState, Usd, UserId,
};
use flyco_provider::byo_ssh::{ByoSsh, CommandOutcome, CommandRunner, container_name};
use flyco_provider::{
    ClaudeCredential, CloudProvider as _, DaemonBootstrap, HttpError, Machine, ProviderError,
    ProvisionRequest, byo_ssh,
};
use skyzen::routing::Router;
use skyzen::sql;
use skyzen_services::queue::{
    QueueBatch, QueueBatchDisposition, QueueMessage, QueueMessageDisposition, ReceiveOptions,
};
use skyzen_services::{Db, Kv, Queue};
use skyzen_test::mock::InMemoryQueue;
use skyzen_test::{TestClient, TestContext};

use crate::provisioning::{LinkedAccount, Provisioner};
use crate::provisioning_queue::{self, MAX_ATTEMPTS, ProvisioningJob};
use crate::testing::{
    HARNESS_TOKEN, machine_choice, migrated_router_on, seed_harness_account, seed_provider_account,
    seed_user, test_config, test_rooms,
};
use crate::{machines, session, sessions};

const REPO: &str = "lexoliu/flyco";

/// The opening instruction every test session is created with.
const PROMPT: &str = "audit the relay for dropped frames";

// ── The provisioner under test ──

/// What the registered host answers with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// The podman script ran and exited with this status.
    Podman { exit_status: u32 },
    /// The connection itself did not happen, which is the one failure worth
    /// asking again about.
    Unreachable,
}

/// Runs the driver's own scripts without opening a connection.
#[derive(Debug, Clone, Copy)]
struct CannedRunner {
    exit_status: u32,
}

impl CommandRunner for CannedRunner {
    fn run(
        &mut self,
        _script: &str,
    ) -> impl Future<Output = Result<CommandOutcome, ProviderError>> + Send {
        core::future::ready(Ok(CommandOutcome {
            exit_status: self.exit_status,
            output: "podman said something".to_owned(),
        }))
    }
}

/// The real byo-ssh driver, over a recorded connection.
///
/// Counts the provisions it was actually asked for — which is what "a
/// redelivered job does not provision twice" is an assertion about — and
/// keeps the last bootstrap, which is where the credentials the queue minted
/// and unsealed become visible.
#[derive(Debug)]
struct RecordedHost {
    answer: Answer,
    provisions: u32,
    bootstrap: Option<DaemonBootstrap>,
}

impl RecordedHost {
    const fn answering(answer: Answer) -> Self {
        Self {
            answer,
            provisions: 0,
            bootstrap: None,
        }
    }

    /// A host whose podman run succeeds.
    const fn healthy() -> Self {
        Self::answering(Answer::Podman { exit_status: 0 })
    }
}

impl Provisioner for RecordedHost {
    async fn provision(
        &mut self,
        account: &LinkedAccount,
        request: &ProvisionRequest,
    ) -> Result<Machine, ProviderError> {
        self.provisions = self.provisions.saturating_add(1);
        self.bootstrap = Some(request.bootstrap.clone());

        let ProviderCredentials::ByoSsh { host, .. } = account.credentials() else {
            panic!("these tests only link registered SSH hosts");
        };

        match self.answer {
            Answer::Podman { exit_status } => {
                byo_ssh::SshExecutor::new(ByoSsh::new(host.clone()), CannedRunner { exit_status })
                    .provision(request)
                    .await
            }
            Answer::Unreachable => Err(ProviderError::Transport(HttpError::Transport(
                "the registered host did not answer".to_owned(),
            ))),
        }
    }
}

// ── Fixtures ──

/// A signed-in caller with a linked host to provision onto.
struct Caller {
    user: UserId,
    token: String,
    account: ProviderAccountId,
}

async fn sign_in(kv: &Kv, db: &Db) -> Caller {
    let user = seed_user(db).await;
    let token = session::issue(kv, user.id).await.expect("issue a session");
    let account = seed_provider_account(db, user.id).await;
    Caller {
        user: user.id,
        token,
        account,
    }
}

async fn open(client: &TestClient<Router>, caller: &Caller) -> SessionDetail {
    let response = client
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(&CreateSession {
            prompt: PROMPT.to_owned(),
            harness: HarnessKind::ClaudeCode,
            repo: REPO.to_owned(),
            budget_limit: Usd::from_dollars(10),
            machine: Some(machine_choice(caller.account)),
            spot: true,
        })
        .send()
        .await;
    response.assert_status(201);
    response.json()
}

async fn read(client: &TestClient<Router>, caller: &Caller, session: SessionId) -> SessionDetail {
    let response = client
        .get(&format!("/v1/sessions/{session}"))
        .bearer(&caller.token)
        .send()
        .await;
    response.assert_status(200);
    response.json()
}

/// Takes every job the queue is holding, as a batch the consumer accepts.
///
/// The messages are acknowledged as they are taken, so a test that drains
/// twice sees the second batch empty unless something enqueued again.
async fn drain(queue: &Queue) -> QueueBatch<ProvisioningJob> {
    let taken = queue
        .receive_json::<ProvisioningJob>(ReceiveOptions::new().with_max_messages(16))
        .await
        .expect("read the provisioning queue");

    let mut messages = Vec::with_capacity(taken.len());
    for message in taken {
        queue.ack(&message.receipt).await.expect("settle a message");
        messages.push(QueueMessage {
            id: message.id.unwrap_or_default(),
            timestamp_ms: 0,
            body: message.body,
        });
    }

    QueueBatch {
        queue: "provisioning".to_owned(),
        messages,
    }
}

/// Runs whatever the queue is holding through the consumer.
async fn run_queue(
    db: &Db,
    queue: &Queue,
    provisioner: &mut RecordedHost,
) -> QueueBatchDisposition {
    let batch = drain(queue).await;
    provisioning_queue::consume(db, &test_config(), queue, &test_rooms(), provisioner, batch).await
}

/// Every job the queue holds, delivered or not.
///
/// A retry is sent with a delivery delay and the mock keeps it invisible
/// until that delay lapses against a real clock, so a test that only
/// received would never see one.
fn queued(backend: &InMemoryQueue) -> Vec<ProvisioningJob> {
    backend
        .messages()
        .iter()
        .map(|body| serde_json::from_slice(body).expect("a queued provisioning job"))
        .collect()
}

/// One job, as a batch the consumer accepts.
fn batch(job: ProvisioningJob) -> QueueBatch<ProvisioningJob> {
    QueueBatch {
        queue: "provisioning".to_owned(),
        messages: vec![QueueMessage {
            id: "delivery".to_owned(),
            timestamp_ms: 0,
            body: job,
        }],
    }
}

/// Runs one job twice, which is what an at-least-once queue eventually does.
async fn run_job_twice(
    db: &Db,
    queue: &Queue,
    provisioner: &mut RecordedHost,
    job: ProvisioningJob,
) {
    for _ in 0..2 {
        provisioning_queue::consume(
            db,
            &test_config(),
            queue,
            &test_rooms(),
            provisioner,
            batch(job),
        )
        .await;
    }
}

// ── Creation-time validation ──

#[skyzen::test]
async fn a_machine_the_account_cannot_deploy_is_refused_where_it_was_chosen(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);

    let mut choice = machine_choice(caller.account);
    choice.machine_type = "Standard_D96as_v5".to_owned();

    let refused = client
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(&CreateSession {
            prompt: PROMPT.to_owned(),
            harness: HarnessKind::ClaudeCode,
            repo: REPO.to_owned(),
            budget_limit: Usd::from_dollars(10),
            machine: Some(choice),
            spot: true,
        })
        .send()
        .await;

    refused.assert_status(422);
    let problem: Problem = refused.json();
    assert_eq!(
        problem.kind,
        "https://flyco.dev/problems/machine-unavailable"
    );
    assert!(
        problem.detail.contains("Standard_D96as_v5"),
        "the refusal names the machine that was asked for: {}",
        problem.detail
    );

    // Nothing was written and nothing was queued: the choice failed before
    // the session existed.
    let sessions: u32 = sql!(db, "SELECT COUNT(*) AS n FROM sessions")
        .fetch_scalar()
        .await
        .expect("count sessions");
    assert_eq!(sessions, 0);
    assert!(drain(&queue).await.is_empty());
}

// ── The queue, end to end ──

#[skyzen::test]
async fn a_created_session_gets_the_machine_it_asked_for(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);

    let session = open(&client, &caller).await.summary.id;
    let mut host = RecordedHost::healthy();
    run_queue(&db, &queue, &mut host).await;
    assert_eq!(host.provisions, 1);

    let machine = machines::for_session(&db, session)
        .await
        .expect("read the machine row")
        .expect("the session reserved a machine row when it was created");
    assert_eq!(machine.state, MachineState::Running);
    assert_eq!(
        machine.native_id.as_deref(),
        Some(&*container_name(machine.id))
    );

    // The machine exists; the agent does not answer yet, so the session is
    // still provisioning. A daemon reaching the control plane is what moves
    // it, and nothing else does.
    assert_eq!(
        read(&client, &caller, session).await.summary.state,
        SessionState::Provisioning
    );

    let token = client
        .post(&format!("/v1/sessions/{session}/daemon-token"))
        .bearer(&caller.token)
        .send()
        .await;
    token.assert_status(200);
    let token: flyco_core::DaemonToken = token.json();

    // Natively the upgrade itself is refused — there is no path that carries
    // a socket into a room off the Worker — but the credential check and the
    // lifecycle move both run first, which is the half this asserts.
    client
        .get(&format!("/v1/sessions/{session}/relay/daemon"))
        .bearer(&token.token)
        .send()
        .await;

    let live = read(&client, &caller, session).await;
    assert_eq!(live.summary.state, SessionState::Active);
    assert_eq!(live.failure, None);
}

#[skyzen::test]
async fn a_redelivered_job_does_not_provision_a_second_machine(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);

    let session = open(&client, &caller).await.summary.id;
    let job = drain(&queue).await.messages[0].body;

    let mut host = RecordedHost::healthy();
    run_job_twice(&db, &queue, &mut host, job).await;

    assert_eq!(
        host.provisions, 1,
        "the second delivery must not reach the provider: a second machine is \
         a cloud resource nobody is billing anybody for"
    );
    let machines: u32 = sql!(
        db,
        "SELECT COUNT(*) AS n FROM machines WHERE session_id = {session}"
    )
    .fetch_scalar()
    .await
    .expect("count machines");
    assert_eq!(machines, 1);
}

#[skyzen::test]
async fn a_provision_that_fails_leaves_the_session_visibly_failed(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);

    let session = open(&client, &caller).await.summary.id;
    let mut host = RecordedHost::answering(Answer::Podman { exit_status: 125 });
    run_queue(&db, &queue, &mut host).await;

    let failed = read(&client, &caller, session).await;
    assert_eq!(
        failed.summary.state,
        SessionState::Failed,
        "a session whose machine could not be built must not sit in `provisioning`"
    );
    let reason = failed.failure.expect("a failed session says why");
    assert!(
        reason.contains("125"),
        "the reason is the provider's own: {reason}"
    );

    // A podman exit is the host answering, not the connection failing, so
    // nothing was queued for another attempt.
    assert!(drain(&queue).await.is_empty());

    // And the slot is free again: a failed session holds no machine.
    let live = crate::sessions::live_count(&db, caller.user)
        .await
        .expect("count live sessions");
    assert_eq!(live, 0);
}

#[skyzen::test]
async fn an_unreachable_host_is_retried_a_bounded_number_of_times(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    // This test reads the queue's backend rather than receiving from it: a
    // retry is sent with a delivery delay, and the mock honours the delay
    // against a real clock. Receiving would report an empty queue and let
    // the test conclude that nothing was retried.
    let backend = InMemoryQueue::new();
    let queue = Queue::new(backend.clone());
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);

    let session = open(&client, &caller).await.summary.id;
    let mut host = RecordedHost::answering(Answer::Unreachable);
    let mut job = drain(&queue).await.messages[0].body;

    for attempt in 1..=MAX_ATTEMPTS {
        assert_eq!(job.attempt, attempt);
        provisioning_queue::consume(
            &db,
            &test_config(),
            &queue,
            &test_rooms(),
            &mut host,
            batch(job),
        )
        .await;

        if attempt == MAX_ATTEMPTS {
            break;
        }
        assert_eq!(
            read(&client, &caller, session).await.summary.state,
            SessionState::Provisioning,
            "attempt {attempt} failed transiently, so the session is still on its way"
        );
        job = *queued(&backend)
            .last()
            .expect("a transient failure queues the same machine again");
    }

    assert_eq!(host.provisions, MAX_ATTEMPTS);
    let failed = read(&client, &caller, session).await;
    assert_eq!(failed.summary.state, SessionState::Failed);
    assert!(
        failed
            .failure
            .as_ref()
            .is_some_and(|reason| reason.contains("gave up")),
        "the reason says it stopped trying: {:?}",
        failed.failure
    );
    assert!(
        queued(&backend)
            .iter()
            .all(|job| job.attempt <= MAX_ATTEMPTS),
        "nothing was queued past the last attempt"
    );
}

// ── Resuming ──

#[skyzen::test]
async fn an_archived_session_comes_back_through_the_same_queue(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);

    let session = open(&client, &caller).await.summary.id;
    let machine = machines::for_session(&db, session)
        .await
        .expect("read the machine row")
        .expect("a machine row")
        .id;
    let original = drain(&queue).await;

    client
        .post(&format!("/v1/sessions/{session}/archive"))
        .bearer(&caller.token)
        .send()
        .await
        .assert_status(200);

    let mut host = RecordedHost::healthy();
    let disposition = provisioning_queue::consume(
        &db,
        &test_config(),
        &queue,
        &test_rooms(),
        &mut host,
        original,
    )
    .await;
    assert!(matches!(disposition, QueueBatchDisposition::PerMessage(_)));
    assert_eq!(
        host.provisions, 0,
        "an archived session drops its stale job"
    );

    sessions::record_harness_session(&db, session, "harness-native-thread")
        .await
        .expect("record the identity the original daemon announced");

    let resumed = client
        .post(&format!("/v1/sessions/{session}/resume"))
        .bearer(&caller.token)
        .send()
        .await;
    resumed.assert_status(200);
    let resumed: SessionDetail = resumed.json();
    assert_eq!(resumed.summary.state, SessionState::Provisioning);

    let queued = drain(&queue).await;
    assert_eq!(queued.len(), 1);
    assert_eq!(
        queued.messages[0].body.machine, machine,
        "a resume rebuilds the session's own machine rather than a second one \
         beside it"
    );

    // Re-run it through the consumer to prove the resumed job is the same
    // path, not a second implementation of provisioning.
    let disposition = provisioning_queue::consume(
        &db,
        &test_config(),
        &queue,
        &test_rooms(),
        &mut host,
        queued,
    )
    .await;
    assert!(matches!(
        disposition,
        QueueBatchDisposition::PerMessage(ref decisions)
            if decisions == &[QueueMessageDisposition::Ack]
    ));
    assert_eq!(host.provisions, 1);
    assert_eq!(
        host.bootstrap
            .expect("the rebuilt machine was handed a bootstrap")
            .resume_session_id
            .as_deref(),
        Some("harness-native-thread"),
        "a resume continues the harness conversation the previous machine announced"
    );
    assert_eq!(
        machines::for_session(&db, session)
            .await
            .expect("read the machine row")
            .expect("a machine row")
            .state,
        MachineState::Running
    );
}

#[skyzen::test]
async fn a_running_session_cannot_be_resumed(ctx: TestContext, kv: Kv, db: Db, queue: Queue) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let session = open(&client, &caller).await.summary.id;

    let refused = client
        .post(&format!("/v1/sessions/{session}/resume"))
        .bearer(&caller.token)
        .send()
        .await;
    refused.assert_status(409);
    assert_eq!(
        refused.json::<Problem>().kind,
        "https://flyco.dev/problems/invalid-session-transition"
    );
}

#[skyzen::test]
async fn a_stranger_cannot_resume_somebody_elses_session(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let owner = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let session = open(&client, &owner).await.summary.id;

    let stranger = crate::testing::seed_other_user(&db).await;
    let stranger = session::issue(&kv, stranger.id)
        .await
        .expect("issue a session");

    client
        .post(&format!("/v1/sessions/{session}/resume"))
        .bearer(&stranger)
        .send()
        .await
        .assert_status(404);
}

// ── The queue nobody is holding ──

#[skyzen::test]
async fn a_job_for_a_session_that_is_gone_is_dropped(db: Db, queue: Queue) {
    crate::testing::migrate(&db).await;

    let mut host = RecordedHost::healthy();
    let orphan = ProvisioningJob::first(SessionId::generate(), MachineId::generate());

    // Acknowledged, not retried: redelivering a job whose session no longer
    // exists would only produce the same answer for ever.
    let disposition = provisioning_queue::consume(
        &db,
        &test_config(),
        &queue,
        &test_rooms(),
        &mut host,
        batch(orphan),
    )
    .await;
    assert!(matches!(
        disposition,
        QueueBatchDisposition::PerMessage(ref decisions)
            if decisions == &[QueueMessageDisposition::Ack]
    ));
    assert_eq!(host.provisions, 0);
}

#[skyzen::test]
async fn a_machine_boots_already_holding_its_session_credentials(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    seed_harness_account(&db, caller.user, HarnessKind::ClaudeCode).await;
    let client = ctx.client(router);

    let session = open(&client, &caller).await.summary.id;
    let mut host = RecordedHost::healthy();
    run_queue(&db, &queue, &mut host).await;

    let bootstrap = host.bootstrap.expect("the driver was handed a bootstrap");
    assert_eq!(bootstrap.session, session);
    assert_eq!(bootstrap.harness, HarnessKind::ClaudeCode);
    assert_eq!(bootstrap.control_plane_url, "https://flyco.test/");

    // The daemon token is this session's, and it is live: the queue minted it
    // rather than leaving the machine to be paired by hand afterwards.
    assert!(
        bootstrap
            .daemon_token
            .starts_with(flyco_core::DAEMON_TOKEN_PREFIX)
    );
    assert!(
        crate::daemon_tokens::authenticates(&db, session, &bootstrap.daemon_token)
            .await
            .expect("check the token")
    );

    // And the harness credential was unsealed on the way through, so the
    // agent on the machine can sign in.
    assert_eq!(
        bootstrap.claude_auth,
        ClaudeCredential::OauthToken {
            token: HARNESS_TOKEN.to_owned()
        }
    );
    assert_eq!(
        bootstrap.resume_session_id, None,
        "a first machine has no harness conversation to continue"
    );
}

#[skyzen::test]
async fn a_session_with_no_linked_harness_account_still_gets_a_machine(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);

    open(&client, &caller).await;
    let mut host = RecordedHost::healthy();
    run_queue(&db, &queue, &mut host).await;

    // Inheriting is the developer-machine mode: the harness comes up and
    // reports itself unauthenticated, which is a better answer than a
    // machine that never provisions because Anthropic was never linked.
    assert_eq!(
        host.bootstrap
            .expect("the driver was handed a bootstrap")
            .claude_auth,
        ClaudeCredential::Inherit
    );
}
