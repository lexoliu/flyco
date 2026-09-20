//! The machine's end of the host relay: REST out, SSE in.
//!
//! There is no socket. `flycod host` *attaches* over REST for an epoch,
//! holds one command *stream* open under it, and *posts* its outbound
//! frames in sequenced batches. The control plane never opens a
//! connection to anybody's hardware — a Cloudflare Worker has no sockets
//! — so this is the only way work reaches the machine, and it is the
//! machine that dials.
//!
//! # The loop
//!
//! Attach, reporting what this machine currently is, then pump: jobs off
//! the stream, results back in batches. A dropped stream is not an error;
//! it is reconnected, with the same capped exponential backoff the
//! session relay uses, and the room replays every command it is still
//! holding when the next attach opens a stream.
//!
//! # A result travels twice, durable half first
//!
//! [`ReportJobResult`] over REST completes the machine row: what
//! container and volume Podman actually made, or why it did not. The
//! frame beside it is what lets the room *forget* the job it is holding.
//! So the REST call goes first, and the frame only after it succeeded —
//! a room that forgot a job whose machine row was never completed is a
//! session waiting on a container nobody will ever build again. When the
//! REST call fails the frame is not sent, the result stays queued, and
//! the room redelivers the job on the next attachment; running it twice
//! is safe because [every job is idempotent](super::podman).
//!
//! # Jobs do not block the stream
//!
//! A `podman run` pulling an image takes minutes. It runs on its own
//! task, so a second job can start, and a revocation is still read the
//! moment it arrives.

use core::time::Duration;
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use flyco_core::HostId;
use flyco_core::host::{HostFacts, ReportJobResult};
use flyco_provider::host::{ContainerJob, ControlToHost, HostCommand, HostFrames, HostToControl};
use futures_util::StreamExt as _;
use tokio::sync::mpsc;

use crate::control::rest::CommandStream;
use crate::control::wire::{WireError, backoff};
use crate::host::podman::Jobs;
use crate::host::rest::{HostTransport, JobResults};

/// How long the command stream may deliver no bytes before the path is
/// treated as dead.
///
/// The room heartbeats the stream far inside this bound, so a gap this
/// long is a flow a NAT reclaimed rather than a room with nothing to
/// say. The stream, not a frame of ours, is what keeps a quiet
/// attachment alive: every poll of it renews this machine's presence
/// marker, and every byte on it proves the path both ways.
pub const STREAM_IDLE: Duration = Duration::from_secs(90);

/// How often a result that could not be filed is tried again.
///
/// A stuck result is retried rather than dropped, but only while the
/// relay runs: the room holds the job row regardless, so nothing is lost
/// by waiting out a control-plane outage at this interval.
pub const RESULT_RETRY: Duration = Duration::from_secs(60);

/// How many finished jobs may wait for a control plane that is not
/// answering.
///
/// A machine runs a handful of containers, not thousands, and a result
/// that cannot be filed is retried rather than dropped — but the queue
/// is bounded all the same, because an unbounded one turns a
/// control-plane outage into an out-of-memory kill on somebody's own
/// machine.
pub const RESULT_QUEUE_DEPTH: usize = 256;

/// Why the relay stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    /// The control plane revoked this machine's token. The unit exits and
    /// stays exited; the configuration records it.
    Revoked,
    /// A newer attach owns this machine's room. The unit exits and stays
    /// exited — re-attaching would end the winner's stream in turn, which
    /// is the ping-pong the room ended this stream over (issue #336).
    Superseded,
}

/// Everything the relay needs to run one machine.
pub struct HostRelay<J, A> {
    /// Which host this machine is — for logging; the transport itself is
    /// bound to it inside `api`.
    pub host: HostId,
    /// What this machine says about itself, re-reported on every attach.
    pub facts: HostFacts,
    /// Where container jobs are performed.
    pub jobs: Arc<J>,
    /// Where job results are filed durably, and the relay's transport.
    pub api: A,
}

