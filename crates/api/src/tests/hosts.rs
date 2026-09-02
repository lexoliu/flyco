//! Enrolling a machine the user owns, and running a session on it.
//!
//! The whole of the control plane's half of docs/host-enrollment.md: the
//! one-line command, the token that is spent exactly once, the account the
//! machine becomes, the container job a session turns into, and the answer
//! that completes the machine row. What the room does with a job in between
//! is [`crate::tests::host_room`].

use flyco_core::host::{HOST_TOKEN_PREFIX, JobOutcome};
use flyco_core::{
    CreateSession, EnrollHost, EnrolledHost, Enrollment, EnrollmentToken, HarnessKind, HostFacts,
    HostId, HostState, HostView, MachineCatalogEntry, MachineState, Problem, ProviderAccountView,
    ReportJobResult, SessionDetail, SessionId, SessionState, UpdateHost, Usd, UserId,
};
use flyco_provider::host::{container_name, volume_name};
use flyco_provider::{ClaudeCredential, DaemonBootstrap, HarnessCredential};
use skyzen::routing::Router;
use skyzen::sql;
use skyzen_services::queue::{QueueBatch, QueueMessage, ReceiveOptions};
use skyzen_services::{Db, Kv, Queue};
use skyzen_test::{TestClient, TestContext};

use crate::provisioning::CloudProvisioner;
use crate::provisioning_queue::{self, ProvisioningJob};
use crate::rooms::HostRooms;
use crate::testing::{
    SSH_HOST, TestGithub, host_facts, machine_choice, migrated_router, migrated_router_on,
    seed_harness_account, seed_host_account, seed_user, test_config, test_host_rooms, test_rooms,
    test_vendors,
};
use crate::{machines, session, sessions};

const REPO: &str = "lexoliu/flyco";

/// A bootstrap for a container job, for the tests that only need a job to
/// look at.
pub fn bootstrap() -> DaemonBootstrap {
    DaemonBootstrap {
        session: SessionId::generate(),
        provider: flyco_core::CloudProviderKind::Host,
        control_plane_url: "https://flyco.test/".to_owned(),
        daemon_token: "fd_token".to_owned(),
        permission_mode: flyco_core::PermissionMode::Auto,
        auth: HarnessCredential::ClaudeCode(ClaudeCredential::Inherit),
        repo: flyco_provider::testing::checkout(),
        machine_origin: flyco_core::MachineOrigin::Auto,
        machine: flyco_provider::testing::session_machine(),
        resume_session_id: None,
        mcp_servers: Vec::new(),
    }
}

// ── Enrollment ──

async fn mint(client: &TestClient<Router>, token: &str) -> EnrollmentToken {
    let response = client
        .post("/v1/hosts/enrollment-tokens")
        .bearer(token)
        .send()
        .await;
    response.assert_status(201);
    response.json()
}

async fn enroll(client: &TestClient<Router>, token: &str) -> EnrolledHost {
    let response = client
        .post("/v1/hosts/enroll")
        .json(&EnrollHost {
            token: token.to_owned(),
            facts: host_facts(),
        })
        .send()
        .await;
    response.assert_status(201);
    response.json()
}

#[skyzen::test]
async fn a_minted_token_carries_the_command_that_spends_it(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let session = session::issue(&kv, user.id).await.expect("issue a session");

    let minted = mint(&client, &session).await;

    assert!(minted.token.starts_with(HOST_TOKEN_PREFIX));
    assert!(
        minted.expires_at_unix > crate::clock::now_unix(),
        "a token nobody could spend is not a token"
    );
    // The command names *this* deployment, which a wizard assembling it in
    // the browser could not know, and it carries the one token it spends.
    assert!(
        minted
            .command
            .contains("https://flyco.test/install/flycod.sh"),
        "{}",
        minted.command
    );
    assert!(minted.command.contains(&minted.token));
    assert!(minted.command.contains("host enroll"));

    // Only the hash reaches D1.
    let stored: String = sql!(
        db,
        "SELECT token_hash FROM host_enrollment_tokens WHERE id = {minted.id}"
    )
    .fetch_scalar()
    .await
    .expect("read the stored token");
    assert_eq!(stored, crate::crypto::token_hash(&minted.token));
    assert!(!stored.contains(&minted.token));
}

