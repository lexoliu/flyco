//! A session that runs out of harness plan stops itself and comes back on
//! its own (issue #244).
//!
//! Every coding-agent plan meters rolling windows — Claude's five-hour and
//! weekly, Codex's primary and secondary — and hitting one is not a failure:
//! the account works again at a stated instant, and until then a session's
//! machine is compute nobody can use. Flyco already *shows* the windows
//! (#234). This is what it does about them.
//!
//! # Four moments
//!
//! 1. **The report.** The session's own daemon is the only thing that sees
//!    the limit — a refused turn, or a window its harness reports spent —
//!    and it posts `POST /v1/sessions/{id}/usage-limit`. [`pause`] writes the
//!    wait, interrupts the turn and tells the user.
//! 2. **The release.** [`sweep`] stops the machine, so the wait costs
//!    nothing. Not done in the report's own request on purpose: a provider
//!    stop is tens of seconds and the caller is a daemon that has just been
//!    refused, so doing it here makes the release survive a request that
//!    dies half way through — and makes it an invariant checked every
//!    minute rather than a step somebody has to have completed.
//! 3. **The wake.** Ten minutes before the reset, [`sweep`] enqueues the
//!    same [`Recover`](ProvisioningJob::Recover) job a spot reclamation
//!    uses, so the machine is warm and the agent is up when the window turns
//!    over.
//! 4. **The continuation.** Once the window has turned over and the daemon
//!    is back, [`sweep`] says the next thing on the user's behalf and the
//!    session is active again.
//!
//! # Why the cron, and not a delayed message
//!
//! Cloudflare Queues cap delivery delay at twelve hours and a weekly window
//! resets further out than that, so the wake cannot be a delayed message.
//! The minute cron in [`crate::metering`] is already the control plane's
//! clock, and a wait measured in hours does not need a finer one.
//!
//! Every step is keyed on the session row, so a cron that overlaps itself or
//! is retried does each step once: the pause is refused for a session
//! already waiting, the release skips a machine that is already off, the
//! wake moves the session out of the state its own query selects, and the
//! continuation clears the columns in the statement that guards on them.

use flyco_core::{
    ClientEvent, ControlToDaemon, HarnessEvent, MessageOrigin, SessionId, SessionState,
    UsageLimitPause, UsageWindow,
};
use skyzen_services::{Db, Queue};

use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::provisioning_queue::{self, ProvisioningJob};
use crate::rooms::{HostRooms, Rooms};
use crate::{machines, push, sessions};

/// Records that a session's harness has run out of plan, and stops it.
///
/// Three things happen, in this order, and the order is the point:
///
/// 1. The wait is written to the session row. It is what every later step
///    reads, and the only one of the three that cannot be recovered by a
///    retry — the daemon reports a limit once.
/// 2. The conversation is told. A [`HarnessEvent::UsageLimited`] frame is
///    what puts the limit *where it struck* in the transcript, between the
///    turn that was refused and the continuation that follows it hours
///    later; without it the session simply stops mid-thought.
/// 3. The turn is interrupted and the user is notified. Neither is worth
///    failing the report over: the harness has already refused the turn, and
///    a push endpoint that has gone away must not cost the session its
///    pause.
///
/// A second report of a session already waiting is answered without doing
/// any of it. Both harnesses can name one limit twice — the refused turn and
/// the snapshot explaining it — and the user does not want two notifications
/// about one wait.
///
/// # Errors
///
/// Returns [`ApiError::UsageLimitWithoutReset`] for a window that names no
/// reset time, [`ApiError::SessionNotFound`] if the session is gone, or
/// [`ApiError::InvalidTransition`] if it cannot be paused.
pub async fn pause(
    db: &Db,
    config: &ApiConfig,
    rooms: &Rooms,
    session: SessionId,
    window: &UsageWindow,
    at_unix: u64,
) -> Result<(), ApiError> {
    // A limit with no stated end is nothing this can schedule around: the
    // whole of what a pause does is stop the machine until an instant and
    // start it again before it. Refused at the boundary rather than stored
    // as a wait with no end — which would be a session stopped for ever.
    let resets_at = window
        .resets_at_unix
        .and_then(|resets| u64::try_from(resets).ok())
        .ok_or(ApiError::UsageLimitWithoutReset)?;
    let pause = UsageLimitPause::beginning(window.label.clone(), resets_at, at_unix);

    if !sessions::pause_for_usage_limit(db, session, &pause).await? {
        tracing::debug!(
            %session,
            window = %pause.window,
            "a session already waiting out this plan window reported it again"
        );
        return Ok(());
    }

    // The transcript first, then the interrupt: the frame is what explains
    // the gap the interrupt is about to open, and a room that took the
    // interrupt and lost the frame would leave a conversation that stops
    // with no reason given.
    announce(db, rooms, session, window).await;
    if let Err(error) = rooms
        .command(db, session, &ControlToDaemon::Interrupt)
        .await
    {
        // The harness refused the turn on its own — that is how flyco found
        // out — so this is tidying up rather than the thing that stops the
        // work, and a machine that is about to be stopped anyway is not
        // worth failing the pause over.
        tracing::warn!(%session, %error, "a usage-limited session did not take the interrupt");
    }
    if let Err(error) = push::notify_usage_limit(db, config, session, &pause).await {
        tracing::warn!(%session, %error, "the usage-limit notification was not delivered");
    }

    tracing::info!(
        %session,
        window = %pause.window,
        resets_at_unix = pause.resets_at_unix,
        machine_stopped = pause.machine_stopped(),
        "a session is waiting out a spent harness plan window"
    );
    Ok(())
}