impl<J, A> core::fmt::Debug for HostRelay<J, A> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HostRelay")
            .field("host", &self.host)
            .field("facts", &self.facts)
            .finish_non_exhaustive()
    }
}

/// One attach to the host room: the epoch naming it, the command stream
/// opened under it, and the sequence the next outbound frame takes.
struct Attachment {
    /// The attach this stream belongs to.
    epoch: u64,
    /// Commands from the room, in the order it sequenced them.
    commands: CommandStream<HostCommand>,
    /// The sequence `pending`'s head will carry.
    next_seq: u64,
}

/// Attaches to the host room and opens its command stream.
///
/// One step rather than two because neither half is useful alone: an
/// attach without its stream is a machine that can speak but not hear,
/// and a stream without the attach's epoch is refused.
async fn attach<A: HostTransport>(api: &A, facts: &HostFacts) -> Result<Attachment, WireError> {
    let attached = api.attach(facts).await?;
    let commands = api.commands(attached.epoch, STREAM_IDLE).await?;
    tracing::info!(epoch = attached.epoch, "attached to this machine's room");
    Ok(Attachment {
        epoch: attached.epoch,
        commands,
        next_seq: 1,
    })
}

/// Problem types an attach can never retry away.
///
/// A revoked token is not one of them — it is answered with
/// [`Stopped::Revoked`], because a machine that was offline when the
/// command arrived learns its revocation from the refused attach.
const FATAL_REFUSALS: &[&str] = &["missing-credential", "host-not-found", "host-removed"];

/// One finished job, on its way to the control plane.
#[derive(Debug, Clone)]
struct Finished {
    /// The command sequence that asked for the job.
    ///
    /// Kept so a redelivery of the same row is recognized until the
    /// answer is confirmed stored — which is when the room drops the row
    /// and the sequence can leave [`Pump::handled`].
    seq: u64,
    /// What is reported, both ways.
    report: ReportJobResult,
    /// Whether the durable half already landed, so a retry after a
    /// dropped stream does not file it twice.
    filed: bool,
}

/// The transport half of the relay, across attachments.
struct Pump<J, A> {
    jobs: Arc<J>,
    api: A,
    /// Where a finished job's task reports back: the command sequence and
    /// the result it produced.
    results: mpsc::Sender<(u64, ReportJobResult)>,
    /// Results whose durable half has not landed yet, oldest first.
    outstanding: VecDeque<Finished>,
    /// Frames staged but not confirmed stored, each with the command
    /// sequence whose job it answers.
    ///
    /// They outlive the attach they were produced under: a dropped stream
    /// ends the epoch, and the next attach re-sends the whole tail from
    /// sequence one.
    pending: VecDeque<(u64, HostToControl)>,
    /// Command sequences already acted on, so a redelivered job is
    /// recognized rather than run again.
    ///
    /// A job row is retired by its answer, not by delivery — so a machine
    /// that took a job and lost its stream is asked again, and this set
    /// is what makes "again" cheap: an entry leaves when the answer is
    /// confirmed stored, which is when the row is gone for good.
    handled: HashSet<u64>,
    /// The wait the control plane named on the last refused batch,
    /// honoured by the reconnect that follows (issue #342).
    retry_hint: Option<Duration>,
}

/// Why one attachment ended.
enum Ended {
    /// The stream dropped; re-attach.
    Disconnected,
    /// A newer attach superseded this one; stop.
    Superseded,
    /// The control plane revoked this machine; stop.
    Revoked,
}