#[skyzen::test]
async fn a_token_is_pending_until_a_machine_spends_it(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let session = session::issue(&kv, user.id).await.expect("issue a session");
    let minted = mint(&client, &session).await;
    let path = format!("/v1/hosts/enrollment-tokens/{}", minted.id);

    let waiting = client.get(&path).bearer(&session).send().await;
    waiting.assert_status(200);
    assert_eq!(waiting.json::<Enrollment>(), Enrollment::Pending);

    let enrolled = enroll(&client, &minted.token).await;
    assert!(enrolled.host_token.starts_with(HOST_TOKEN_PREFIX));

    let arrived = client.get(&path).bearer(&session).send().await;
    arrived.assert_status(200);
    let Enrollment::Enrolled { host } = arrived.json::<Enrollment>() else {
        panic!("the wizard flips to the machine that arrived");
    };
    assert_eq!(host.id, enrolled.host_id);
    assert_eq!(host.facts, host_facts());
    assert_eq!(host.label, SSH_HOST, "a machine opens named after itself");
}

#[skyzen::test]
async fn a_token_is_spent_exactly_once(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let session = session::issue(&kv, user.id).await.expect("issue a session");
    let minted = mint(&client, &session).await;

    enroll(&client, &minted.token).await;

    let replayed = client
        .post("/v1/hosts/enroll")
        .json(&EnrollHost {
            token: minted.token.clone(),
            facts: host_facts(),
        })
        .send()
        .await;
    replayed.assert_status(410);
    assert_eq!(
        replayed.json::<Problem>().kind,
        "https://flyco.dev/problems/enrollment-token-expired"
    );

    // And one machine came of it, not two.
    let hosts = client.get("/v1/hosts").bearer(&session).send().await;
    assert_eq!(hosts.json::<Vec<HostView>>().len(), 1);
}

#[skyzen::test]
async fn an_expired_or_unknown_token_enrolls_nothing(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let session = session::issue(&kv, user.id).await.expect("issue a session");
    let minted = mint(&client, &session).await;

    // Ten minutes on, to the second the token stops being accepted.
    sql!(
        db,
        "UPDATE host_enrollment_tokens SET expires_at_unix = 1 WHERE id = {minted.id}"
    )
    .execute()
    .await
    .expect("expire the token");

    for token in [
        minted.token.as_str(),
        "fh_never-minted",
        "not-even-a-prefix",
    ] {
        let response = client
            .post("/v1/hosts/enroll")
            .json(&EnrollHost {
                token: token.to_owned(),
                facts: host_facts(),
            })
            .send()
            .await;
        response.assert_status(410);
    }

    assert!(
        client
            .get("/v1/hosts")
            .bearer(&session)
            .send()
            .await
            .json::<Vec<HostView>>()
            .is_empty()
    );
}

#[skyzen::test]
async fn an_enrolled_machine_is_a_provider_account_offering_itself(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let session = session::issue(&kv, user.id).await.expect("issue a session");
    let minted = mint(&client, &session).await;
    let enrolled = enroll(&client, &minted.token).await;

    // The compute chip, the catalog, session creation and the usage panel
    // all speak `provider_accounts`; a machine somebody owns needs no
    // special case in any of them.
    let accounts = client.get("/v1/providers").bearer(&session).send().await;
    let accounts: Vec<ProviderAccountView> = accounts.json();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].kind, flyco_core::CloudProviderKind::Host);
    assert_eq!(accounts[0].label, SSH_HOST);

    // Offline until it connects, and therefore offering nothing yet: a
    // machine flyco cannot reach is not one to put on the slider.
    let catalog = client
        .get("/v1/machines/catalog")
        .bearer(&session)
        .send()
        .await;
    assert!(catalog.json::<Vec<MachineCatalogEntry>>().is_empty());

    // The relay upgrade is where the control plane sees it arrive.
    connect(&client, enrolled.host_id, &enrolled.host_token).await;
    assert_eq!(state_of(&db, enrolled.host_id).await, HostState::Online);

    let catalog = client
        .get("/v1/machines/catalog")
        .bearer(&session)
        .send()
        .await;
    let catalog: Vec<MachineCatalogEntry> = catalog.json();
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].machine_type, SSH_HOST);
    assert_eq!(catalog[0].account, Some(accounts[0].id));
    assert_eq!(catalog[0].pricing, flyco_core::MachinePricing::UserOwned);
    assert_eq!(catalog[0].capacity, Some(host_facts().capacity()));
    assert_eq!(catalog[0].lineage, Some(host_facts().lineage()));
}

/// Opens the machine's relay, which natively is refused *after* it has been
/// authenticated — which is exactly the half this asserts.
async fn connect(client: &TestClient<Router>, host: HostId, token: &str) {
    let response = client
        .get(&format!("/v1/hosts/{host}/relay"))
        .bearer(token)
        .send()
        .await;
    assert_eq!(
        response.status(),
        501,
        "a native control plane authenticates the upgrade and then refuses the socket"
    );
}

