//! What actually puts a session on a machine, end to end.
//!
//! Driven against an enrolled host, which is the machine flyco can exercise
//! without a cloud account: a "machine" is a podman container on hardware
//! the user owns, and the only thing standing between this test and a real
//! one is the socket that machine holds. The provisioner below plans with
//! the *real* [`Host`] planner and answers where the room would be, so every
//! container name, every bootstrap and every retry decision is the deployed
//! one. No cloud call, no cloud resource, no credentials that could be live.
//!
//! What the room itself does with a job — holding it for a machine that is
//! away, and the `JobResult` that completes the row — is
//! [`crate::tests::hosts`].

use core::future::Future;

use flyco_core::{
    CreateSession, HarnessKind, MachineId, MachineState, Problem, ProviderAccountId,
    ProviderCredentials, SessionDetail, SessionId, SessionState, Usd, UserId,
};
use flyco_provider::host::container_name;
use flyco_provider::{
    ClaudeCredential, DaemonBootstrap, HarnessCredential, HttpError, Machine, MachineOperation,
    ProviderError, ProvisionRequest,
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
use crate::rooms::Rooms;
use crate::testing::{
    GITHUB_ACCESS_TOKEN, GITHUB_COMMIT_EMAIL, GITHUB_NAME, HARNESS_TOKEN, TEST_DEFAULT_BRANCH,
    TestGithub, machine_choice, migrated_router_on, seed_harness_account, seed_provider_account,
    seed_user, test_config, test_host_rooms, test_rooms, test_vendors,
};
use crate::vendors::Vendors;
use crate::{machines, session, sessions};

const REPO: &str = "lexoliu/flyco";

/// A branch a session names for itself, distinct from the repository's
/// default — so a test that mixed the two up would fail rather than pass.
const NAMED_BRANCH: &str = "feat/issue-73-repo-clone";

/// The opening instruction every test session is created with.
const PROMPT: &str = "audit the relay for dropped frames";

// ── The provisioner under test ──

/// What the machine's room answers with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// The room took the job, which is what a connected machine looks like.
    Takes,
    /// The room refused it. The machine answered; nothing is going to change
    /// on a second attempt.
    Refused,
    /// The room could not be reached at all, which is the one failure worth
    /// asking again about.
    Unreachable,
}

/// The real planner, over a room that answers instead of holding a socket.
///
/// Counts the provisions it was actually asked for — which is what "a
/// redelivered job does not provision twice" is an assertion about — and
/// keeps the last bootstrap, which is where the credentials the queue minted
/// and unsealed become visible.
#[derive(Debug)]
struct RecordedHost {
    answer: Answer,
    provisions: u32,
    /// Every machine this host was asked to start again, in order.
    ///
    /// A recovery is a *start*, never a provision, and the two counters are
    /// separate so a recovery that quietly rebuilt the machine — and with
    /// it the volume holding the session's work — would fail rather than
    /// pass.
    restarts: Vec<Machine>,
    bootstrap: Option<DaemonBootstrap>,
}

impl RecordedHost {
    const fn answering(answer: Answer) -> Self {
        Self {
            answer,
            provisions: 0,
            restarts: Vec::new(),
            bootstrap: None,
        }
    }

    /// A machine whose room takes what it is sent.
    const fn healthy() -> Self {
        Self::answering(Answer::Takes)
    }
}

impl Provisioner for RecordedHost {
    fn provision(
        &mut self,
        account: &LinkedAccount,
        request: &ProvisionRequest,
    ) -> impl Future<Output = Result<Machine, ProviderError>> {
        // Nothing here suspends: planning is pure, and where the deployed
        // provisioner posts the job to a room this one answers for it.
        core::future::ready(self.plan(account, request))
    }

    fn restart(
        &mut self,
        _account: &LinkedAccount,
        machine: &Machine,
    ) -> impl Future<Output = Result<Machine, ProviderError>> {
        if self.answer == Answer::Unreachable {
            return core::future::ready(Err(ProviderError::Transport(HttpError::Transport(
                "the machine's room did not answer".to_owned(),
            ))));
        }
        self.restarts.push(machine.clone());
        core::future::ready(Ok(Machine {
            state: MachineState::Running,
            ..machine.clone()
        }))
    }
}

