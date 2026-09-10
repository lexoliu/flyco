//! A session that runs out of harness plan stops itself and comes back on
//! its own (issue #244).
//!
//! What is worth pinning here is the *sequence*, because every step of it is
//! decided from one session row and a clock: the report writes the wait, the
//! minute sweep releases the machine, wakes it ten minutes before the reset,
//! and says the next thing when the window has turned over. Each step has to
//! happen once however many times the cron runs, and the two shapes of pause
//! — machine released, machine kept — have to reach the same end.
//!
//! The mechanism is driven directly rather than through the router, because
//! the room a route handler broadcasts into belongs to that router and
//! nothing outside it can read the stream back.

use flyco_core::{
    ClientEvent, HarnessEvent, MachineState, MessageOrigin, PausedReason, Problem, SessionDetail,
    SessionId, SessionState, USAGE_LIMIT_CONTINUE_MESSAGE, USAGE_LIMIT_WAKE_LEAD_SECS, UsageWindow,
};
use skyzen::sql;
use skyzen_services::{Db, Kv, Queue};
use skyzen_test::{TestContext, mock::InMemoryQueue};

use crate::provisioning_queue::{ProvisioningJob, RecoveryCause};
use crate::rooms::Rooms;
use crate::testing::{
    machine_choice, migrate, migrated_router, seed_provider_account, seed_session, seed_user,
    test_config, test_host_rooms, test_rooms,
};
use crate::{daemon_tokens, machines, session, sessions, usage_limits};

/// A fixed instant every test reasons from, so nothing depends on when the
/// suite runs.
const NOW: u64 = 1_800_000_000;

/// A weekly window, far enough out that the machine is not worth keeping.
const DAYS: u64 = 24 * 60 * 60;

/// The five-hour window, spent, turning over `in_secs` from [`NOW`].
fn five_hour(in_secs: u64) -> UsageWindow {
    UsageWindow::new(
        Some(300),
        None,
        100,
        Some(i64::try_from(NOW + in_secs).expect("a reset time fits")),
    )
}

/// A weekly window, spent, turning over three days from [`NOW`].
fn weekly() -> UsageWindow {
    UsageWindow::new(
        Some(10_080),
        None,
        100,
        Some(i64::try_from(NOW + 3 * DAYS).expect("a reset time fits")),
    )
}

/// Everything one waiting session needs: an owner, a live session, and a
/// machine row the sweep can act on.
struct Waiting {
    user: flyco_core::UserId,
    session: SessionId,
    machine: flyco_core::MachineId,
    rooms: Rooms,
}

/// Opens a live session on a reserved machine.
///
/// The session is taken live because that is the only state a usage limit is
/// interesting from: the daemon that reports one is a daemon that reached
/// the control plane.
async fn live(db: &Db) -> Waiting {
    migrate(db).await;
    let user = seed_user(db).await;
    let session = seed_session(db, &user).await;
    let account = seed_provider_account(db, user.id).await;
    let choice = machine_choice(account);
    let spec = flyco_core::MachineSpec {
        provider: flyco_core::CloudProviderKind::Host,
        machine_type: choice.machine_type.clone(),
        runtime: choice.runtime,
        region: choice.region.clone(),
        spot: choice.spot,
        disk_gib: choice.disk_gib,
    };
    let machine = machines::reserve(db, session, account, &spec)
        .await
        .expect("reserve a machine");
    let rooms = test_rooms();
    sessions::daemon_arrived(db, &rooms, session)
        .await
        .expect("the session's daemon reached the control plane");
    Waiting {
        user: user.id,
        session,
        machine,
        rooms,
    }
}

/// Marks the machine as the provider actually built it, which is what a
/// lifecycle operation needs a row to name.
async fn built(db: &Db, machine: flyco_core::MachineId, state: MachineState) {
    let native = format!("flyco-{machine}");
    sql!(
        db,
        "UPDATE machines SET state = {state}, native_id = {native} WHERE id = {machine}"
    )
    .execute()
    .await
    .expect("record what the provider built");
}