async fn state_of(db: &Db, host: HostId) -> HostState {
    sql!(db, "SELECT state FROM hosts WHERE id = {host}")
        .fetch_scalar()
        .await
        .expect("read the host state")
}

#[skyzen::test]
async fn a_machine_that_presents_the_wrong_token_reaches_no_room(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let session = session::issue(&kv, user.id).await.expect("issue a session");
    let minted = mint(&client, &session).await;
    let enrolled = enroll(&client, &minted.token).await;

    for presented in ["fh_another-machines-token", "fd_a-daemon-token", ""] {
        let response = client
            .get(&format!("/v1/hosts/{}/relay", enrolled.host_id))
            .bearer(presented)
            .send()
            .await;
        assert!(
            response.status() == 401,
            "presenting `{presented}` opened a socket"
        );
    }
    assert_eq!(state_of(&db, enrolled.host_id).await, HostState::Offline);
}

#[skyzen::test]
async fn rotating_a_token_revokes_the_one_the_machine_holds(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let session = session::issue(&kv, user.id).await.expect("issue a session");
    let minted = mint(&client, &session).await;
    let enrolled = enroll(&client, &minted.token).await;

    let rotated = client
        .post(&format!("/v1/hosts/{}/token/rotate", enrolled.host_id))
        .bearer(&session)
        .send()
        .await;
    rotated.assert_status(200);
    let rotated: EnrolledHost = rotated.json();
    assert_ne!(rotated.host_token, enrolled.host_token);

    assert!(
        !crate::hosts::authenticates(&db, enrolled.host_id, &enrolled.host_token)
            .await
            .expect("check the old token")
    );
    assert!(
        crate::hosts::authenticates(&db, enrolled.host_id, &rotated.host_token)
            .await
            .expect("check the new token")
    );
}

#[skyzen::test]
async fn a_machine_can_be_renamed_and_is_only_ever_the_callers(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let owner = seed_user(&db).await;
    let stranger = crate::testing::seed_other_user(&db).await;
    let session = session::issue(&kv, owner.id)
        .await
        .expect("issue a session");
    let intruder = session::issue(&kv, stranger.id)
        .await
        .expect("issue a session");
    let minted = mint(&client, &session).await;
    let enrolled = enroll(&client, &minted.token).await;
    let path = format!("/v1/hosts/{}", enrolled.host_id);

    let renamed = client
        .patch(&path)
        .bearer(&session)
        .json(&UpdateHost {
            label: "the machine under the desk".to_owned(),
        })
        .send()
        .await;
    renamed.assert_status(200);
    assert_eq!(
        renamed.json::<HostView>().label,
        "the machine under the desk"
    );

    let empty = client
        .patch(&path)
        .bearer(&session)
        .json(&UpdateHost {
            label: "   ".to_owned(),
        })
        .send()
        .await;
    empty.assert_status(422);

    // Somebody else's machine is indistinguishable from one that is not
    // there.
    let foreign = client.get(&path).bearer(&intruder).send().await;
    foreign.assert_status(404);
    assert_eq!(
        foreign.json::<Problem>().kind,
        "https://flyco.dev/problems/host-not-found"
    );
}

// ── Running a session on it ──

/// A signed-in caller with an enrolled machine to provision onto.
struct Caller {
    user: UserId,
    token: String,
    host: HostId,
    account: flyco_core::ProviderAccountId,
}

async fn sign_in(kv: &Kv, db: &Db) -> Caller {
    let user = seed_user(db).await;
    let token = session::issue(kv, user.id).await.expect("issue a session");
    let (host, account) = seed_host_account(db, user.id).await;
    seed_harness_account(db, user.id, HarnessKind::ClaudeCode).await;
    Caller {
        user: user.id,
        token,
        host,
        account,
    }
}