impl RecordedHost {
    /// What the deployed provisioner does for a machine somebody owns, minus
    /// the room it posts to.
    fn plan(
        &mut self,
        account: &LinkedAccount,
        request: &ProvisionRequest,
    ) -> Result<Machine, ProviderError> {
        self.provisions = self.provisions.saturating_add(1);
        self.bootstrap = Some(request.bootstrap.clone());

        let ProviderCredentials::Host { .. } = account.credentials() else {
            panic!("these tests only provision onto enrolled machines");
        };
        let planner = account
            .host_planner()
            .expect("a host account names the machine it provisions onto");
        // The real planner, so a container name or a refused machine type is
        // the deployed answer rather than a fixture's opinion.
        let job = planner.plan(&MachineOperation::Provision(Box::new(request.clone())))?;

        match self.answer {
            Answer::Takes => Ok(Machine {
                id: request.machine,
                native_id: job.container().to_owned(),
                region: planner.machine_type().to_owned(),
                state: MachineState::Running,
                capacity_mode: flyco_provider::CapacityMode::OnDemand,
                address: Some(planner.machine_type().to_owned()),
            }),
            Answer::Refused => Err(ProviderError::Rejected(
                "this machine refused the container job".to_owned(),
            )),
            Answer::Unreachable => Err(ProviderError::Transport(HttpError::Transport(
                "the machine's room did not answer".to_owned(),
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
            branch: None,
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
    kv: &Kv,
    queue: &Queue,
    provisioner: &mut RecordedHost,
) -> QueueBatchDisposition {
    run_queue_as(db, kv, queue, provisioner, TestGithub::default()).await
}

/// The same, against a GitHub that says something else about the caller's
/// stored token.
async fn run_queue_as(
    db: &Db,
    kv: &Kv,
    queue: &Queue,
    provisioner: &mut RecordedHost,
    github: TestGithub,
) -> QueueBatchDisposition {
    let batch = drain(queue).await;
    provisioning_queue::consume(
        db,
        &test_config(),
        kv,
        queue,
        &test_rooms(),
        &mut clients(provisioner, &github, &test_vendors()),
        batch,
    )
    .await
}

/// The same, against rooms the caller keeps so it can read them back.
async fn run_queue_watching(
    db: &Db,
    kv: &Kv,
    queue: &Queue,
    rooms: &Rooms,
    provisioner: &mut RecordedHost,
    batch: QueueBatch<ProvisioningJob>,
) -> QueueBatchDisposition {
    provisioning_queue::consume(
        db,
        &test_config(),
        kv,
        queue,
        rooms,
        &mut clients(provisioner, &TestGithub::default(), &test_vendors()),
        batch,
    )
    .await
}

/// The services one job reaches, wired to the fakes.
///
/// The vendor pair is built per call rather than borrowed from the caller:
/// nothing in these tests varies it, and a `Vendors` that outlived the call
/// would need a lifetime the fakes do not have.
fn clients<'a>(
    provisioner: &'a mut RecordedHost,
    github: &'a TestGithub,
    vendors: &'a Vendors,
) -> provisioning_queue::Clients<'a, RecordedHost, TestGithub> {
    provisioning_queue::Clients {
        provisioner,
        vendors,
        github,
    }
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
    kv: &Kv,
    queue: &Queue,
    provisioner: &mut RecordedHost,
    job: &ProvisioningJob,
) {
    for _ in 0..2 {
        provisioning_queue::consume(
            db,
            &test_config(),
            kv,
            queue,
            &test_rooms(),
            &mut clients(provisioner, &TestGithub::default(), &test_vendors()),
            batch(job.clone()),
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
            branch: None,
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
    run_queue(&db, &kv, &queue, &mut host).await;
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
    let job = drain(&queue).await.messages.remove(0).body;

    let mut host = RecordedHost::healthy();
    run_job_twice(&db, &kv, &queue, &mut host, &job).await;

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
async fn a_machine_that_stops_making_progress_fails_instead_of_spinning(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let session = open(&client, &caller).await.summary.id;
    let rooms = test_rooms();

    // The machine was built and then went quiet: the daemon crash-loops,
    // or it cannot reach the control plane, and nothing else will ever
    // notice — the queue's job is done and the daemon is the thing that
    // would report the failure.
    let now = crate::clock::now_unix();
    let stalled = now.saturating_sub(flyco_core::PROVISION_DEADLINE_SECS + 1);
    sql!(
        db,
        "UPDATE sessions SET created_at_unix = {stalled}, last_active_unix = {stalled} \
         WHERE id = {session}"
    )
    .execute()
    .await
    .expect("age the session past the deadline");

    crate::app::fail_stalled_provisions(&db, &test_config(), &rooms, &test_host_rooms(), now)
        .await
        .expect("sweep the stalled provisions");

    let failed = read(&client, &caller, session).await;
    assert_eq!(failed.summary.state, SessionState::Failed);
    let reason = failed.failure.expect("a failed session says why");
    assert!(
        reason.contains("never reported its agent ready"),
        "the reason names what did not happen: {reason}"
    );

    // And the machine goes with it: a machine that never came up is still
    // a machine running up a bill.
    let machine = crate::machines::for_session(&db, session)
        .await
        .expect("read the machine row")
        .expect("the session reserved a row");
    assert_eq!(machine.state, flyco_core::MachineState::Destroyed);
}

#[skyzen::test]
async fn a_machine_that_outlives_its_session_is_released_by_the_sweep(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let session = open(&client, &caller).await.summary.id;
    let rooms = test_rooms();

    // A built machine whose session then ended without it: the shape every
    // interrupted release path leaves behind, and the one a daemon's
    // failure report leaves when its own process stops mid-request.
    let running = flyco_core::MachineState::Running;
    sql!(
        db,
        "UPDATE machines SET state = {running} WHERE session_id = {session}"
    )
    .execute()
    .await
    .expect("mark the machine built");
    crate::sessions::fail(
        &db,
        &rooms,
        session,
        "the harness never mounted flyco's server",
    )
    .await
    .expect("fail the session");

    crate::app::release_ended_machines(&db, &test_config(), &test_host_rooms())
        .await
        .expect("sweep the machines of ended sessions");

    let machine = crate::machines::for_session(&db, session)
        .await
        .expect("read the machine row")
        .expect("the session reserved a row");
    assert_eq!(
        machine.state,
        flyco_core::MachineState::Destroyed,
        "no session flyco has stopped may go on holding a machine"
    );
}

#[skyzen::test]
async fn a_machine_still_making_progress_is_not_called_stalled(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let session = open(&client, &caller).await.summary.id;
    let rooms = test_rooms();

    // Opened long ago, but a stage arrived a moment ago: a big repository
    // on a cold image is slow, not broken.
    let now = crate::clock::now_unix();
    let opened = now.saturating_sub(flyco_core::PROVISION_DEADLINE_SECS + 1);
    sql!(
        db,
        "UPDATE sessions SET created_at_unix = {opened}, last_active_unix = {opened} \
         WHERE id = {session}"
    )
    .execute()
    .await
    .expect("age the session past the deadline");
    crate::sessions::note_progress(&db, session)
        .await
        .expect("record a stage");

    crate::app::fail_stalled_provisions(&db, &test_config(), &rooms, &test_host_rooms(), now)
        .await
        .expect("sweep the stalled provisions");

    assert_eq!(
        read(&client, &caller, session).await.summary.state,
        SessionState::Provisioning,
        "a machine that is still reporting stages keeps being built"
    );
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
    let mut host = RecordedHost::answering(Answer::Refused);
    let rooms = test_rooms();
    run_queue_watching(&db, &kv, &queue, &rooms, &mut host, drain(&queue).await).await;

    let failed = read(&client, &caller, session).await;
    assert_eq!(
        failed.summary.state,
        SessionState::Failed,
        "a session whose machine could not be built must not sit in `provisioning`"
    );

    // The page is watching the room, not polling the row: the failure has
    // to arrive as an event or the timeline spins until a reload.
    assert!(
        recorded(&rooms, session).await.iter().any(|event| matches!(
            event,
            flyco_core::ClientEvent::SessionStateChanged {
                state: SessionState::Failed
            }
        )),
        "the room is told the session failed"
    );

    // The reservation is released with the session: a machine that was
    // never built is not `starting`.
    let machine = crate::machines::for_session(&db, session)
        .await
        .expect("read the machine row")
        .expect("the session reserved a row");
    assert_eq!(machine.state, flyco_core::MachineState::Destroyed);
    let reason = failed.failure.expect("a failed session says why");
    assert!(
        reason.contains("refused"),
        "the reason is the machine's own: {reason}"
    );

    // A refusal is the machine answering, not the connection failing, so
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
    let mut job = drain(&queue).await.messages.remove(0).body;

    for attempt in 1..=MAX_ATTEMPTS {
        assert_eq!(job.attempt(), Some(attempt));
        provisioning_queue::consume(
            &db,
            &test_config(),
            &kv,
            &queue,
            &test_rooms(),
            &mut clients(&mut host, &TestGithub::default(), &test_vendors()),
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
        job = queued(&backend)
            .last()
            .expect("a transient failure queues the same machine again")
            .clone();
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
            .all(|job| job.attempt().is_none_or(|attempt| attempt <= MAX_ATTEMPTS)),
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
        &kv,
        &queue,
        &test_rooms(),
        &mut clients(&mut host, &TestGithub::default(), &test_vendors()),
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
        queued.messages[0].body.machine(),
        Some(machine),
        "a resume rebuilds the session's own machine rather than a second one \
         beside it"
    );

    // Re-run it through the consumer to prove the resumed job is the same
    // path, not a second implementation of provisioning.
    let disposition = provisioning_queue::consume(
        &db,
        &test_config(),
        &kv,
        &queue,
        &test_rooms(),
        &mut clients(&mut host, &TestGithub::default(), &test_vendors()),
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
async fn a_job_for_a_session_that_is_gone_is_dropped(kv: Kv, db: Db, queue: Queue) {
    crate::testing::migrate(&db).await;

    let mut host = RecordedHost::healthy();
    let orphan = ProvisioningJob::first(SessionId::generate(), MachineId::generate());

    // Acknowledged, not retried: redelivering a job whose session no longer
    // exists would only produce the same answer for ever.
    let disposition = provisioning_queue::consume(
        &db,
        &test_config(),
        &kv,
        &queue,
        &test_rooms(),
        &mut clients(&mut host, &TestGithub::default(), &test_vendors()),
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
    run_queue(&db, &kv, &queue, &mut host).await;

    let bootstrap = host.bootstrap.expect("the driver was handed a bootstrap");
    assert_eq!(bootstrap.session, session);
    assert_eq!(bootstrap.auth.harness(), HarnessKind::ClaudeCode);
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
        bootstrap.auth,
        HarnessCredential::ClaudeCode(ClaudeCredential::OauthToken {
            token: HARNESS_TOKEN.to_owned()
        })
    );
    assert_eq!(
        bootstrap.resume_session_id, None,
        "a first machine has no harness conversation to continue"
    );
}

#[skyzen::test]
async fn a_machine_boots_holding_the_users_enabled_mcp_registry_and_nothing_else(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);

    for (name, enabled) in [("git", true), ("retired", false)] {
        client
            .post("/v1/mcp-servers")
            .bearer(&caller.token)
            .json(&flyco_core::UpsertMcpServer {
                name: name.to_owned(),
                config: flyco_core::McpServerConfig::Stdio {
                    command: "uvx".to_owned(),
                    args: vec!["mcp-server-git".to_owned()],
                    env: Vec::new(),
                },
                enabled,
            })
            .send()
            .await
            .assert_status(201);
    }

    open(&client, &caller).await;
    let mut host = RecordedHost::healthy();
    run_queue(&db, &kv, &queue, &mut host).await;

    let mounted = host
        .bootstrap
        .expect("the driver was handed a bootstrap")
        .mcp_servers;

    // The list is the allowlist the machine writes into its harness's
    // root-owned config, so a server the user switched off is not one the
    // session can reach — there is no second place for it to come from.
    assert_eq!(
        mounted
            .iter()
            .map(|server| server.name.as_str())
            .collect::<Vec<_>>(),
        ["git"]
    );
    assert_eq!(
        mounted[0].config,
        flyco_core::McpServerConfig::Stdio {
            command: "uvx".to_owned(),
            args: vec!["mcp-server-git".to_owned()],
            env: Vec::new(),
        }
    );
}

#[skyzen::test]
async fn a_machine_boots_knowing_what_to_check_out_and_who_to_commit_as(
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
    run_queue(&db, &kv, &queue, &mut host).await;

    let repo = host
        .bootstrap
        .expect("the driver was handed a bootstrap")
        .repo;
    assert_eq!(repo.slug.to_string(), REPO);
    assert_eq!(
        repo.branch.to_string(),
        TEST_DEFAULT_BRANCH,
        "a session that named no branch works on the repository's default"
    );
    // Behaving as the user, not as a bot: the machine holds the caller's own
    // GitHub token, unsealed on the way through, and commits under the
    // caller's own identity.
    assert_eq!(repo.token, GITHUB_ACCESS_TOKEN);
    assert_eq!(repo.identity.name, GITHUB_NAME);
    assert_eq!(repo.identity.email, GITHUB_COMMIT_EMAIL);
}

#[skyzen::test]
async fn a_session_carries_the_branch_it_was_opened_on(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);

    let response = client
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(&CreateSession {
            prompt: PROMPT.to_owned(),
            harness: HarnessKind::ClaudeCode,
            repo: REPO.to_owned(),
            branch: Some(NAMED_BRANCH.to_owned()),
            budget_limit: Usd::from_dollars(10),
            machine: Some(machine_choice(caller.account)),
            spot: true,
        })
        .send()
        .await;
    response.assert_status(201);
    let session: SessionDetail = response.json();
    assert_eq!(
        session
            .summary
            .branch
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some(NAMED_BRANCH),
        "the branch is recorded where the caller named it"
    );

    let mut host = RecordedHost::healthy();
    run_queue(&db, &kv, &queue, &mut host).await;
    assert_eq!(
        host.bootstrap
            .expect("the driver was handed a bootstrap")
            .repo
            .branch
            .to_string(),
        NAMED_BRANCH,
        "and it is the branch the machine is told to check out"
    );
}

#[skyzen::test]
async fn a_branch_git_would_refuse_is_refused_where_it_was_typed(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);

    let response = client
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(&CreateSession {
            prompt: PROMPT.to_owned(),
            harness: HarnessKind::ClaudeCode,
            repo: REPO.to_owned(),
            branch: Some("not a branch".to_owned()),
            budget_limit: Usd::from_dollars(10),
            machine: Some(machine_choice(caller.account)),
            spot: true,
        })
        .send()
        .await;

    response.assert_status(422);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/invalid-branch"
    );
    assert!(
        drain(&queue).await.messages.is_empty(),
        "nothing was queued for a session that was never opened"
    );
}

#[skyzen::test]
async fn a_token_without_the_repo_scope_cannot_open_a_session(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    // The user signed flyco in before it asked for `repo`, or narrowed the
    // authorization afterwards. Either way the only fix is signing in again,
    // and they are told so in the moment they pressed send.
    crate::testing::migrate(&db).await;
    let router = crate::testing::test_router_with_github(
        db.clone(),
        queue.clone(),
        TestGithub::without_repo_scope(),
    );
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);

    let response = client
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(&CreateSession {
            prompt: PROMPT.to_owned(),
            harness: HarnessKind::ClaudeCode,
            repo: REPO.to_owned(),
            branch: None,
            budget_limit: Usd::from_dollars(10),
            machine: Some(machine_choice(caller.account)),
            spot: true,
        })
        .send()
        .await;

    response.assert_status(403);
    let problem: Problem = response.json();
    assert_eq!(
        problem.kind,
        "https://flyco.dev/problems/github-token-insufficient"
    );
    assert!(
        problem.detail.contains("repo") && problem.detail.contains("sign in"),
        "the refusal names the scope and what to do about it: {}",
        problem.detail
    );
}

#[skyzen::test]
async fn a_stored_token_that_lost_the_repo_scope_fails_the_session_visibly(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    // The same refusal on the resume path, where nobody is watching a form:
    // the session was opened while the token was still good, and the machine
    // is built later. It must fail loudly rather than provision a machine
    // whose clone cannot authenticate.
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);

    let session = open(&client, &caller).await.summary.id;
    let mut host = RecordedHost::healthy();
    run_queue_as(
        &db,
        &kv,
        &queue,
        &mut host,
        TestGithub::without_repo_scope(),
    )
    .await;

    assert_eq!(
        host.provisions, 0,
        "no machine is built for a checkout that could not be authenticated"
    );
    let failed = read(&client, &caller, session).await;
    assert_eq!(failed.summary.state, SessionState::Failed);
    let reason = failed.failure.expect("a failed session says why");
    assert!(
        reason.contains("repo") && reason.contains("sign in"),
        "the session names the missing scope and the fix: {reason}"
    );
}

#[skyzen::test]
async fn a_session_opened_before_flyco_tracked_branches_resolves_one_once(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    // Migration 0015 leaves those rows NULL rather than claiming `main`.
    // The queue resolves the repository's default branch from GitHub and
    // writes it back, so the answer is settled once and stays stable even if
    // the repository's default moves later.
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);

    let session = open(&client, &caller).await.summary.id;
    sql!(db, "UPDATE sessions SET branch = NULL WHERE id = {session}")
        .execute()
        .await
        .expect("age the row back to before branches were recorded");

    let mut host = RecordedHost::healthy();
    run_queue(&db, &kv, &queue, &mut host).await;

    assert_eq!(
        host.bootstrap
            .expect("the driver was handed a bootstrap")
            .repo
            .branch
            .to_string(),
        TEST_DEFAULT_BRANCH
    );
    let stored: Option<String> = sql!(db, "SELECT branch FROM sessions WHERE id = {session}")
        .fetch_scalar_optional()
        .await
        .expect("read the row back");
    assert_eq!(
        stored.as_deref(),
        Some(TEST_DEFAULT_BRANCH),
        "the resolved branch is written back rather than resolved again next time"
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
    run_queue(&db, &kv, &queue, &mut host).await;

    // Inheriting is the developer-machine mode: the harness comes up and
    // reports itself unauthenticated, which is a better answer than a
    // machine that never provisions because Anthropic was never linked.
    assert_eq!(
        host.bootstrap
            .expect("the driver was handed a bootstrap")
            .auth,
        HarnessCredential::ClaudeCode(ClaudeCredential::Inherit)
    );
}

// ── What the agent is told, and what it is allowed to change ──
//
// The daemon-scoped `agent/*` routes are what flycod's local MCP server is
// made of (docs/ux.md §9.5, issue #65). They are exercised here rather than
// beside the other machine routes because they need a session that has
// actually been provisioned: a machine row with a price and a size on it,
// and a daemon token that proves which session is asking.

/// Mints the `fd_` token a session's daemon authenticates with.
async fn pair(client: &TestClient<Router>, caller: &Caller, session: SessionId) -> String {
    let response = client
        .post(&format!("/v1/sessions/{session}/daemon-token"))
        .bearer(&caller.token)
        .send()
        .await;
    response.assert_status(200);
    response.json::<flyco_core::DaemonToken>().token
}

/// Opens a session, builds its machine, and pairs a daemon with it.
async fn provisioned(
    client: &TestClient<Router>,
    caller: &Caller,
    db: &Db,
    kv: &Kv,
    queue: &Queue,
) -> (SessionId, String) {
    let session = open(client, caller).await.summary.id;
    let mut host = RecordedHost::healthy();
    run_queue(db, kv, queue, &mut host).await;
    let token = pair(client, caller, session).await;
    (session, token)
}

#[skyzen::test]
async fn the_bootstrap_tells_the_daemon_which_machine_and_who_chose_it(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);

    // `open` names a machine, which is what makes the choice the user's.
    open(&client, &caller).await;
    let mut host = RecordedHost::healthy();
    run_queue(&db, &kv, &queue, &mut host).await;

    let bootstrap = host.bootstrap.expect("the driver was handed a bootstrap");
    assert_eq!(bootstrap.machine_origin, flyco_core::MachineOrigin::User);
    assert_eq!(
        bootstrap.machine.machine_type,
        machine_choice(caller.account).machine_type
    );
    // An enrolled machine is hardware the user already owns: flyco meters
    // nothing on it, so it quotes no price rather than quoting zero. It does
    // quote a size, because the machine measured itself and said so.
    assert_eq!(bootstrap.machine.hourly, None);
    assert_eq!(
        bootstrap.machine.capacity,
        Some(crate::testing::host_facts().capacity())
    );
    assert!(!bootstrap.machine.is_license_bound());
}

#[skyzen::test]
async fn the_agent_reads_its_machine_and_who_chose_it(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let (session, daemon) = provisioned(&client, &caller, &db, &kv, &queue).await;

    let response = client
        .get(&format!("/v1/sessions/{session}/agent/machine"))
        .bearer(&daemon)
        .send()
        .await;
    response.assert_status(200);

    let view: flyco_core::AgentMachineView = response.json();
    assert_eq!(view.origin, flyco_core::MachineOrigin::User);
    assert_eq!(view.state, MachineState::Running);
    assert_eq!(
        view.machine.machine_type,
        machine_choice(caller.account).machine_type
    );
}

#[skyzen::test]
async fn the_agents_machine_routes_refuse_a_credential_that_is_not_its_own(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let (session, _) = provisioned(&client, &caller, &db, &kv, &queue).await;

    // A session token opens the user's own routes and none of the daemon's:
    // the agent is never handed a credential that could reach another
    // session, and the middleware is what makes that true.
    client
        .get(&format!("/v1/sessions/{session}/agent/machine"))
        .bearer(&caller.token)
        .send()
        .await
        .assert_status(401);
}

#[skyzen::test]
async fn the_agent_sees_only_the_types_its_session_can_become(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let (session, daemon) = provisioned(&client, &caller, &db, &kv, &queue).await;

    let response = client
        .get(&format!("/v1/sessions/{session}/agent/machine/catalog"))
        .bearer(&daemon)
        .send()
        .await;
    response.assert_status(200);

    // A registered host offers exactly itself: it is the hardware it is, and
    // the curated catalog says so rather than inventing sizes to move
    // between.
    let catalog: Vec<flyco_core::MachineCatalogEntry> = response.json();
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].account, Some(caller.account));
    assert!(matches!(
        catalog[0].pricing,
        flyco_core::MachinePricing::UserOwned
    ));
}