async fn detail(db: &Db, waiting: &Waiting) -> SessionDetail {
    sessions::find(db, waiting.user, waiting.session)
        .await
        .expect("read the session")
}

/// Every `ClientEvent` the room recorded, in order.
async fn recorded(rooms: &Rooms, session: SessionId) -> Vec<ClientEvent> {
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

/// Every provisioning job the queue holds, delivered or not.
fn queued(backend: &InMemoryQueue) -> Vec<ProvisioningJob> {
    backend
        .messages()
        .iter()
        .map(|body| serde_json::from_slice(body).expect("a queued provisioning job"))
        .collect()
}

#[skyzen::test]
async fn a_limit_whose_reset_is_days_away_pauses_the_session_and_books_its_return(
    _ctx: TestContext,
    db: Db,
) {
    let waiting = live(&db).await;

    usage_limits::pause(
        &db,
        &test_config(),
        &waiting.rooms,
        waiting.session,
        &weekly(),
        NOW,
    )
    .await
    .expect("the limit is recorded");

    let session = detail(&db, &waiting).await;
    assert_eq!(session.summary.state, SessionState::Paused);
    assert_eq!(
        session.summary.paused_reason,
        Some(PausedReason::UsageLimit),
        "a plan window is not the same pause as a spent budget"
    );
    let pause = session.usage_limit.expect("the wait is on the session");
    assert_eq!(pause.window, "Weekly");
    assert_eq!(pause.resets_at_unix, NOW + 3 * DAYS);
    assert_eq!(
        pause.resume_at_unix,
        Some(NOW + 3 * DAYS - USAGE_LIMIT_WAKE_LEAD_SECS),
        "the machine comes back ten minutes before the window turns over"
    );
    assert!(pause.machine_stopped());

    // The conversation says where the limit struck, and the page is told to
    // re-read the session.
    let events = recorded(&waiting.rooms, waiting.session).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            ClientEvent::Harness {
                event: HarnessEvent::UsageLimited { window }
            } if window.label == "Weekly"
        )),
        "{events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            ClientEvent::SessionStateChanged {
                state: SessionState::Paused
            }
        )),
        "{events:?}"
    );
}

#[skyzen::test]
async fn a_limit_whose_reset_is_close_keeps_the_machine(_ctx: TestContext, db: Db) {
    let waiting = live(&db).await;

    usage_limits::pause(
        &db,
        &test_config(),
        &waiting.rooms,
        waiting.session,
        &five_hour(12 * 60),
        NOW,
    )
    .await
    .expect("the limit is recorded");

    let pause = detail(&db, &waiting)
        .await
        .usage_limit
        .expect("the wait is on the session");
    assert_eq!(pause.window, "5-hour");
    assert!(
        !pause.machine_stopped(),
        "stopping and starting a machine for twelve minutes buys nothing"
    );
    assert!(pause.resume_at_unix.is_none());
}

/// Both harnesses can name one limit twice — the refused turn, and the
/// snapshot that explains it — and the user wants one pause and one
/// notification.
#[skyzen::test]
async fn the_same_limit_reported_twice_pauses_once(_ctx: TestContext, db: Db) {
    let waiting = live(&db).await;
    let config = test_config();

    usage_limits::pause(
        &db,
        &config,
        &waiting.rooms,
        waiting.session,
        &weekly(),
        NOW,
    )
    .await
    .expect("the limit is recorded");
    let after_first = recorded(&waiting.rooms, waiting.session).await.len();

    usage_limits::pause(
        &db,
        &config,
        &waiting.rooms,
        waiting.session,
        &five_hour(4 * 60 * 60),
        NOW,
    )
    .await
    .expect("a second report of a waiting session is accepted and ignored");

    assert_eq!(
        recorded(&waiting.rooms, waiting.session).await.len(),
        after_first,
        "a session already waiting says nothing more about it"
    );
    let pause = detail(&db, &waiting)
        .await
        .usage_limit
        .expect("still waiting");
    assert_eq!(
        pause.window, "Weekly",
        "the wait the session is already in is the one it stays in"
    );
}