async fn open(client: &TestClient<Router>, caller: &Caller) -> SessionDetail {
    let response = client
        .post("/v1/sessions")
        .bearer(&caller.token)
        .json(&CreateSession {
            prompt: "audit the relay for dropped frames".to_owned(),
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

/// Runs whatever the queue is holding through the real consumer, with the
/// real provisioner over the host rooms the caller keeps.
async fn run_queue(db: &Db, queue: &Queue, hosts: &HostRooms) {
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

    let github = TestGithub::default();
    let vendors = test_vendors();
    let mut provisioner = CloudProvisioner::new(hosts.clone());
    provisioning_queue::consume(
        db,
        &test_config(),
        queue,
        &test_rooms(),
        &mut provisioning_queue::Clients {
            provisioner: &mut provisioner,
            vendors: &vendors,
            github: &github,
        },
        QueueBatch {
            queue: "provisioning".to_owned(),
            messages,
        },
    )
    .await;
}

#[skyzen::test]
async fn a_session_on_a_machine_you_own_becomes_a_container_job(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let hosts = test_host_rooms();
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);

    let session = open(&client, &caller).await.summary.id;
    run_queue(&db, &queue, &hosts).await;

    // The job is in the machine's room, durably, whether or not the socket
    // happened to be up at that instant.
    let status = hosts.status(caller.host).await.expect("read the host room");
    assert_eq!(status.pending_jobs, 1);

    let machine = machines::for_session(&db, session)
        .await
        .expect("read the machine row")
        .expect("the session reserved a machine row when it was created");
    assert_eq!(machine.state, MachineState::Running);
    assert_eq!(
        machine.native_id.as_deref(),
        Some(&*container_name(machine.id)),
        "the row names the container the job named"
    );
    assert_eq!(
        machine.volume_name, None,
        "the volume is what the machine reports; nothing has answered yet"
    );

    // The machine exists as far as the control plane is concerned; the agent
    // does not answer yet, so the session is still provisioning.
    assert_eq!(
        sessions::find(&db, caller.user, session)
            .await
            .expect("read the session")
            .summary
            .state,
        SessionState::Provisioning
    );
}

#[skyzen::test]
async fn a_running_container_completes_the_machine_row(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let hosts = test_host_rooms();
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let session = open(&client, &caller).await.summary.id;
    run_queue(&db, &queue, &hosts).await;
    let machine = machines::for_session(&db, session)
        .await
        .expect("read the machine row")
        .expect("a machine row");

    let reported = client
        .post(&format!("/v1/hosts/{}/job-results", caller.host))
        .bearer(crate::testing::HOST_TOKEN)
        .json(&ReportJobResult {
            job_id: machine.id,
            outcome: JobOutcome::Running {
                container: container_name(machine.id),
                volume: volume_name(machine.id),
            },
        })
        .send()
        .await;
    reported.assert_status(204);

    let completed = machines::for_session(&db, session)
        .await
        .expect("read the machine row")
        .expect("a machine row");
    assert_eq!(completed.state, MachineState::Running);
    assert_eq!(
        completed.native_id.as_deref(),
        Some(&*container_name(machine.id))
    );
    assert_eq!(
        completed.volume_name.as_deref(),
        Some(&*volume_name(machine.id)),
        "the volume the session's work lives on is what a later removal has to name"
    );
}

#[skyzen::test]
async fn a_container_that_would_not_start_fails_the_session(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let hosts = test_host_rooms();
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let session = open(&client, &caller).await.summary.id;
    run_queue(&db, &queue, &hosts).await;
    let machine = machines::for_session(&db, session)
        .await
        .expect("read the machine row")
        .expect("a machine row");

    client
        .post(&format!("/v1/hosts/{}/job-results", caller.host))
        .bearer(crate::testing::HOST_TOKEN)
        .json(&ReportJobResult {
            job_id: machine.id,
            outcome: JobOutcome::Failed {
                message: "podman: no space left on device".to_owned(),
            },
        })
        .send()
        .await
        .assert_status(204);

    let failed = sessions::find(&db, caller.user, session)
        .await
        .expect("read the session");
    assert_eq!(
        failed.summary.state,
        SessionState::Failed,
        "a session whose container never came up must not sit in `provisioning`"
    );
    assert!(
        failed
            .failure
            .expect("a failed session says why")
            .contains("no space left"),
        "the reason is the machine's own"
    );
}

#[skyzen::test]
async fn a_job_result_for_another_machine_is_refused(ctx: TestContext, _kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let (host, _) = seed_host_account(&db, user.id).await;

    let response = client
        .post(&format!("/v1/hosts/{host}/job-results"))
        .bearer(crate::testing::HOST_TOKEN)
        .json(&ReportJobResult {
            job_id: flyco_core::MachineId::generate(),
            outcome: JobOutcome::Done,
        })
        .send()
        .await;

    response.assert_status(404);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/machine-not-found"
    );
}

#[skyzen::test]
async fn a_job_result_needs_the_machines_own_token(ctx: TestContext, _kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let (host, _) = seed_host_account(&db, user.id).await;

    let response = client
        .post(&format!("/v1/hosts/{host}/job-results"))
        .bearer("fh_not-this-machine")
        .json(&ReportJobResult {
            job_id: flyco_core::MachineId::generate(),
            outcome: JobOutcome::Done,
        })
        .send()
        .await;

    response.assert_status(401);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/invalid-host-credential"
    );
}