#[skyzen::test]
async fn an_agent_may_not_resize_to_a_type_its_session_is_not_offered(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let (session, daemon) = provisioned(&client, &caller, &db, &kv, &queue).await;

    let response = client
        .post(&format!("/v1/sessions/{session}/agent/machine/resize"))
        .bearer(&daemon)
        .json(&flyco_core::ResizeMachine {
            machine_type: "mac2.metal".to_owned(),
        })
        .send()
        .await;
    response.assert_status(422);

    let problem: Problem = response.json();
    assert!(problem.kind.ends_with("machine-type-not-offered"));
    assert!(problem.detail.contains("mac2.metal"));
}

#[skyzen::test]
async fn approving_a_license_bound_resize_is_what_moves_the_machine(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let (session, _) = provisioned(&client, &caller, &db, &kv, &queue).await;

    // The approval the daemon raises instead of resizing, recorded the same
    // way the daemon's own `POST /v1/sessions/{id}/approvals` records it.
    let approval = crate::approvals::raise(
        &db,
        session,
        &flyco_core::ApprovalPayload::MachineResizeLicenseBound {
            machine_type: "mac2.metal".to_owned(),
            minimum: flyco_core::BillingMinimum::new(24, Usd::from_cents(65)),
            reason: "the build needs a signed macOS toolchain".to_owned(),
        },
    )
    .await
    .expect("raise the approval");

    let response = client
        .post(&format!("/v1/approvals/{approval}/decision"))
        .bearer(&caller.token)
        .json(&flyco_core::DecideApproval {
            decision: flyco_core::ApprovalDecision::Approved,
        })
        .send()
        .await;

    // Approving *is* the resize: the control plane acts on the machine
    // rather than handing the decision back to the agent. This session runs
    // on hardware the user registered, which cannot become a Mac, so what
    // comes back is the refusal that proves the attempt was made — a
    // decision that changed nothing would have answered 200.
    response.assert_status(422);
    let problem: Problem = response.json();
    assert!(problem.kind.ends_with("machine-type-not-offered"));

    // And the decision itself stands: the user did allow it.
    let approvals = crate::approvals::list(&db, caller.user, Some(session), None)
        .await
        .expect("list approvals");
    assert_eq!(approvals[0].state, flyco_core::ApprovalState::Approved);
}

