//! What actually puts a session on a machine, end to end.
//!
//! Driven against an enrolled host, which is the machine flyco can exercise
//! without a cloud account: a "machine" is a podman container on hardware
//! the user owns, and the only thing standing between this test and a real
//! one is the attachment that machine holds. The provisioner below plans with
//! the *real* [`Host`] planner and answers where the room would be, so every
//! container name, every bootstrap and every retry decision is the deployed
//! one. No cloud call, no cloud resource, no credentials that could be live.
//!
//! What the room itself does with a job — holding it for a machine that is
//! away, and the `JobResult` that completes the row — is
//! [`crate::tests::hosts`].

use core::future::Future;

use flyco_core::{
    CloudProviderKind, CreateSession, HarnessKind, MachineCapacity, MachineCatalogEntry,
    MachineChoice, MachineId, MachinePricing, MachineState, OsFamily, Problem, ProviderAccountId,
    ProviderCredentials, Runtime, SessionDetail, SessionId, SessionState, StoragePricing, Usd,
    UserId,
};
use flyco_provider::host::container_name;
use flyco_provider::{
    ClaudeCredential, Continuation, DaemonBootstrap, HarnessCredential, HttpError, Machine,
    MachineOperation, ProviderError, ProvisionRequest, Provisioning,
};
use skyzen::routing::Router;
use skyzen::sql;
use skyzen_services::queue::{
    QueueBatch, QueueBatchDisposition, QueueMessage, QueueMessageDisposition, ReceiveOptions,
};
use skyzen_services::{Db, Kv, Queue};
use skyzen_test::mock::InMemoryQueue;
use skyzen_test::{TestClient, TestContext};

use crate::catalog::{self, RegionCatalog, RegionOutcome};
use crate::provisioning::{LinkedAccount, Provisioner};
use crate::provisioning_queue::{self, MAX_ATTEMPTS, ProvisioningJob};
use crate::rooms::Rooms;
use crate::testing::{
    GITHUB_ACCESS_TOKEN, GITHUB_COMMIT_EMAIL, GITHUB_NAME, HARNESS_TOKEN, TEST_DEFAULT_BRANCH,
    TestGithub, machine_choice, migrated_router_on, seed_azure_account, seed_harness_account,
    seed_provider_account, seed_user, test_config, test_host_rooms, test_rooms, test_vendors,
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
    /// The room took the job but the machine was still coming up when the
    /// call's budget ran out: the build is handed back, and the next call
    /// finds it running.
    Slow,
    /// The provider no longer holds the machine at all — a codespace that
    /// was deleted between the reconcile that last saw it and the start the
    /// recovery asked for.
    Gone,
}

/// The real planner, over a room that answers instead of holding an
/// attachment.
///
/// Counts the provisions it was actually asked for — which is what "a
/// redelivered job does not provision twice" is an assertion about — and
/// keeps the last bootstrap, which is where the credentials the queue minted
/// and unsealed become visible.
/// What a slow build is known by before its execution has a name.
const SLOW_JOB: &str = "flyco-slow-job";
/// Where the slow host says it got to.
const SLOW_CONTINUATION: &str = "still starting";