/// Runs one machine until its token is revoked.
///
/// # Errors
///
/// Returns [`WireError`] if the relay queue overflows or the control
/// plane refuses the attach in a way retrying cannot fix. A dropped
/// stream is not an error: it is reconnected.
pub async fn run<J, A>(relay: HostRelay<J, A>) -> Result<Stopped, WireError>
where
    J: Jobs,
    A: JobResults + HostTransport,
{
    let (results, mut finished) = mpsc::channel(RESULT_QUEUE_DEPTH);
    let mut pump = Pump {
        jobs: relay.jobs,
        api: relay.api,
        results,
        outstanding: VecDeque::new(),
        pending: VecDeque::new(),
        handled: HashSet::new(),
        retry_hint: None,
    };
    // How far down the room's command log this machine has applied —
    // global across attachments, exactly as the log's sequences are.
    let mut applied = 0_u64;

    let mut attempt = 0_u32;
    loop {
        let mut attachment = match attach(&pump.api, &relay.facts).await {
            Ok(attachment) => attachment,
            Err(error) => {
                if revoked(&error) {
                    tracing::warn!(
                        "this machine's credential was refused; treating that as revoked"
                    );
                    return Ok(Stopped::Revoked);
                }
                if fatal_attach(&error) {
                    return Err(error);
                }
                // A refusal that names its wait is honoured over the ladder.
                let wait = backoff(attempt).max(error.retry_after().unwrap_or_default());
                tracing::warn!(%error, ?wait, attempt, "could not reach this machine's room");
                attempt = attempt.saturating_add(1);
                tokio::time::sleep(wait).await;
                continue;
            }
        };

        let attached_at = tokio::time::Instant::now();
        match pump
            .pump(&mut attachment, &mut finished, &mut applied)
            .await?
        {
            Ended::Disconnected => {
                // As on the session relay: a stream dead within moments
                // of its attach is counted with the failed attaches — an
                // instant death is the signature of a peer superseding
                // it, and re-attaching uncounted is the ping-pong that
                // spends the account's request budget. A stream that
                // held resets the ladder instead.
                if attached_at.elapsed() < crate::control::wire::ATTACH_STABLE {
                    attempt = attempt.saturating_add(1);
                } else {
                    attempt = 0;
                }
                let wait = backoff(attempt).max(pump.retry_hint.take().unwrap_or_default());
                tracing::warn!(
                    ?wait,
                    attempt,
                    "this machine's room disconnected; re-attaching"
                );
                tokio::time::sleep(wait).await;
            }
            Ended::Superseded => {
                tracing::warn!(
                    "a newer attach owns this machine's room; \
                     flycod host is stopping and will not reconnect"
                );
                return Ok(Stopped::Superseded);
            }
            Ended::Revoked => {
                tracing::warn!(
                    "the control plane revoked this machine; \
                     flycod host is stopping and will not reconnect"
                );
                return Ok(Stopped::Revoked);
            }
        }
    }
}

/// Whether an attach refusal is the revocation a host missed.
///
/// A host token never rotates while it lives, so a refused token is a
/// dead one — which for this machine is what `Revoked` would have said,
/// delivered to the attach instead because the command could not reach
/// an offline host.
fn revoked(error: &WireError) -> bool {
    matches!(
        error,
        WireError::ControlApi(api) if api.kind() == Some("invalid-host-credential")
    )
}

/// Whether an attach failure ends the run rather than backing off.
fn fatal_attach(error: &WireError) -> bool {
    match error {
        WireError::Unaddressable(_) | WireError::Unwelcome(_) => true,
        WireError::ControlApi(api) => api
            .kind()
            .is_some_and(|kind| FATAL_REFUSALS.contains(&kind)),
        _ => false,
    }
}