#[skyzen::test]
async fn denying_a_license_bound_resize_leaves_the_machine_alone(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let (session, _) = provisioned(&client, &caller, &db, &kv, &queue).await;

    let approval = crate::approvals::raise(
        &db,
        session,
        &flyco_core::ApprovalPayload::MachineResizeLicenseBound {
            machine_type: "mac2.metal".to_owned(),
            minimum: flyco_core::BillingMinimum::new(24, Usd::from_cents(65)),
            reason: "the build needs a signed macOS toolchain".to_owned(),
        },
    )
    .await
    .expect("raise the approval");

    let response = client
        .post(&format!("/v1/approvals/{approval}/decision"))
        .bearer(&caller.token)
        .json(&flyco_core::DecideApproval {
            decision: flyco_core::ApprovalDecision::Denied,
        })
        .send()
        .await;
    response.assert_status(200);

    let machine = machines::for_session(&db, session)
        .await
        .expect("read the machine")
        .expect("the session has one");
    assert_eq!(machine.state, MachineState::Running);
    assert_eq!(
        machine.session_machine().machine_type,
        machine_choice(caller.account).machine_type
    );
}

#[skyzen::test]
async fn a_users_own_resize_is_not_gated_on_a_licence(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let (session, _) = provisioned(&client, &caller, &db, &kv, &queue).await;

    // The user's route reaches the same catalog check and the same refusal
    // for a type this session cannot become — the licence gate is the one
    // difference between it and the agent's, and it does not apply here
    // because a person decided.
    let response = client
        .post(&format!("/v1/sessions/{session}/machine/resize"))
        .bearer(&caller.token)
        .json(&flyco_core::ResizeMachine {
            machine_type: "mac2.metal".to_owned(),
        })
        .send()
        .await;
    response.assert_status(422);
    assert!(
        response
            .json::<Problem>()
            .kind
            .ends_with("machine-type-not-offered")
    );
}