/// Puts the limit and the new state in front of everyone watching.
///
/// Two frames, because they answer different questions and the page reads
/// them in different places: the harness event lands in the transcript where
/// the refused turn is, and the state change is what makes the header, the
/// rail dot and the composer re-read the session (they refetch on it).
///
/// Logged rather than raised: the wait is already durable, and a room that
/// cannot be reached costs a watcher a live update and nothing else.
async fn announce(db: &Db, rooms: &Rooms, session: SessionId, window: &UsageWindow) {
    if let Err(error) = rooms
        .broadcast(
            db,
            session,
            &ClientEvent::Harness {
                event: HarnessEvent::UsageLimited {
                    window: window.clone(),
                },
            },
        )
        .await
    {
        tracing::warn!(%session, %error, "a usage limit did not reach its session room");
    }
    if let Err(error) = rooms
        .broadcast(
            db,
            session,
            &ClientEvent::SessionStateChanged {
                state: SessionState::Paused,
            },
        )
        .await
    {
        tracing::warn!(%session, %error, "a usage-limit pause did not reach its session room");
    }
}

/// Carries every waiting session one step further, once a minute.
///
/// One pass over the handful of sessions waiting out a plan window, with the
/// three deadlines compared against one instant so a row cannot be woken and
/// continued in the same minute on two different readings of the clock.
///
/// A session that refuses one step is logged and the sweep goes on to the
/// next: one account whose credentials expired must not stop every other
/// user's session from coming back.
///
/// # Errors
///
/// Returns [`ApiError`] if the waiting sessions cannot be read at all.
pub async fn sweep(
    db: &Db,
    config: &ApiConfig,
    rooms: &Rooms,
    hosts: &HostRooms,
    queue: &Queue,
    at_unix: u64,
) -> Result<(), ApiError> {
    for wait in sessions::usage_limit_waits(db).await? {
        let pause = wait.pause();
        if pause.resets_at_unix <= at_unix && can_hear_us(&wait, &pause) {
            if let Err(error) = continue_session(db, config, rooms, &wait, &pause).await {
                tracing::warn!(
                    session = %wait.id,
                    %error,
                    "a session whose plan window reset was not continued"
                );
            }
            continue;
        }
        // Only a pause that released the machine has anything left to do
        // before the reset; one that kept it is simply waiting, and one whose
        // reset has passed with its daemon still coming up is waited for.
        let Some(resume_at) = pause.resume_at_unix else {
            continue;
        };
        if resume_at <= at_unix {
            if let Err(error) = wake(db, rooms, queue, &wait, at_unix).await {
                tracing::warn!(
                    session = %wait.id,
                    %error,
                    "a waiting session's machine was not started for its reset"
                );
            }
        } else if let Err(error) = release(db, config, hosts, &wait).await {
            tracing::warn!(
                session = %wait.id,
                %error,
                "a waiting session's machine could not be released"
            );
        }
    }
    Ok(())
}

/// Whether there is a daemon on the other end to take a message.
///
/// The two shapes of wait answer this from different states, which is why it
/// is stated once rather than inferred at the point of use:
///
/// * A pause that **kept** the machine never lost its daemon, and the session
///   is still [`Paused`](SessionState::Paused). It can be continued from
///   there.
/// * A pause that **released** it is only ready once the machine is back and
///   the daemon has said so, which is the move to
///   [`Active`](SessionState::Active). Still `Paused` means the wake has not
///   run — a cron that was down through the whole ten-minute lead, say — and
///   the session needs its machine started before it needs telling anything.
///   Still `Provisioning` means it is on its way, and the next sweep asks
///   again.
const fn can_hear_us(wait: &sessions::UsageLimitWait, pause: &UsageLimitPause) -> bool {
    match wait.state {
        SessionState::Active => true,
        SessionState::Paused => !pause.machine_stopped(),
        _ => false,
    }
}