impl<J: Jobs, A: JobResults + HostTransport> Pump<J, A> {
    /// Pumps one attachment until it ends.
    async fn pump(
        &mut self,
        attach: &mut Attachment,
        finished: &mut mpsc::Receiver<(u64, ReportJobResult)>,
        applied: &mut u64,
    ) -> Result<Ended, WireError> {
        // No separate acknowledgement cursor is kept here: a host's log
        // rows are deleted by the `JobResult` frame that answers them,
        // not by `ack_through` — every batch `flush` posts carries
        // `applied` along for the non-job rows, and a `Revoked` ends the
        // run either way.
        let mut retry = tokio::time::interval(RESULT_RETRY);
        // The first tick is immediate; an attach that just succeeded has
        // nothing to retry yet.
        retry.tick().await;

        // A job that finished while the stream was down is reported the
        // moment there is a stream again.
        if !self.flush(attach, *applied).await {
            return Ok(Ended::Disconnected);
        }

        loop {
            tokio::select! {
                event = attach.commands.next() => {
                    let Some(command) = event else {
                        return Ok(Ended::Disconnected);
                    };
                    let command = match command {
                        Ok(command) => command,
                        Err(error) => {
                            tracing::warn!(%error, "the command stream failed");
                            return Ok(Ended::Disconnected);
                        }
                    };
                    if let Some(seq) = command.seq {
                        *applied = (*applied).max(seq);
                    }
                    match command.command {
                        ControlToHost::Run { job } => {
                            // A job is always a row — the one command the
                            // room composes without one is `Superseded`,
                            // and a `Run` that cannot name its position can
                            // neither be deduplicated nor answered.
                            let Some(seq) = command.seq else {
                                return Err(WireError::Undecodable(
                                    "a container job arrived without its log position"
                                        .to_owned(),
                                ));
                            };
                            // A redelivery means the row is still held —
                            // the answer never landed. `handled` holds
                            // exactly the sequences whose job is running
                            // or whose answer is in flight, so a re-seen
                            // row is skipped rather than run again.
                            if self.handled.insert(seq) {
                                self.start(seq, job);
                            } else {
                                tracing::debug!(
                                    seq,
                                    "a redelivered job is already handled; skipping it"
                                );
                            }
                        }
                        ControlToHost::Revoked => return Ok(Ended::Revoked),
                        ControlToHost::Superseded => return Ok(Ended::Superseded),
                    }
                }
                Some((seq, report)) = finished.recv() => {
                    self.outstanding.push_back(Finished {
                        seq,
                        report,
                        filed: false,
                    });
                    if !self.flush(attach, *applied).await {
                        return Ok(Ended::Disconnected);
                    }
                }
                _ = retry.tick() => {
                    // A control plane that was refusing reports may be
                    // back, and the retry carries this stream's ack state
                    // along with it.
                    if !self.flush(attach, *applied).await {
                        return Ok(Ended::Disconnected);
                    }
                }
            }
        }
    }

    /// Performs one job on its own task, so a slow `podman run` does not
    /// hold up the stream.
    fn start(&self, seq: u64, job: ContainerJob) {
        let Some(machine) = job.machine() else {
            // Unreachable against this control plane — every Podman name
            // it sends is derived from a machine id — and dropped rather
            // than guessed at if a later one changes that: the room keeps
            // the job, so a daemon that learns the new shape still runs
            // it.
            tracing::error!(
                container = job.container(),
                "a container job names no machine this daemon can report against"
            );
            return;
        };
        let jobs = Arc::clone(&self.jobs);
        let results = self.results.clone();
        tokio::spawn(async move {
            let outcome = jobs.perform(job).await;
            if results
                .send((
                    seq,
                    ReportJobResult {
                        job_id: machine,
                        outcome,
                    },
                ))
                .await
                .is_err()
            {
                tracing::error!(
                    %machine,
                    "a container job finished after the relay stopped; \
                     its room will redeliver the job"
                );
            }
        });
    }