// ── Spot reclamation ──
//
// Flyco migrates the session itself and the agent takes no part in it
// (issue #66): the daemon spends the provider's notice saving the session,
// and everything below is what the control plane does with what it reports.
// The disk is never released, so recovering is a *start* of the same
// machine — never a provision, which would build a second one and leave the
// session's work on a disk nobody is attached to.

/// One line of the append-only ledger.
#[derive(Debug, skyzen::FromRow)]
struct LedgerLine {
    detail: String,
}

#[skyzen::test]
async fn a_session_going_live_tells_the_room_it_did(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let session = open(&client, &caller).await.summary.id;
    let rooms = test_rooms();

    sessions::daemon_arrived(&db, &rooms, session)
        .await
        .expect("the session's daemon reached the control plane");

    // The page is watching the room, not polling the row: the last
    // provisioning stage says the agent is up, not that the session is, so
    // without this frame the header goes on counting a provisioning clock
    // while the agent answers below it (issue #209).
    assert!(
        recorded(&rooms, session).await.iter().any(|event| matches!(
            event,
            flyco_core::ClientEvent::SessionStateChanged {
                state: SessionState::Active
            }
        )),
        "the room is told the session went live"
    );

    // A daemon reconnects after every eviction, redeploy and dropped
    // socket, and none of those is a lifecycle event to announce again.
    let after_first = recorded(&rooms, session).await.len();
    sessions::daemon_arrived(&db, &rooms, session)
        .await
        .expect("a daemon reconnects");
    assert_eq!(
        recorded(&rooms, session).await.len(),
        after_first,
        "a reconnect is not a second transition"
    );
}