/// Stops the machine of a session that is waiting and still holds one.
///
/// The invariant behind "the pause costs nothing", checked rather than
/// trusted to the request that wrote the wait. Only for a pause that decided
/// to release compute — the caller has already established that — and only
/// while the session is still [`Paused`](SessionState::Paused): once the wake
/// has run, the machine is meant to be on.
async fn release(
    db: &Db,
    config: &ApiConfig,
    hosts: &HostRooms,
    wait: &sessions::UsageLimitWait,
) -> Result<(), ApiError> {
    if wait.state != SessionState::Paused {
        return Ok(());
    }
    if machines::stop_for_flyco(db, config, hosts, wait.user_id, wait.id).await? {
        tracing::info!(
            session = %wait.id,
            "released the compute of a session waiting out a plan window"
        );
    }
    Ok(())
}

/// Starts the machine again, ten minutes before the window turns over.
///
/// The same [`Recover`](ProvisioningJob::Recover) job a spot reclamation
/// enqueues, because it is the same operation: start the machine the session
/// already has, on the disk it never lost. There is one implementation of
/// putting a session back on its own machine and this is not a second one.
///
/// [`sessions::recovering`] moves the session out of
/// [`Paused`](SessionState::Paused) *before* the job is enqueued, which is
/// what makes one wake per pause: the next sweep's query no longer selects a
/// session that is provisioning.
async fn wake(
    db: &Db,
    rooms: &Rooms,
    queue: &Queue,
    wait: &sessions::UsageLimitWait,
    at_unix: u64,
) -> Result<(), ApiError> {
    if wait.state != SessionState::Paused {
        return Ok(());
    }
    let machine = machines::for_session(db, wait.id)
        .await?
        .ok_or(ApiError::MachineNotFound)?;
    sessions::recovering(db, wait.id).await?;
    if let Err(error) = rooms
        .broadcast(
            db,
            wait.id,
            &ClientEvent::SessionStateChanged {
                state: SessionState::Provisioning,
            },
        )
        .await
    {
        tracing::warn!(session = %wait.id, %error, "a usage-limit wake did not reach its room");
    }
    // A machine the provider stopped is started; one the reconcile found
    // gone while the session waited is provisioned around — the row's
    // `native_id` is the difference, and `reset_for_resume` is what puts it
    // back in the queue's hands.
    let job = if machine.native_id.is_some() {
        ProvisioningJob::waking(wait.id, machine.id, at_unix)
    } else {
        machines::reset_for_resume(db, wait.id).await?;
        ProvisioningJob::first(wait.id, machine.id)
    };
    provisioning_queue::enqueue(queue, job).await?;
    tracing::info!(
        session = %wait.id,
        machine = %machine.id,
        "starting a waiting session's machine ahead of its plan window reset"
    );
    Ok(())
}

/// Says the next thing on the user's behalf, now the window has turned over.
///
/// Called only for a session [`can_hear_us`] has cleared, so what is left
/// here is the ordering.
///
/// The columns are cleared *before* the message is sent, and that order is
/// deliberate: the clear is the compare-and-swap that decides which cron
/// invocation owns this continuation, and sending first would let two of them
/// each say a thing to the same agent.
async fn continue_session(
    db: &Db,
    config: &ApiConfig,
    rooms: &Rooms,
    wait: &sessions::UsageLimitWait,
    pause: &UsageLimitPause,
) -> Result<(), ApiError> {
    if !sessions::end_usage_limit_wait(db, wait.id, wait.state).await? {
        return Ok(());
    }

    let text = pause.continuation().to_owned();
    rooms
        .command(
            db,
            wait.id,
            &ControlToDaemon::UserMessage {
                text,
                // Flyco's own nudge is flyco speaking; the message the user
                // queued into the composer while the session waited is theirs.
                origin: if pause.queued_message.is_some() {
                    MessageOrigin::User
                } else {
                    MessageOrigin::Flyco
                },
            },
        )
        .await?;
    if let Err(error) = rooms
        .broadcast(
            db,
            wait.id,
            &ClientEvent::SessionStateChanged {
                state: SessionState::Active,
            },
        )
        .await
    {
        tracing::warn!(session = %wait.id, %error, "a usage-limit resume did not reach its room");
    }
    if let Err(error) = push::notify_usage_limit_over(db, config, wait.id, &pause.window).await {
        tracing::warn!(session = %wait.id, %error, "the resume notification was not delivered");
    }
    tracing::info!(
        session = %wait.id,
        window = %pause.window,
        queued = pause.queued_message.is_some(),
        "a plan window reset and the session was told to continue"
    );
    Ok(())
}