// ── Removing it ──

#[skyzen::test]
async fn removing_a_machine_with_a_session_on_it_is_refused_until_forced(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let hosts = test_host_rooms();
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let session = open(&client, &caller).await.summary.id;
    run_queue(&db, &queue, &hosts).await;
    let path = format!("/v1/hosts/{}", caller.host);

    let refused = client.delete(&path).bearer(&caller.token).send().await;
    refused.assert_status(409);
    let problem = refused.json::<Problem>();
    assert_eq!(
        problem.kind,
        "https://flyco.dev/problems/host-has-active-sessions"
    );
    assert!(
        problem.detail.contains('1'),
        "the refusal counts what is still running: {}",
        problem.detail
    );
    assert_eq!(state_of(&db, caller.host).await, HostState::Online);

    // Forced: the containers are stopped, their volumes kept, and the token
    // is gone.
    client
        .delete(&format!("{path}?force=true"))
        .bearer(&caller.token)
        .send()
        .await
        .assert_status(204);

    assert_eq!(state_of(&db, caller.host).await, HostState::Removed);
    assert!(
        !crate::hosts::authenticates(&db, caller.host, crate::testing::HOST_TOKEN)
            .await
            .expect("check the revoked token"),
        "a removed machine's token opens nothing"
    );

    let machine = machines::for_session(&db, session)
        .await
        .expect("read the machine row")
        .expect("a machine row");
    assert_eq!(
        machine.state,
        MachineState::Deallocated,
        "the container is stopped and its volume kept, which is what deallocated means"
    );

    // It is gone from every list it was in, and offers nothing.
    assert!(
        client
            .get("/v1/hosts")
            .bearer(&caller.token)
            .send()
            .await
            .json::<Vec<HostView>>()
            .is_empty()
    );
    assert!(
        client
            .get("/v1/providers")
            .bearer(&caller.token)
            .send()
            .await
            .json::<Vec<ProviderAccountView>>()
            .is_empty()
    );
    assert!(
        client
            .get("/v1/machines/catalog")
            .bearer(&caller.token)
            .send()
            .await
            .json::<Vec<MachineCatalogEntry>>()
            .is_empty()
    );
}

#[skyzen::test]
async fn removing_an_idle_machine_needs_no_force(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    let (host, _) = seed_host_account(&db, user.id).await;

    client
        .delete(&format!("/v1/hosts/{host}"))
        .bearer(&token)
        .send()
        .await
        .assert_status(204);
    assert_eq!(state_of(&db, host).await, HostState::Removed);

    // And a removed machine is never handed a live credential again.
    let rotated = client
        .post(&format!("/v1/hosts/{host}/token/rotate"))
        .bearer(&token)
        .send()
        .await;
    rotated.assert_status(409);
    assert_eq!(
        rotated.json::<Problem>().kind,
        "https://flyco.dev/problems/host-removed"
    );
}

#[skyzen::test]
async fn a_machine_that_is_not_connected_refuses_the_operations_a_user_watches(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    queue: Queue,
) {
    let hosts = test_host_rooms();
    let router = migrated_router_on(&db, queue.clone()).await;
    let caller = sign_in(&kv, &db).await;
    let client = ctx.client(router);
    let session = open(&client, &caller).await.summary.id;
    run_queue(&db, &queue, &hosts).await;

    // Nothing holds a socket in a native control plane, so the room reports
    // exactly what a machine that was unplugged reports.
    let response = client
        .post(&format!("/v1/sessions/{session}/machine/stop"))
        .bearer(&caller.token)
        .send()
        .await;

    response.assert_status(409);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/host-offline"
    );
}

#[skyzen::test]
async fn the_facts_a_machine_reports_are_what_its_view_carries(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    let minted = mint(&client, &token).await;

    let grown = HostFacts {
        memory_mib: 64 * 1024,
        disk_free_gib: 120,
        ..host_facts()
    };
    let response = client
        .post("/v1/hosts/enroll")
        .json(&EnrollHost {
            token: minted.token,
            facts: grown.clone(),
        })
        .send()
        .await;
    response.assert_status(201);
    let host = response.json::<EnrolledHost>().host_id;

    let view = client
        .get(&format!("/v1/hosts/{host}"))
        .bearer(&token)
        .send()
        .await;
    view.assert_status(200);
    assert_eq!(view.json::<HostView>().facts, grown);
}