/// A limit flyco cannot place in time is nothing it can schedule around: the
/// machine would be stopped until an instant nobody named.
#[skyzen::test]
async fn a_limit_that_names_no_reset_cannot_pause_a_session(_ctx: TestContext, db: Db) {
    let waiting = live(&db).await;

    let refused = usage_limits::pause(
        &db,
        &test_config(),
        &waiting.rooms,
        waiting.session,
        &UsageWindow::new(Some(300), None, 100, None),
        NOW,
    )
    .await
    .expect_err("a wait with no end is refused");
    assert_eq!(
        refused.problem().kind,
        "https://flyco.dev/problems/usage-limit-without-reset"
    );

    let session = detail(&db, &waiting).await;
    assert_eq!(session.summary.state, SessionState::Active);
    assert!(session.usage_limit.is_none());
}

#[skyzen::test]
async fn the_sweep_starts_the_machine_before_the_reset_exactly_once(_ctx: TestContext, db: Db) {
    let waiting = live(&db).await;
    built(&db, waiting.machine, MachineState::Deallocated).await;
    let backend = InMemoryQueue::new();
    let queue = Queue::new(backend.clone());
    let hosts = test_host_rooms();
    let config = test_config();

    usage_limits::pause(
        &db,
        &config,
        &waiting.rooms,
        waiting.session,
        &weekly(),
        NOW,
    )
    .await
    .expect("the limit is recorded");
    let resume_at = NOW + 3 * DAYS - USAGE_LIMIT_WAKE_LEAD_SECS;

    // A minute before the wake is due: nothing is started.
    usage_limits::sweep(&db, &config, &waiting.rooms, &hosts, &queue, resume_at - 60)
        .await
        .expect("the sweep runs");
    assert!(queued(&backend).is_empty(), "the wake is not due yet");
    assert_eq!(
        detail(&db, &waiting).await.summary.state,
        SessionState::Paused
    );

    // And at the wake, twice, because Cloudflare overlaps and retries crons.
    for _ in 0..2 {
        usage_limits::sweep(&db, &config, &waiting.rooms, &hosts, &queue, resume_at)
            .await
            .expect("the sweep runs");
    }

    let jobs = queued(&backend);
    assert_eq!(jobs.len(), 1, "one pause is one wake: {jobs:?}");
    assert!(
        matches!(
            jobs.first(),
            Some(ProvisioningJob::Recover {
                machine,
                cause: RecoveryCause::UsageLimit,
                ..
            }) if *machine == waiting.machine
        ),
        "{jobs:?}"
    );

    // The session is on its way back, and still says why.
    let session = detail(&db, &waiting).await;
    assert_eq!(session.summary.state, SessionState::Provisioning);
    assert_eq!(
        session.summary.paused_reason,
        Some(PausedReason::UsageLimit),
        "the reason outlives the pause: it is what tells this provisioning \
         apart from a first one"
    );
}