#[derive(Debug)]
struct RecordedHost {
    answer: Answer,
    provisions: u32,
    resumes: u32,
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
            resumes: 0,
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
    ) -> impl Future<Output = Result<Provisioning, ProviderError>> {
        // Nothing here suspends: planning is pure, and where the deployed
        // provisioner posts the job to a room this one answers for it.
        let slow = self.answer == Answer::Slow;
        core::future::ready(self.plan(account, request).map(|machine| {
            if slow {
                Provisioning::Pending {
                    machine: Machine {
                        native_id: SLOW_JOB.to_owned(),
                        state: MachineState::Provisioning,
                        ..machine
                    },
                    continuation: Continuation::write(&SLOW_CONTINUATION).expect("a string"),
                }
            } else {
                Provisioning::Ready(machine)
            }
        }))
    }

    fn resume(
        &mut self,
        _account: &LinkedAccount,
        machine: &Machine,
        continuation: &Continuation,
    ) -> impl Future<Output = Result<Provisioning, ProviderError>> {
        self.resumes = self.resumes.saturating_add(1);
        assert_eq!(
            continuation
                .read::<String>()
                .expect("the continuation this host wrote"),
            SLOW_CONTINUATION,
            "the queue hands back exactly what the provider handed it"
        );
        assert_eq!(
            machine.native_id, SLOW_JOB,
            "resumed on the pending machine"
        );
        core::future::ready(Ok(Provisioning::Ready(Machine {
            native_id: format!("{SLOW_JOB}/run-1"),
            state: MachineState::Running,
            ..machine.clone()
        })))
    }

    fn restart(
        &mut self,
        _account: &LinkedAccount,
        machine: &Machine,
    ) -> impl Future<Output = Result<Machine, ProviderError>> {
        match self.answer {
            Answer::Unreachable => {
                return core::future::ready(Err(ProviderError::Transport(HttpError::Transport(
                    "the machine's room did not answer".to_owned(),
                ))));
            }
            Answer::Gone => {
                return core::future::ready(Err(ProviderError::Gone(format!(
                    "the provider holds nothing named {}",
                    machine.native_id
                ))));
            }
            _ => {}
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

        if !matches!(account.credentials(), ProviderCredentials::Host { .. }) {
            // A cloud account answers the way its driver would: a machine
            // the provider named, already running. What these tests exercise
            // is how the queue got to the call — the catalog it priced, the
            // bootstrap it minted — not the provider's side of it.
            return match self.answer {
                Answer::Unreachable => Err(ProviderError::Transport(HttpError::Transport(
                    "the provider could not be reached".to_owned(),
                ))),
                _ => Ok(Machine {
                    id: request.machine,
                    native_id: "provider-native".to_owned(),
                    runtime: request.spec.runtime,
                    region: request.spec.region.clone(),
                    state: MachineState::Running,
                    capacity_mode: flyco_provider::CapacityMode::OnDemand,
                    address: None,
                }),
            };
        }
        let planner = account
            .host_planner()
            .expect("a host account names the machine it provisions onto");
        // The real planner, so a container name or a refused machine type is
        // the deployed answer rather than a fixture's opinion.
        let job = planner.plan(&MachineOperation::Provision(Box::new(request.clone())))?;

        match self.answer {
            Answer::Takes | Answer::Slow => Ok(Machine {
                id: request.machine,
                native_id: job.container().to_owned(),
                runtime: flyco_core::Runtime::Container,
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
            Answer::Gone => Err(ProviderError::Gone(
                "the provider holds nothing under that name".to_owned(),
            )),
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
            source: None,
            prompt: PROMPT.to_owned(),
            harness: HarnessKind::ClaudeCode,
            repos: vec![flyco_core::RepoSelection {
                repo: REPO.to_owned(),
                branch: None,
            }],
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
            source: None,
            prompt: PROMPT.to_owned(),
            harness: HarnessKind::ClaudeCode,
            repos: vec![flyco_core::RepoSelection {
                repo: REPO.to_owned(),
                branch: None,
            }],
            budget_limit: Usd::from_dollars(10),
            machine: Some(choice),
            spot: true,
            model: None,
            permission_mode: None,
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

    // The daemon's attach is the lifecycle move: it authenticates the
    // `fd_` token, mints the room's epoch, and reports the session live.
    let attached = client
        .post(&format!("/v1/sessions/{session}/relay/attach"))
        .bearer(&token.token)
        .json(&flyco_core::wire::DaemonAttach {
            protocol_version: flyco_core::WIRE_PROTOCOL_VERSION,
        })
        .send()
        .await;
    attached.assert_status(200);

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
    run_queue(&db, &kv, &queue, &mut RecordedHost::healthy()).await;
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

    crate::app::fail_stalled_provisions(
        &db,
        &test_config(),
        &TestGithub::default(),
        &rooms,
        &test_host_rooms(),
        now,
    )
    .await
    .expect("sweep the stalled provisions");

    let failed = read(&client, &caller, session).await;
    assert_eq!(failed.summary.state, SessionState::Failed);
    let reason = failed.failure.expect("a failed session says why");
    assert!(
        reason.contains("never reported its agent ready"),
        "the reason names what did not happen: {reason}"
    );
    // Releasing the machine goes through the host's own room, which this
    // test does not connect; the reservation-only case below is where the
    // release is pinned.
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

    crate::app::release_ended_machines(
        &db,
        &test_config(),
        &TestGithub::default(),
        &test_host_rooms(),
    )
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
async fn a_machine_the_provider_never_finished_is_not_said_to_have_been_built(
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

    // The provider never answered: the queue's provision is still in flight
    // or still failing, so the row is the reservation and nothing more. The
    // sentence has to say that rather than send anyone looking for a daemon
    // on a machine that does not exist.
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

    crate::app::fail_stalled_provisions(
        &db,
        &test_config(),
        &TestGithub::default(),
        &rooms,
        &test_host_rooms(),
        now,
    )
    .await
    .expect("sweep the stalled provisions");

    let failed = read(&client, &caller, session).await;
    assert_eq!(failed.summary.state, SessionState::Failed);
    let reason = failed.failure.expect("a failed session says why");
    assert!(
        reason.contains("had not finished building the machine after 15 minutes"),
        "the reason says the machine never existed: {reason}"
    );
    assert!(!reason.contains("was built"), "{reason}");

    // And the reservation goes with it: nothing was built, so there is
    // nothing to ask the provider for, and the row is closed outright.
    let machine = crate::machines::for_session(&db, session)
        .await
        .expect("read the machine row")
        .expect("the session reserved a row");
    assert_eq!(machine.state, flyco_core::MachineState::Destroyed);
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

    crate::app::fail_stalled_provisions(
        &db,
        &test_config(),
        &TestGithub::default(),
        &rooms,
        &test_host_rooms(),
        now,
    )
    .await
    .expect("sweep the stalled provisions");

    assert_eq!(
        read(&client, &caller, session).await.summary.state,
        SessionState::Provisioning,
        "a machine that is still reporting stages keeps being built"
    );
}

#[skyzen::test]
async fn a_build_the_provider_hands_back_is_carried_on_in_its_own_leg(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    // Measured: a cold image took Azure six minutes, and the one invocation
    // that started it died on the Worker's subrequest ceiling (issue #257).
    // The provider now hands the build back; the queue records what exists
    // of the machine, asks for the rest later, and finishes it then.
    let backend = InMemoryQueue::new();
    let queue = Queue::new(backend.clone());
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);

    let session = open(&client, &caller).await.summary.id;
    let mut host = RecordedHost::answering(Answer::Slow);
    run_queue(&db, &kv, &queue, &mut host).await;

    assert_eq!(
        read(&client, &caller, session).await.summary.state,
        SessionState::Provisioning,
        "a build in progress is a session still waiting for its machine"
    );
    let pending = crate::machines::for_session(&db, session)
        .await
        .expect("read the machine row")
        .expect("the session reserved a row");
    assert_eq!(
        pending.native_id.as_deref(),
        Some(SLOW_JOB),
        "what the provider created so far is recorded, so a stall can destroy it"
    );
    assert_eq!(pending.state, flyco_core::MachineState::Provisioning);
    let mut legs = queued(&backend);
    assert!(
        matches!(
            legs.as_slice(),
            [ProvisioningJob::Continue { session: queued, machine, attempt: 1, .. }]
                if *queued == session && *machine == pending.id
        ),
        "one continuation, for this machine: {legs:?}"
    );
    let leg = legs.remove(0);

    // A `Provision` delivered again meanwhile does not start a second
    // machine beside the one being built.
    let redelivered = batch(ProvisioningJob::first(session, pending.id));
    run_queue_watching(&db, &kv, &queue, &test_rooms(), &mut host, redelivered).await;
    assert_eq!(host.provisions, 1, "the continuation owns the build");

    // The next leg — delivered after its delay, which the in-memory queue
    // honours and this test does not wait out — finds the machine up.
    run_queue_watching(&db, &kv, &queue, &test_rooms(), &mut host, batch(leg)).await;
    assert_eq!(host.resumes, 1);
    let built = crate::machines::for_session(&db, session)
        .await
        .expect("read the machine row")
        .expect("the session reserved a row");
    assert_eq!(built.state, flyco_core::MachineState::Running);
    assert_eq!(built.native_id.as_deref(), Some("flyco-slow-job/run-1"));
    assert_eq!(
        queued(&backend)
            .iter()
            .filter(|job| matches!(job, ProvisioningJob::Continue { .. }))
            .count(),
        1,
        "a finished build asks for nothing more: the one continuation ever queued is the \
         leg this test took by hand"
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

    let bootstrap = host.bootstrap.expect("the driver was handed a bootstrap");
    let repo = &bootstrap.repos[0];
    assert_eq!(repo.slug.to_string(), REPO);
    assert_eq!(
        repo.branch.to_string(),
        TEST_DEFAULT_BRANCH,
        "a session that named no branch works on the repository's default"
    );
    // Behaving as the user, not as a bot: the machine holds the caller's own
    // GitHub token, unsealed on the way through, and commits under the
    // caller's own identity.
    assert_eq!(bootstrap.github.token, GITHUB_ACCESS_TOKEN);
    assert_eq!(bootstrap.github.identity.name, GITHUB_NAME);
    assert_eq!(bootstrap.github.identity.email, GITHUB_COMMIT_EMAIL);
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
            source: None,
            prompt: PROMPT.to_owned(),
            harness: HarnessKind::ClaudeCode,
            repos: vec![flyco_core::RepoSelection {
                repo: REPO.to_owned(),
                branch: Some(NAMED_BRANCH.to_owned()),
            }],
            budget_limit: Usd::from_dollars(10),
            machine: Some(machine_choice(caller.account)),
            spot: true,
            model: None,
            permission_mode: None,
        })
        .send()
        .await;
    response.assert_status(201);
    let session: SessionDetail = response.json();
    assert_eq!(
        session.summary.repos[0]
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
            .repos[0]
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
            source: None,
            prompt: PROMPT.to_owned(),
            harness: HarnessKind::ClaudeCode,
            repos: vec![flyco_core::RepoSelection {
                repo: REPO.to_owned(),
                branch: Some("not a branch".to_owned()),
            }],
            budget_limit: Usd::from_dollars(10),
            machine: Some(machine_choice(caller.account)),
            spot: true,
            model: None,
            permission_mode: None,
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
            source: None,
            prompt: PROMPT.to_owned(),
            harness: HarnessKind::ClaudeCode,
            repos: vec![flyco_core::RepoSelection {
                repo: REPO.to_owned(),
                branch: None,
            }],
            budget_limit: Usd::from_dollars(10),
            machine: Some(machine_choice(caller.account)),
            spot: true,
            model: None,
            permission_mode: None,
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
    sql!(
        db,
        "UPDATE session_repos SET branch = NULL WHERE session_id = {session}"
    )
    .execute()
    .await
    .expect("age the row back to before branches were recorded");

    let mut host = RecordedHost::healthy();
    run_queue(&db, &kv, &queue, &mut host).await;

    assert_eq!(
        host.bootstrap
            .expect("the driver was handed a bootstrap")
            .repos[0]
            .branch
            .to_string(),
        TEST_DEFAULT_BRANCH
    );
    let stored: Option<String> = sql!(
        db,
        "SELECT branch FROM session_repos WHERE session_id = {session}"
    )
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

/// Where the Azure fixture says its machines are.
const AZURE_REGION: &str = "eastus";
/// The type the fixture's catalog offers there.
const AZURE_MACHINE_TYPE: &str = "Standard_D4als_v6";

/// One Azure machine type, priced so a provision that used it can be told
/// apart from one that asked the provider — which cannot be asked at all:
/// the seeded account has no resource group, so any catalog read fails
/// before it reaches the network.
fn azure_entry(account: ProviderAccountId) -> MachineCatalogEntry {
    MachineCatalogEntry {
        account: Some(account),
        provider: CloudProviderKind::Azure,
        region: AZURE_REGION.to_owned(),
        machine_type: AZURE_MACHINE_TYPE.to_owned(),
        runtime: Runtime::Vm,
        free_grant: None,
        os: OsFamily::Linux,
        capacity: Some(MachineCapacity {
            vcpus: 4,
            memory_mib: 16 * 1024,
        }),
        lineage: None,
        pricing: MachinePricing::Metered {
            on_demand_hourly: Usd::from_micros(160_000),
            spot_hourly: Some(Usd::from_micros(30_000)),
            minimum: None,
            storage: StoragePricing::PerGibHourly {
                rate: Usd::from_micros(10),
            },
        },
    }
}

/// The session a test opens against the Azure fixture.
async fn open_azure(
    client: &TestClient<Router>,
    caller: &Caller,
    account: ProviderAccountId,
) -> SessionDetail {
    let response = client
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(&CreateSession {
            source: None,
            prompt: PROMPT.to_owned(),
            harness: HarnessKind::ClaudeCode,
            repos: vec![flyco_core::RepoSelection {
                repo: REPO.to_owned(),
                branch: None,
            }],
            budget_limit: Usd::from_dollars(10),
            machine: Some(MachineChoice {
                provider_account: account,
                machine_type: AZURE_MACHINE_TYPE.to_owned(),
                runtime: Runtime::Vm,
                region: AZURE_REGION.to_owned(),
                spot: true,
                disk_gib: flyco_core::DEFAULT_DISK_GIB,
            }),
            spot: true,
            model: None,
            permission_mode: None,
        })
        .send()
        .await;
    response.assert_status(201);
    response.json()
}

#[skyzen::test]
async fn a_provision_is_priced_from_the_cached_catalog(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);

    let account = seed_azure_account(&db, caller.user).await;
    catalog::record_region(
        &kv,
        account,
        RegionCatalog {
            region: AZURE_REGION.to_owned(),
            read_at_unix: crate::clock::now_unix(),
            outcome: RegionOutcome::Offered {
                entries: vec![azure_entry(account)],
            },
        },
    )
    .await
    .expect("cache the account's catalog");

    let session = open_azure(&client, &caller, account).await.summary.id;
    let mut host = RecordedHost::healthy();
    run_queue(&db, &kv, &queue, &mut host).await;

    // The provider could not have been read — the account has no resource
    // group — so a machine that came up was priced by the document the
    // picker wrote, and the price it carries is the document's.
    assert_eq!(host.provisions, 1);
    let machine = machines::for_session(&db, session)
        .await
        .expect("read the machine row")
        .expect("the session reserved a machine row");
    assert_eq!(machine.state, MachineState::Running);
    let bootstrap = host.bootstrap.expect("the driver was handed a bootstrap");
    assert_eq!(bootstrap.machine.machine_type, AZURE_MACHINE_TYPE);
    assert_eq!(bootstrap.machine.hourly, Some(Usd::from_micros(30_000)));
}

#[skyzen::test]
async fn a_provision_reads_on_demand_when_the_cache_cannot_answer(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);

    let account = seed_azure_account(&db, caller.user).await;
    catalog::record_region(
        &kv,
        account,
        RegionCatalog {
            region: AZURE_REGION.to_owned(),
            read_at_unix: crate::clock::now_unix(),
            outcome: RegionOutcome::Offered {
                entries: vec![azure_entry(account)],
            },
        },
    )
    .await
    .expect("cache the account's catalog");

    let session = open_azure(&client, &caller, account).await.summary.id;

    // The document is emptied between the choice and the job: the provision
    // now has to ask the provider, and that read fails the honest way —
    // before the network — rather than inventing an entry.
    catalog::record_account(&kv, account, Vec::new(), crate::clock::now_unix())
        .await
        .expect("empty the account's document");

    let mut host = RecordedHost::healthy();
    run_queue(&db, &kv, &queue, &mut host).await;

    assert_eq!(host.provisions, 0);
    let detail = read(&client, &caller, session).await;
    assert_eq!(detail.summary.state, SessionState::Failed);
    assert!(
        detail
            .failure
            .as_deref()
            .is_some_and(|failure| failure.contains("resource group")),
        "the session should fail with the provider's own refusal, got {:?}",
        detail.failure
    );
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

    // A daemon re-attaches after every eviction, redeploy and dropped
    // stream, and none of those is a lifecycle event to announce again.
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
async fn a_stopping_container_marks_its_machine_and_queues_nothing(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    // The container counterpart of a spot notice, and deliberately not the
    // same route. A reclaimed virtual machine keeps its disk and has a
    // recovery queued against it; a stopping container has already handed
    // its working tree over as the `workdir-patch` and there is nothing to
    // schedule against a deadline. What the control plane keeps is the mark
    // the container drivers read to tell an execution that was asked to go
    // from one that died.
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

    let response = client
        .post(&format!("/v1/sessions/{session}/stopping"))
        .bearer(&daemon)
        .json(&flyco_core::ReportStopping {
            reason: flyco_core::StopReason::Sigterm,
        })
        .send()
        .await;
    response.assert_status(202);

    let reason: Option<flyco_core::StopReason> = sql!(
        db,
        "SELECT stopping_reason FROM machines WHERE id = {machine}"
    )
    .fetch_scalar()
    .await
    .expect("read the machine row back");
    assert_eq!(reason, Some(flyco_core::StopReason::Sigterm));

    let since: Option<u64> = sql!(
        db,
        "SELECT stopping_since_unix FROM machines WHERE id = {machine}"
    )
    .fetch_scalar()
    .await
    .expect("read the machine row back");
    assert!(
        since.is_some(),
        "the instant is what says the session is safe to start elsewhere"
    );

    assert!(
        queued(&backend)
            .into_iter()
            .all(|job| !matches!(job, ProvisioningJob::Recover { .. })),
        "a container that stopped has no disk to recover onto"
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
            flyco_core::ClientEvent::UserMessage { text, .. } => Some(text),
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

// ── A machine the provider let go of ──
//
// Codespaces is the provider that suspends and deletes machines out from
// under flyco, which is why the interruptions that are not a reclaim take
// their tests here: every one of them ends at a `Recover` job — the same
// job a reclaim takes — and the only thing that differs is what the row's
// `native_id` decides when it gets there. The host provisioner stands in
// for the codespaces driver: the row's native name is a container name,
// which is the same kind of string either way.

/// Marks the session's machine lost the way the reconcile sweep would: the
/// row destroyed and unnamed, the session interrupted as `MachineLost`.
async fn machine_lost(db: &Db, session: SessionId) {
    let row = machines::for_session(db, session)
        .await
        .expect("read the machine row")
        .expect("the session has a machine");
    machines::mark_lost(db, row.id)
        .await
        .expect("the row is released");
    sessions::machine_lost(db, session)
        .await
        .expect("the session is interrupted");
}

/// Interrupts a live session the way a suspension does, so the test that
/// follows is about what wakes it.
async fn suspended(db: &Db, session: SessionId) {
    sessions::interrupted(db, session, flyco_core::InterruptedReason::Suspended)
        .await
        .expect("the session is interrupted");
}

#[skyzen::test]
async fn a_suspended_session_is_woken_by_its_next_message(ctx: TestContext, kv: Kv, db: Db) {
    let backend = InMemoryQueue::new();
    let queue = Queue::new(backend.clone());
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let (session, _daemon) = provisioned(&client, &caller, &db, &kv, &queue).await;
    let rooms = test_rooms();
    sessions::daemon_arrived(&db, &rooms, session)
        .await
        .expect("the daemon arrived");
    suspended(&db, session).await;

    let response = client
        .post(&format!("/v1/sessions/{session}/messages"))
        .bearer(&caller.token)
        .json(&flyco_core::SendMessage {
            text: "are you still there?".to_owned(),
        })
        .send()
        .await;
    response.assert_status(202);

    let jobs = queued(&backend);
    let recovery = jobs
        .iter()
        .find(|job| matches!(job, ProvisioningJob::Recover { .. }))
        .expect("the message enqueued the machine's start");
    let ProvisioningJob::Recover { cause, .. } = recovery else {
        unreachable!()
    };
    assert_eq!(*cause, provisioning_queue::RecoveryCause::Suspended);

    let mut host = RecordedHost::healthy();
    let disposition =
        run_queue_watching(&db, &kv, &queue, &rooms, &mut host, batch(recovery.clone())).await;
    assert!(matches!(
        disposition,
        QueueBatchDisposition::PerMessage(ref decisions)
            if decisions == &[QueueMessageDisposition::Ack]
    ));

    assert_eq!(
        host.restarts.len(),
        1,
        "the held machine is started by name"
    );
    assert_eq!(host.provisions, 0, "nothing new is provisioned for it");
    assert!(
        recorded(&rooms, session)
            .await
            .iter()
            .any(|e| matches!(e, flyco_core::ClientEvent::UserMessage { text, .. } if text.contains("suspended for inactivity"))),
        "the agent is told its machine was suspended and restarted"
    );
}

#[skyzen::test]
async fn a_machine_lost_session_provisions_around_its_next_message(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let backend = InMemoryQueue::new();
    let queue = Queue::new(backend.clone());
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let (session, _daemon) = provisioned(&client, &caller, &db, &kv, &queue).await;
    let rooms = test_rooms();
    sessions::daemon_arrived(&db, &rooms, session)
        .await
        .expect("the daemon arrived");
    machine_lost(&db, session).await;

    let response = client
        .post(&format!("/v1/sessions/{session}/messages"))
        .bearer(&caller.token)
        .json(&flyco_core::SendMessage {
            text: "are you still there?".to_owned(),
        })
        .send()
        .await;
    response.assert_status(202);

    let jobs = queued(&backend);
    assert!(
        jobs.iter()
            .any(|job| matches!(job, ProvisioningJob::Provision { .. })),
        "a machine that is gone is provisioned around, not started: {jobs:?}"
    );
    assert!(
        !jobs
            .iter()
            .any(|job| matches!(job, ProvisioningJob::Recover { .. })),
        "there is nothing to recover: {jobs:?}"
    );

    let mut host = RecordedHost::healthy();
    run_queue_watching(&db, &kv, &queue, &rooms, &mut host, drain(&queue).await).await;
    assert_eq!(
        host.provisions, 1,
        "the fresh machine is built on the freed row"
    );
}

#[skyzen::test]
async fn a_resume_starts_the_machine_the_provider_still_holds(ctx: TestContext, kv: Kv, db: Db) {
    let backend = InMemoryQueue::new();
    let queue = Queue::new(backend.clone());
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let (session, _daemon) = provisioned(&client, &caller, &db, &kv, &queue).await;
    let rooms = test_rooms();
    sessions::daemon_arrived(&db, &rooms, session)
        .await
        .expect("the daemon arrived");
    suspended(&db, session).await;

    let response = client
        .post(&format!("/v1/sessions/{session}/resume"))
        .bearer(&caller.token)
        .send()
        .await;
    response.assert_status(200);

    let jobs = queued(&backend);
    let recovery = jobs
        .iter()
        .find(|job| matches!(job, ProvisioningJob::Recover { .. }))
        .expect("a suspended machine resumes into its own start");
    let ProvisioningJob::Recover { cause, .. } = recovery else {
        unreachable!()
    };
    assert_eq!(*cause, provisioning_queue::RecoveryCause::Resumed);

    let mut host = RecordedHost::healthy();
    run_queue_watching(&db, &kv, &queue, &rooms, &mut host, batch(recovery.clone())).await;
    assert_eq!(host.restarts.len(), 1);
    assert_eq!(host.provisions, 0);
}

#[skyzen::test]
async fn a_resume_provisions_around_a_machine_that_is_gone(ctx: TestContext, kv: Kv, db: Db) {
    let backend = InMemoryQueue::new();
    let queue = Queue::new(backend.clone());
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let (session, _daemon) = provisioned(&client, &caller, &db, &kv, &queue).await;
    let rooms = test_rooms();
    sessions::daemon_arrived(&db, &rooms, session)
        .await
        .expect("the daemon arrived");
    machine_lost(&db, session).await;

    let response = client
        .post(&format!("/v1/sessions/{session}/resume"))
        .bearer(&caller.token)
        .send()
        .await;
    response.assert_status(200);

    let jobs = queued(&backend);
    assert!(
        jobs.iter()
            .any(|job| matches!(job, ProvisioningJob::Provision { .. })),
        "a resume with no machine to start provisions: {jobs:?}"
    );

    let mut host = RecordedHost::healthy();
    run_queue_watching(&db, &kv, &queue, &rooms, &mut host, drain(&queue).await).await;
    assert_eq!(host.provisions, 1);
}

#[skyzen::test]
async fn a_recovery_that_finds_the_machine_gone_provisions_a_fresh_one(
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
    let rooms = test_rooms();
    run_queue_watching(&db, &kv, &queue, &rooms, &mut host, drain(&queue).await).await;
    let daemon = pair(&client, &caller, session).await;
    report_reclaim(&client, &db, session, &daemon, 30).await;
    let recovery = queued_recovery(&backend);

    // Between the reclaim and its recovery the provider deleted the
    // machine — a codespace removed on github.com, a container pruned on
    // the host. `Gone` is the answer a start gets for that.
    host.answer = Answer::Gone;
    let disposition =
        run_queue_watching(&db, &kv, &queue, &rooms, &mut host, batch(recovery)).await;
    assert!(matches!(
        disposition,
        QueueBatchDisposition::PerMessage(ref decisions)
            if decisions == &[QueueMessageDisposition::Ack]
    ));

    let row = machines::for_session(&db, session)
        .await
        .expect("read the machine row")
        .expect("the session still has a machine");
    assert_eq!(row.native_id, None, "the dead name is cleared");
    assert_eq!(
        row.state,
        MachineState::Provisioning,
        "the same row is provisioned again, not replaced"
    );
    assert_eq!(
        host.restarts.len(),
        0,
        "the start that answered `gone` is the last thing tried on that name"
    );
    let jobs = queued(&backend);
    assert!(
        jobs.iter().any(
            |job| matches!(job, ProvisioningJob::Provision { session: s, .. } if *s == session)
        ),
        "the recovery queues the provision it could not do itself: {jobs:?}"
    );

    host.answer = Answer::Takes;
    run_queue_watching(&db, &kv, &queue, &rooms, &mut host, drain(&queue).await).await;
    assert_eq!(
        host.provisions, 2,
        "the fresh machine is built on the freed row — the same host's second provision, \
         after the one the session opened with"
    );
}

#[skyzen::test]
async fn a_recovery_for_a_machine_already_released_does_not_ask_the_provider(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let backend = InMemoryQueue::new();
    let queue = Queue::new(backend.clone());
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let (session, _daemon) = provisioned(&client, &caller, &db, &kv, &queue).await;
    let rooms = test_rooms();
    sessions::daemon_arrived(&db, &rooms, session)
        .await
        .expect("the daemon arrived");
    suspended(&db, session).await;
    machine_lost(&db, session).await;

    // A recovery that was already in the queue when the sweep released the
    // row arrives to find no native name on it: there is nothing to start,
    // and asking the provider would 404. The job's answer is to leave the
    // session's own wake — a message, a resume — to provision.
    let row = machines::for_session(&db, session)
        .await
        .expect("read the machine row")
        .expect("the session still has a machine");
    let mut host = RecordedHost::healthy();
    let disposition = run_queue_watching(
        &db,
        &kv,
        &queue,
        &rooms,
        &mut host,
        batch(ProvisioningJob::resuming(
            session,
            row.id,
            crate::clock::now_unix(),
        )),
    )
    .await;
    assert!(matches!(
        disposition,
        QueueBatchDisposition::PerMessage(ref decisions)
            if decisions == &[QueueMessageDisposition::Ack]
    ));
    assert_eq!(host.restarts.len(), 0, "no name, no start");
    assert_eq!(
        host.provisions, 0,
        "the job does not provision over a wake's job"
    );

    let detail = read(&client, &caller, session).await;
    assert_eq!(
        detail.summary.state,
        SessionState::Interrupted,
        "the session stays interrupted until somebody speaks to it"
    );
}