/// Reports a reclamation the way the session's own daemon does.
///
/// The session is taken live first, because that is the only state a
/// reclamation is interesting from: a daemon that reports a notice is one
/// that reached the control plane, which is what makes its session active.
async fn report_reclaim(
    client: &TestClient<Router>,
    db: &Db,
    session: SessionId,
    daemon_token: &str,
    seconds_remaining: u32,
) {
    sessions::daemon_arrived(db, &test_rooms(), session)
        .await
        .expect("the session's daemon reached the control plane");
    let response = client
        .post(&format!("/v1/sessions/{session}/spot-notice"))
        .bearer(daemon_token)
        .json(&flyco_core::ReportSpotNotice { seconds_remaining })
        .send()
        .await;
    response.assert_status(202);
}

/// The one recovery a reclaimed session's queue is holding.
fn queued_recovery(backend: &InMemoryQueue) -> ProvisioningJob {
    let recoveries: Vec<ProvisioningJob> = queued(backend)
        .into_iter()
        .filter(|job| matches!(job, ProvisioningJob::Recover { .. }))
        .collect();
    assert_eq!(recoveries.len(), 1, "one notice queues one recovery");
    recoveries.into_iter().next().expect("exactly one recovery")
}

/// Every `ClientEvent` the room recorded, in order.
async fn recorded(rooms: &Rooms, session: SessionId) -> Vec<flyco_core::ClientEvent> {
    rooms
        .events(session, 0)
        .await
        .expect("read the room's stream")
        .events
        .iter()
        .map(|stored| {
            serde_json::from_value(stored.event.clone()).expect("a recorded client event")
        })
        .collect()
}