#[skyzen::test]
async fn a_woken_session_is_continued_once_its_daemon_is_back_and_the_window_has_reset(
    _ctx: TestContext,
    db: Db,
) {
    let waiting = live(&db).await;
    built(&db, waiting.machine, MachineState::Deallocated).await;
    let queue = Queue::new(InMemoryQueue::new());
    let hosts = test_host_rooms();
    let config = test_config();
    let resets_at = NOW + 3 * DAYS;

    usage_limits::pause(
        &db,
        &config,
        &waiting.rooms,
        waiting.session,
        &weekly(),
        NOW,
    )
    .await
    .expect("the limit is recorded");
    usage_limits::sweep(
        &db,
        &config,
        &waiting.rooms,
        &hosts,
        &queue,
        resets_at - USAGE_LIMIT_WAKE_LEAD_SECS,
    )
    .await
    .expect("the sweep wakes it");

    // The window has turned over but the machine is still coming up. Nothing
    // is said to a session with no daemon to hear it.
    let before = recorded(&waiting.rooms, waiting.session).await.len();
    usage_limits::sweep(&db, &config, &waiting.rooms, &hosts, &queue, resets_at)
        .await
        .expect("the sweep runs");
    assert_eq!(
        recorded(&waiting.rooms, waiting.session).await.len(),
        before,
        "a session that is still provisioning is waited for, not talked to"
    );
    assert!(detail(&db, &waiting).await.usage_limit.is_some());

    // Its daemon reaches the control plane, and the next sweep continues it.
    sessions::daemon_arrived(&db, &waiting.rooms, waiting.session)
        .await
        .expect("the restarted machine's daemon reported in");
    usage_limits::sweep(&db, &config, &waiting.rooms, &hosts, &queue, resets_at)
        .await
        .expect("the sweep continues it");

    let events = recorded(&waiting.rooms, waiting.session).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            ClientEvent::UserMessage { text, origin }
                if text == USAGE_LIMIT_CONTINUE_MESSAGE && *origin == MessageOrigin::Flyco
        )),
        "flyco says the next thing, and the transcript says it was flyco: {events:?}"
    );

    let session = detail(&db, &waiting).await;
    assert_eq!(session.summary.state, SessionState::Active);
    assert!(session.summary.paused_reason.is_none());
    assert!(session.usage_limit.is_none());

    // And a cron that runs again says nothing a second time.
    let after = recorded(&waiting.rooms, waiting.session).await.len();
    usage_limits::sweep(&db, &config, &waiting.rooms, &hosts, &queue, resets_at)
        .await
        .expect("the sweep runs");
    assert_eq!(recorded(&waiting.rooms, waiting.session).await.len(), after);
}

/// The machine-kept half of the wait reaches the same end, and it does it
/// without ever leaving [`SessionState::Paused`].
#[skyzen::test]
async fn a_session_that_kept_its_machine_is_continued_at_the_reset(_ctx: TestContext, db: Db) {
    let waiting = live(&db).await;
    built(&db, waiting.machine, MachineState::Running).await;
    let backend = InMemoryQueue::new();
    let queue = Queue::new(backend.clone());
    let hosts = test_host_rooms();
    let config = test_config();
    let resets_at = NOW + 12 * 60;

    usage_limits::pause(
        &db,
        &config,
        &waiting.rooms,
        waiting.session,
        &five_hour(12 * 60),
        NOW,
    )
    .await
    .expect("the limit is recorded");

    usage_limits::sweep(&db, &config, &waiting.rooms, &hosts, &queue, resets_at)
        .await
        .expect("the sweep continues it");

    assert!(
        queued(&backend).is_empty(),
        "a machine that never stopped has nothing to wake"
    );
    let events = recorded(&waiting.rooms, waiting.session).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            ClientEvent::UserMessage { text, .. } if text == USAGE_LIMIT_CONTINUE_MESSAGE
        )),
        "{events:?}"
    );
    let session = detail(&db, &waiting).await;
    assert_eq!(session.summary.state, SessionState::Active);
    assert!(session.usage_limit.is_none());
}