    /// Reports every outstanding result — durable half first, then the
    /// frame that lets the room forget the job — and posts every staged
    /// frame as one sequenced batch.
    ///
    /// Answers whether the attachment is still usable. A result that
    /// could not be filed stays queued and stops the flush — it is
    /// retried on the next tick or the next attachment, and until it
    /// lands the room keeps the job, which is exactly the state the
    /// redelivery is for. A refused or lost batch leaves `pending`
    /// untouched and answers `false`: the attach is dropped and the next
    /// epoch re-sends the whole tail, which the room deduplicates by
    /// sequence.
    async fn flush(&mut self, attach: &mut Attachment, applied: u64) -> bool {
        while let Some(finished) = self.outstanding.front_mut() {
            if !finished.filed {
                if let Err(error) = self.api.report(finished.report.clone()).await {
                    tracing::warn!(
                        %error,
                        machine = %finished.report.job_id,
                        "a job result did not reach the control plane; \
                         its room will redeliver the job"
                    );
                    return true;
                }
                finished.filed = true;
            }
            let finished = self
                .outstanding
                .pop_front()
                .expect("the front a front_mut just touched");
            self.pending.push_back((
                finished.seq,
                HostToControl::JobResult {
                    job_id: finished.report.job_id,
                    outcome: finished.report.outcome,
                },
            ));
        }

        if self.pending.is_empty() && applied == 0 {
            return true;
        }
        // An empty batch is still a contact: it renews presence, and its
        // `ack_through` is how the room learns a delivered command is done.
        let batch = HostFrames {
            epoch: attach.epoch,
            from_seq: attach.next_seq,
            ack_through: applied,
            frames: self
                .pending
                .iter()
                .map(|(_, frame)| frame.clone())
                .collect(),
        };
        if let Err(error) = self.api.frames(&batch).await {
            self.retry_hint = error.retry_after();
            tracing::warn!(
                %error,
                frames = batch.frames.len(),
                "a frame batch did not reach the room; retrying it on the next attach"
            );
            return false;
        }
        attach.next_seq = attach
            .next_seq
            .saturating_add(u64::try_from(batch.frames.len()).unwrap_or(0));
        // Confirmed means the room retired the job rows these frames
        // answered, so their sequences can never be delivered again.
        for (seq, _) in self.pending.drain(..) {
            self.handled.remove(&seq);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    //! The machine's end of the relay, driven against a real loopback room.

    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use flyco_core::host::{HostFacts, JobOutcome, ReportJobResult};
    use flyco_core::machine::CpuArchitecture;
    use flyco_core::{HostId, MachineId};
    use flyco_provider::host::{ContainerJob, ControlToHost, HostToControl, container_name};
    use tokio::sync::{Notify, mpsc};

    use super::{HostRelay, Stopped, run};
    use crate::control::rest::{CommandStream, ControlApiError};
    use crate::host::podman::Jobs;
    use crate::host::rest::{HostTransport, HttpHostApi, JobResults};
    use crate::testing::{Directive, HostRelay as Room, Seen};

    /// A host token shaped the way the control plane mints them.
    const TOKEN: &str = "fh_the-machines-own-token";

    /// What this machine says about itself.
    fn facts() -> HostFacts {
        HostFacts {
            architecture: CpuArchitecture::Arm64,
            vcpus: 8,
            memory_mib: 32768,
            disk_free_gib: 200,
            podman_version: "5.4.0".to_owned(),
            kernel: "6.12.1".to_owned(),
            hostname: "build-box".to_owned(),
        }
    }

    /// A `stop` on one machine: the smallest job that is still a real row.
    fn stop_job(machine: MachineId) -> ContainerJob {
        ContainerJob::Stop {
            container: container_name(machine),
        }
    }

    /// Somewhere jobs are *recorded* rather than performed.
    ///
    /// `pending` makes `perform` never return — the shape a job still
    /// running when the stream drops has, and the one the redelivery tests
    /// need.
    struct FakeJobs {
        performed: mpsc::UnboundedSender<ContainerJob>,
        outcome: Option<JobOutcome>,
        /// Whether a started job's report goes out the moment the relay
        /// asks for it, or only once the test releases the gate — the hook
        /// "is the frame sent before the durable write lands?" needs.
        gate: Arc<Notify>,
    }

    impl Jobs for FakeJobs {
        async fn perform(&self, job: ContainerJob) -> JobOutcome {
            self.performed.send(job).expect("the test is listening");
            self.gate.notified().await;
            self.outcome.clone().unwrap_or(JobOutcome::Done)
        }
    }

    /// The relay's control plane: the relay transport is real HTTP+SSE
    /// against the loopback room, and `report` records — or refuses — so a
    /// test can watch the durable half of a result land before its frame.
    #[derive(Clone)]
    struct FakeApi {
        transport: HttpHostApi,
        filed: mpsc::UnboundedSender<ReportJobResult>,
        /// When set, the next `report` is refused once — a control plane
        /// mid-deploy — and the flag clears so the retry lands.
        refuse_next: Arc<AtomicBool>,
        /// Lets a test hold a report open, so "the frame only leaves once
        /// the durable write did" is assertable rather than a race.
        gate: Arc<Notify>,
    }

    impl JobResults for FakeApi {
        async fn report(&self, report: ReportJobResult) -> Result<(), ControlApiError> {
            if self.refuse_next.swap(false, Ordering::SeqCst) {
                return Err(ControlApiError::Transport(
                    "the control plane is mid-deploy".to_owned(),
                ));
            }
            self.filed.send(report).expect("the test is listening");
            self.gate.notified().await;
            Ok(())
        }
    }

    impl HostTransport for FakeApi {
        fn attach(
            &self,
            facts: &HostFacts,
        ) -> impl core::future::Future<
            Output = Result<flyco_provider::host::HostAttached, ControlApiError>,
        > + Send {
            self.transport.attach(facts)
        }

        fn commands(
            &self,
            epoch: u64,
            idle: core::time::Duration,
        ) -> impl core::future::Future<
            Output = Result<CommandStream<flyco_provider::host::HostCommand>, ControlApiError>,
        > + Send {
            self.transport.commands(epoch, idle)
        }

        fn frames(
            &self,
            batch: &flyco_provider::host::HostFrames,
        ) -> impl core::future::Future<Output = Result<(), ControlApiError>> + Send {
            self.transport.frames(batch)
        }
    }

    /// Everything a host relay test drives.
    struct Harness {
        room: Room,
        /// The machine the relay under test enrolled as — a second attach
        /// for the same one is how a test supersedes it.
        host: HostId,
        performed: mpsc::UnboundedReceiver<ContainerJob>,
        filed: mpsc::UnboundedReceiver<ReportJobResult>,
        /// Releases a job's `perform` — the job is running until it fires.
        job_gate: Arc<Notify>,
        /// Releases a `report` — the durable write lands once it fires.
        report_gate: Arc<Notify>,
        refuse_next: Arc<AtomicBool>,
        run: tokio::task::JoinHandle<Result<Stopped, crate::control::wire::WireError>>,
    }

    impl Harness {
        /// Starts a relay against a live room.
        async fn start() -> Self {
            Self::against(Room::listen().await)
        }

        /// Starts a relay whose jobs return `outcome` — or run until the
        /// gate fires when `outcome` is `None`.
        fn against(room: Room) -> Self {
            let host = HostId::generate();
            let (performed_out, performed) = mpsc::unbounded_channel();
            let (filed_out, filed) = mpsc::unbounded_channel();
            let job_gate = Arc::new(Notify::new());
            let report_gate = Arc::new(Notify::new());
            let refuse_next = Arc::new(AtomicBool::new(false));
            let api = FakeApi {
                transport: HttpHostApi::new(room.base.clone(), host, TOKEN.to_owned()),
                filed: filed_out,
                refuse_next: Arc::clone(&refuse_next),
                gate: Arc::clone(&report_gate),
            };
            let run = tokio::spawn(run(HostRelay {
                host,
                facts: facts(),
                jobs: Arc::new(FakeJobs {
                    performed: performed_out,
                    outcome: Some(JobOutcome::Done),
                    gate: Arc::clone(&job_gate),
                }),
                api,
            }));
            Self {
                room,
                host,
                performed,
                filed,
                job_gate,
                report_gate,
                refuse_next,
                run,
            }
        }

        /// Waits for the room to see this machine's attach and stream.
        async fn attached(&mut self) -> serde_json::Value {
            let Some(Seen::Attached { body, .. }) = self.room.next().await else {
                panic!("the machine did not attach");
            };
            assert!(
                matches!(self.room.next().await, Some(Seen::StreamOpened(_))),
                "an accepted attach opens the command stream"
            );
            body
        }

        /// Ends the open command stream and waits for the re-attach.
        async fn reconnect(&mut self) {
            self.room
                .directives
                .send(Directive::Close)
                .expect("the room is live");
            assert_eq!(self.room.next().await, Some(Seen::StreamClosed));
            self.attached().await;
        }

        /// Queues a command the way the room's mailbox does.
        fn command(&self, command: ControlToHost) {
            self.room
                .directives
                .send(Directive::Send(command))
                .expect("the room is live");
        }
    }

    #[tokio::test]
    async fn a_machine_attaches_with_its_token_and_its_facts() {
        let mut room = Room::listen().await;
        let host = HostId::generate();
        let (performed, _) = mpsc::unbounded_channel();
        let (filed, _) = mpsc::unbounded_channel();
        let api = FakeApi {
            transport: HttpHostApi::new(room.base.clone(), host, TOKEN.to_owned()),
            filed,
            refuse_next: Arc::new(AtomicBool::new(false)),
            gate: Arc::new(Notify::new()),
        };
        let run = tokio::spawn(run(HostRelay {
            host,
            facts: facts(),
            jobs: Arc::new(FakeJobs {
                performed,
                outcome: Some(JobOutcome::Done),
                gate: Arc::new(Notify::new()),
            }),
            api,
        }));

        let Some(Seen::Attached {
            authorization,
            body,
        }) = room.next().await
        else {
            panic!("the machine did not attach");
        };
        assert_eq!(
            authorization.as_deref(),
            Some("Bearer fh_the-machines-own-token"),
            "the attach carries this machine's own token"
        );
        assert_eq!(
            body["facts"]["hostname"], "build-box",
            "and what the machine measured about itself"
        );

        run.abort();
    }

    #[tokio::test]
    async fn a_job_is_performed_and_answered_durably_before_the_frame_leaves() {
        let mut harness = Harness::start().await;
        harness.attached().await;

        let machine = MachineId::generate();
        harness.command(ControlToHost::Run {
            job: stop_job(machine),
        });
        assert_eq!(
            harness.performed.recv().await,
            Some(stop_job(machine)),
            "the job reaches the machine"
        );
        // The job's `perform` is still gated — release it so the result
        // exists at all.
        harness.job_gate.notify_one();
        // …and the report is still gated: the frame must not leave while
        // the durable half is in flight.
        assert_eq!(
            harness.filed.recv().await.map(|report| report.job_id),
            Some(machine),
            "the result is filed with the control plane first"
        );
        assert!(
            tokio::time::timeout(
                core::time::Duration::from_millis(300),
                harness.room.next_frame()
            )
            .await
            .is_err(),
            "the room must not see the frame before the durable write lands"
        );

        harness.report_gate.notify_one();
        assert_eq!(
            harness.room.next_frame().await,
            HostToControl::JobResult {
                job_id: machine,
                outcome: JobOutcome::Done,
            },
            "the frame follows once the row is durable"
        );

        harness.run.abort();
    }

    #[tokio::test]
    async fn a_result_the_room_never_got_is_refiled_on_the_next_attach() {
        let mut harness = Harness::start().await;
        harness.attached().await;

        // The first report is refused; the frame that would let the room
        // forget the job must not leave with the durable half unwritten.
        harness.refuse_next.store(true, Ordering::SeqCst);
        let machine = MachineId::generate();
        harness.command(ControlToHost::Run {
            job: stop_job(machine),
        });
        harness.performed.recv().await;
        harness.job_gate.notify_one();
        harness.report_gate.notify_one();
        assert!(
            tokio::time::timeout(
                core::time::Duration::from_millis(300),
                harness.room.next_frame()
            )
            .await
            .is_err(),
            "a refused report announces nothing to the room"
        );

        // A dropped stream is where the retry happens: the next attach's
        // first flush files the result, then the frame.
        harness.reconnect().await;
        assert_eq!(
            harness.filed.recv().await.map(|report| report.job_id),
            Some(machine),
            "the retry lands the durable half"
        );
        harness.report_gate.notify_one();
        assert_eq!(
            harness.room.next_frame().await,
            HostToControl::JobResult {
                job_id: machine,
                outcome: JobOutcome::Done,
            }
        );

        harness.run.abort();
    }

    #[tokio::test]
    async fn a_redelivered_job_is_not_run_twice() {
        // A job that is still running when the stream drops is redelivered
        // on the next attach — the room holds the row until the result
        // retires it. The machine must recognise the row rather than start
        // the same container again.
        let mut harness = Harness::start().await;
        harness.attached().await;
        // Jobs never finish on their own in this test: `outcome` never
        // lands because the gate is never fired.
        let machine = MachineId::generate();
        harness.command(ControlToHost::Run {
            job: stop_job(machine),
        });
        harness.performed.recv().await;

        harness.reconnect().await;

        // The replayed row arrives — and is skipped, so nothing else is
        // performed: the first `stop` is still the only job this machine
        // ever ran.
        tokio::time::sleep(core::time::Duration::from_millis(300)).await;
        assert!(
            harness.performed.try_recv().is_err(),
            "a redelivered job must not be performed a second time"
        );

        harness.run.abort();
    }

    #[tokio::test]
    async fn a_revoked_machine_stops_rather_than_reconnecting() {
        let mut harness = Harness::start().await;
        harness.attached().await;

        harness.command(ControlToHost::Revoked);

        let stopped = tokio::time::timeout(core::time::Duration::from_secs(5), harness.run)
            .await
            .expect("the run ended")
            .expect("the run did not panic");
        assert_eq!(
            stopped.expect("a revocation is not an error"),
            Stopped::Revoked
        );
    }

    #[tokio::test]
    async fn a_superseded_machine_stops_rather_than_reconnecting() {
        let mut harness = Harness::start().await;
        harness.attached().await;

        // A second `flycod host` attached as this machine — the unit
        // started twice, or a spare launched beside it — and the room
        // bumped its epoch. The superseded stream is told why before it
        // ends, and the answer to losing is to stop: re-attaching would
        // end the winner's stream in turn (issue #336).
        let spare = HttpHostApi::new(harness.room.base.clone(), harness.host, TOKEN.to_owned());
        spare.attach(&facts()).await.expect("the spare's attach");
        assert!(
            matches!(harness.room.next().await, Some(Seen::Attached { .. })),
            "the room saw the spare attach"
        );
        assert_eq!(
            harness.room.next().await,
            Some(Seen::StreamClosed),
            "and the superseded stream ending"
        );

        let stopped = tokio::time::timeout(core::time::Duration::from_secs(5), harness.run)
            .await
            .expect("the run ended")
            .expect("the run did not panic");
        assert_eq!(
            stopped.expect("a supersession is not an error"),
            Stopped::Superseded
        );
    }

    #[tokio::test]
    async fn a_refused_attach_is_read_as_the_revocation_it_is() {
        let room = Room::revoked().await;
        let host = HostId::generate();
        let (performed, _) = mpsc::unbounded_channel();
        let (filed, _) = mpsc::unbounded_channel();
        let api = FakeApi {
            transport: HttpHostApi::new(room.base.clone(), host, "fh_a-revoked-token".to_owned()),
            filed,
            refuse_next: Arc::new(AtomicBool::new(false)),
            gate: Arc::new(Notify::new()),
        };
        let run = tokio::spawn(run(HostRelay {
            host,
            facts: facts(),
            jobs: Arc::new(FakeJobs {
                performed,
                outcome: Some(JobOutcome::Done),
                gate: Arc::new(Notify::new()),
            }),
            api,
        }));

        let stopped = tokio::time::timeout(core::time::Duration::from_secs(5), run)
            .await
            .expect("the run ended")
            .expect("the run did not panic");
        assert_eq!(
            stopped.expect("a revoked credential ends the run, not errors it"),
            Stopped::Revoked
        );
    }
}