#[skyzen::test]
async fn every_stage_the_queue_announces_is_one_something_can_time(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    // `Booting` and `Installing` used to be announced microseconds apart,
    // with nothing between them but a struct and a D1 write, so every
    // session's timeline read `Booting  0s` — a row with no interval behind
    // it (issue #225). The control plane can see exactly two moments: when
    // it asks the provider, and when the provider has handed back a machine
    // that is durably flyco's. So it announces exactly two stages.
    let queue = Queue::new(InMemoryQueue::new());
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let session = open(&client, &caller).await.summary.id;

    let mut host = RecordedHost::healthy();
    let job = drain(&queue).await;
    let rooms = test_rooms();
    run_queue_watching(&db, &kv, &queue, &rooms, &mut host, job).await;

    let stages: Vec<flyco_core::ProvisioningStage> = recorded(&rooms, session)
        .await
        .into_iter()
        .filter_map(|event| match event {
            flyco_core::ClientEvent::ProvisioningStage { stage, .. } => Some(stage),
            _ => None,
        })
        .collect();
    assert_eq!(
        stages,
        vec![
            flyco_core::ProvisioningStage::Reserving,
            flyco_core::ProvisioningStage::Booting,
        ],
        "the queue announces only what it can time; the rest is the daemon's"
    );
}

#[skyzen::test]
async fn a_reclaimed_session_reads_as_interrupted_and_queues_its_own_recovery(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let backend = InMemoryQueue::new();
    let queue = Queue::new(backend.clone());
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let (session, daemon) = provisioned(&client, &caller, &db, &kv, &queue).await;
    let machine = machines::for_session(&db, session)
        .await
        .expect("read the machine row")
        .expect("a machine row")
        .id;

    report_reclaim(&client, &db, session, &daemon, 30).await;

    let detail = read(&client, &caller, session).await;
    assert_eq!(detail.summary.state, SessionState::Interrupted);
    assert_eq!(
        detail.summary.interrupted_reason,
        Some(flyco_core::InterruptedReason::SpotReclaimed),
        "the status the user reads is `Interrupted · spot reclaimed`, not a session that stopped"
    );

    // Queued rather than performed: the machine holds its disk for the
    // seconds the provider announced, and a start against a running
    // instance is not a restart.
    assert!(matches!(
        queued_recovery(&backend),
        ProvisioningJob::Recover { session: queued, machine: on, .. }
            if queued == session && on == machine
    ));
}

#[skyzen::test]
async fn a_recovery_starts_the_same_machine_rather_than_building_another(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let backend = InMemoryQueue::new();
    let queue = Queue::new(backend.clone());
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);

    let session = open(&client, &caller).await.summary.id;
    let mut host = RecordedHost::healthy();
    let first = drain(&queue).await;
    let rooms = test_rooms();
    run_queue_watching(&db, &kv, &queue, &rooms, &mut host, first).await;
    let daemon = pair(&client, &caller, session).await;
    let built = machines::for_session(&db, session)
        .await
        .expect("read the machine row")
        .expect("a machine row");
    let native = built.native_id.clone().expect("the machine was built");

    report_reclaim(&client, &db, session, &daemon, 30).await;
    let recovery = queued_recovery(&backend);
    let disposition =
        run_queue_watching(&db, &kv, &queue, &rooms, &mut host, batch(recovery)).await;
    assert!(matches!(
        disposition,
        QueueBatchDisposition::PerMessage(ref decisions)
            if decisions == &[QueueMessageDisposition::Ack]
    ));

    assert_eq!(
        host.provisions, 1,
        "a recovery must never provision: the disk holding the session's work is on the \
         machine that already exists"
    );
    assert_eq!(host.restarts.len(), 1);
    assert_eq!(
        host.restarts[0].native_id, native,
        "the machine started again is the machine that was reclaimed"
    );

    let row = machines::for_session(&db, session)
        .await
        .expect("read the machine row")
        .expect("a machine row");
    assert_eq!(row.id, built.id, "the same row, on the same disk");
    assert_eq!(row.state, MachineState::Running);

    // `Migrating`: the session is provisioning again, and the reason it
    // lost its machine is what tells that apart from a first provision.
    let detail = read(&client, &caller, session).await;
    assert_eq!(detail.summary.state, SessionState::Provisioning);
    assert_eq!(
        detail.summary.interrupted_reason,
        Some(flyco_core::InterruptedReason::SpotReclaimed)
    );

    // And the timeline the transcript renders it as.
    let stages: Vec<flyco_core::ProvisioningStage> = recorded(&rooms, session)
        .await
        .into_iter()
        .filter_map(|event| match event {
            flyco_core::ClientEvent::ProvisioningStage { stage, .. } => Some(stage),
            _ => None,
        })
        .collect();
    assert!(
        stages.ends_with(&[
            flyco_core::ProvisioningStage::Reserving,
            flyco_core::ProvisioningStage::Booting,
        ]),
        "a recovery announces the stages it actually goes through: {stages:?}"
    );
}