/// The composer stays usable through the wait, and what is typed into it is
/// what the session says when the window turns over — not flyco's nudge.
#[skyzen::test]
async fn a_message_typed_while_waiting_is_what_the_session_says_when_it_comes_back(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let waiting = live(&db).await;
    built(&db, waiting.machine, MachineState::Running).await;
    let config = test_config();
    let resets_at = NOW + 12 * 60;
    usage_limits::pause(
        &db,
        &config,
        &waiting.rooms,
        waiting.session,
        &five_hour(12 * 60),
        NOW,
    )
    .await
    .expect("the limit is recorded");

    // Through the route a browser actually calls, which answers `202`
    // rather than refusing a paused session.
    let client = ctx.client(crate::testing::test_router(
        db.clone(),
        Queue::new(InMemoryQueue::new()),
    ));
    let token = session::issue(&kv, waiting.user)
        .await
        .expect("issue a session");
    let typed = "carry on with the migration, and squash the commits";
    client
        .post(&format!("/v1/sessions/{}/messages", waiting.session))
        .bearer(&token)
        .json(&flyco_core::SendMessage {
            text: typed.to_owned(),
        })
        .send()
        .await
        .assert_status(202);

    let pause = detail(&db, &waiting)
        .await
        .usage_limit
        .expect("still waiting");
    assert_eq!(
        pause.queued_message.as_deref(),
        Some(typed),
        "the message is held against the wait, visibly"
    );

    usage_limits::sweep(
        &db,
        &config,
        &waiting.rooms,
        &test_host_rooms(),
        &Queue::new(InMemoryQueue::new()),
        resets_at,
    )
    .await
    .expect("the sweep continues it");

    let events = recorded(&waiting.rooms, waiting.session).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            ClientEvent::UserMessage { text, origin }
                if text == typed && *origin == MessageOrigin::User
        )),
        "what the user typed is theirs, and it is what gets sent: {events:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            ClientEvent::UserMessage { text, .. } if text == USAGE_LIMIT_CONTINUE_MESSAGE
        )),
        "the canned nudge is what flyco says when the user said nothing: {events:?}"
    );
}

/// The release is an invariant the sweep checks, not a step it assumes
/// happened: a machine already off is left alone rather than stopped again.
#[skyzen::test]
async fn the_sweep_leaves_a_machine_that_is_already_off_alone(_ctx: TestContext, db: Db) {
    let waiting = live(&db).await;
    built(&db, waiting.machine, MachineState::Deallocated).await;
    let config = test_config();

    usage_limits::pause(
        &db,
        &config,
        &waiting.rooms,
        waiting.session,
        &weekly(),
        NOW,
    )
    .await
    .expect("the limit is recorded");
    usage_limits::sweep(
        &db,
        &config,
        &waiting.rooms,
        &test_host_rooms(),
        &Queue::new(InMemoryQueue::new()),
        NOW + DAYS,
    )
    .await
    .expect("a machine that is already off is no work at all");

    assert_eq!(
        machines::for_session(&db, waiting.session)
            .await
            .expect("read the machine")
            .expect("the session has one")
            .state,
        MachineState::Deallocated
    );
    assert_eq!(
        detail(&db, &waiting).await.summary.state,
        SessionState::Paused
    );
}

// ── The daemon's own route ──

#[skyzen::test]
async fn the_daemon_route_takes_a_limit_and_refuses_one_with_no_reset(
    ctx: TestContext,
    _kv: Kv,
    db: Db,
) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let session = seed_session(&db, &user).await;
    let token = daemon_tokens::issue(&db, user.id, session)
        .await
        .expect("mint a daemon token")
        .token;

    let refused = client
        .post(&format!("/v1/sessions/{session}/usage-limit"))
        .bearer(&token)
        .json(&flyco_core::UsageLimitHit {
            window: UsageWindow::new(Some(300), None, 100, None),
        })
        .send()
        .await;
    refused.assert_status(422);
    assert_eq!(
        refused.json::<Problem>().kind,
        "https://flyco.dev/problems/usage-limit-without-reset"
    );

    // A provisioning session cannot be paused, so the limit is reported from
    // a live one — which is the only state a refused turn happens in.
    sessions::daemon_arrived(&db, &test_rooms(), session)
        .await
        .expect("the daemon reported in");
    client
        .post(&format!("/v1/sessions/{session}/usage-limit"))
        .bearer(&token)
        .json(&flyco_core::UsageLimitHit { window: weekly() })
        .send()
        .await
        .assert_status(202);

    assert_eq!(
        sessions::find(&db, user.id, session)
            .await
            .expect("read the session")
            .summary
            .paused_reason,
        Some(PausedReason::UsageLimit)
    );
}