#[skyzen::test]
async fn a_recovered_session_is_told_afterwards_and_the_ledger_names_the_replacement(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let backend = InMemoryQueue::new();
    let queue = Queue::new(backend.clone());
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);

    let session = open(&client, &caller).await.summary.id;
    let mut host = RecordedHost::healthy();
    let first = drain(&queue).await;
    let rooms = test_rooms();
    run_queue_watching(&db, &kv, &queue, &rooms, &mut host, first).await;
    let daemon = pair(&client, &caller, session).await;

    report_reclaim(&client, &db, session, &daemon, 30).await;
    let recovery = queued_recovery(&backend);
    run_queue_watching(&db, &kv, &queue, &rooms, &mut host, batch(recovery)).await;

    // The agent is told once, afterwards, as an ordinary message in the
    // conversation — which is also what puts it in the transcript.
    let messages: Vec<String> = recorded(&rooms, session)
        .await
        .into_iter()
        .filter_map(|event| match event {
            flyco_core::ClientEvent::UserMessage { text } => Some(text),
            _ => None,
        })
        .collect();
    let expected = format!(
        "The machine was reclaimed and restarted on {}; continue.",
        machine_choice(caller.account).machine_type
    );
    assert_eq!(
        messages
            .iter()
            .filter(|text| text.contains(&expected))
            .count(),
        1,
        "the agent is told once, naming the machine it is on now: {messages:?}"
    );

    // The gap is what the ledger has to explain: the compute meter stopped
    // and restarted while the disk was billed throughout.
    let details: Vec<String> = sql!(
        db,
        "SELECT detail FROM spend_events \
         WHERE budget_id = (SELECT budget_id FROM sessions WHERE id = {session})"
    )
    .fetch_all::<LedgerLine>()
    .await
    .expect("read the ledger")
    .into_iter()
    .map(|line| line.detail)
    .collect();
    assert_eq!(
        details
            .iter()
            .filter(|detail| detail.contains("was reclaimed at"))
            .count(),
        1,
        "the ledger names the replacement exactly once: {details:?}"
    );
}

#[skyzen::test]
async fn a_redelivered_recovery_neither_restarts_twice_nor_bills_twice(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    // Cloudflare Queues delivers at least once, and a recovery is the one
    // job that runs against a machine that already exists.
    let backend = InMemoryQueue::new();
    let queue = Queue::new(backend.clone());
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);

    let session = open(&client, &caller).await.summary.id;
    let mut host = RecordedHost::healthy();
    let first = drain(&queue).await;
    let rooms = test_rooms();
    run_queue_watching(&db, &kv, &queue, &rooms, &mut host, first).await;
    let daemon = pair(&client, &caller, session).await;

    report_reclaim(&client, &db, session, &daemon, 30).await;
    let recovery = queued_recovery(&backend);
    for _ in 0..2 {
        run_queue_watching(&db, &kv, &queue, &rooms, &mut host, batch(recovery.clone())).await;
    }

    assert_eq!(host.provisions, 1);
    let entries: u32 = sql!(
        db,
        "SELECT COUNT(*) AS n FROM spend_events \
         WHERE meter_key LIKE 'spot-recovery:%' \
         AND budget_id = (SELECT budget_id FROM sessions WHERE id = {session})"
    )
    .fetch_scalar()
    .await
    .expect("count the ledger");
    assert_eq!(
        entries, 1,
        "the ledger line is keyed on the reclamation, so a second delivery writes nothing"
    );
}

#[skyzen::test]
async fn a_daemon_that_comes_back_ends_the_migration(ctx: TestContext, kv: Kv, db: Db) {
    let backend = InMemoryQueue::new();
    let queue = Queue::new(backend.clone());
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let (session, daemon) = provisioned(&client, &caller, &db, &kv, &queue).await;

    report_reclaim(&client, &db, session, &daemon, 30).await;
    sessions::recovering(&db, session)
        .await
        .expect("the recovery moves the session back to provisioning");
    // What the daemon on the restarted machine does when it reaches the
    // control plane.
    sessions::daemon_arrived(&db, &test_rooms(), session)
        .await
        .expect("the daemon reached the control plane");

    let detail = read(&client, &caller, session).await;
    assert_eq!(detail.summary.state, SessionState::Active);
    assert_eq!(
        detail.summary.interrupted_reason, None,
        "a session whose daemon is back is not migrating any more"
    );
}

#[skyzen::test]
async fn the_conversation_a_restarted_daemon_continues_comes_from_the_control_plane(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    // A reclaimed machine boots the configuration already on its disk,
    // which was written when the machine was *created*. The id it must
    // resume is the one the control plane recorded, so the daemon asks
    // rather than trusting the file.
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let (session, daemon) = provisioned(&client, &caller, &db, &kv, &queue).await;

    let before = client
        .get(&format!("/v1/sessions/{session}/harness-session"))
        .bearer(&daemon)
        .send()
        .await;
    before.assert_status(200);
    assert_eq!(
        before
            .json::<flyco_core::HarnessSessionView>()
            .harness_session_id,
        None,
        "a session whose harness has never announced itself has no conversation to continue"
    );

    sessions::record_harness_session(&db, session, "harness-native-thread")
        .await
        .expect("record what the daemon announced");

    let after = client
        .get(&format!("/v1/sessions/{session}/harness-session"))
        .bearer(&daemon)
        .send()
        .await;
    after.assert_status(200);
    assert_eq!(
        after
            .json::<flyco_core::HarnessSessionView>()
            .harness_session_id,
        Some("harness-native-thread".to_owned())
    );
}
