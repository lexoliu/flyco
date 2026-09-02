//! The machine's end of the host relay.
//!
//! One outbound WebSocket to this host's room, carrying [`HostToControl`]
//! out and [`ControlToHost`] in. The control plane never opens a connection
//! to anybody's hardware — a Cloudflare Worker has no sockets — so this is
//! the only way work reaches the machine, and it is the machine that dials.
//!
//! # The loop
//!
//! Connect, say [`HostToControl::Hello`] with what this machine currently
//! is, and then pump: jobs in, results out, a [heartbeat](HEARTBEAT) when
//! neither has happened for a while. A dropped socket is not an error; it is
//! reconnected, with the same capped exponential backoff the session relay
//! uses, and the room hands back every job it is still holding when the next
//! `Hello` arrives.
//!
//! # A result travels twice, durable half first
//!
//! [`ReportJobResult`] over REST completes the machine row: what container
//! and volume Podman actually made, or why it did not. The frame beside it
//! is what lets the room *forget* the job it is holding. So the REST call
//! goes first, and the frame only after it succeeded — a room that forgot a
//! job whose machine row was never completed is a session waiting on a
//! container nobody will ever build again. When the REST call fails the
//! frame is not sent, the result stays queued, and the room redelivers the
//! job on the next connection; running it twice is safe because
//! [every job is idempotent](super::podman).
//!
//! # Jobs do not block the socket
//!
//! A `podman run` pulling an image takes minutes. It runs on its own task,
//! so heartbeats keep flowing, a second job can start, and a revocation is
//! still read the moment it arrives.

use core::time::Duration;
use std::collections::VecDeque;
use std::sync::Arc;

use flyco_core::HostId;
use flyco_core::host::{HostFacts, ReportJobResult};
use flyco_provider::host::{ContainerJob, ControlToHost, HostToControl};
use tokio::sync::mpsc;

use crate::control::wire::{
    Socket, WireError, backoff, connect_bearer, next_frame, send, websocket_url,
};
use crate::host::podman::Jobs;
use crate::host::rest::JobResults;

/// How often a machine that has nothing to say says so anyway.
///
/// A hibernating socket can sit idle for hours, and an idle socket is
/// indistinguishable from a machine that was unplugged: until something is
/// written to it, neither end learns the connection is gone. A minute is
/// short enough that a host which lost power reads as offline before anybody
/// is scheduled onto it, and long enough that a fleet of machines is not
/// waking their Durable Objects for nothing.
pub const HEARTBEAT: Duration = Duration::from_secs(60);

/// How many finished jobs may wait for a control plane that is not
/// answering.
///
/// A machine runs a handful of containers, not thousands, and a result that
/// cannot be filed is retried rather than dropped — but the queue is bounded
/// all the same, because an unbounded one turns a control-plane outage into
/// an out-of-memory kill on somebody's own machine.
pub const RESULT_QUEUE_DEPTH: usize = 256;

/// Why the relay stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    /// The control plane revoked this machine's token. The unit exits and
    /// stays exited; the configuration records it.
    Revoked,
}

/// Everything needed to reach this machine's room.
#[derive(Clone)]
pub struct HostEndpoint {
    /// `wss://…/v1/hosts/{id}/relay`.
    url: String,
    /// This machine's `fh_` token.
    token: String,
    /// Which host this machine is.
    host: HostId,
}

impl core::fmt::Debug for HostEndpoint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HostEndpoint")
            .field("url", &self.url)
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

impl HostEndpoint {
    /// Derives the relay endpoint from the control plane's base URL.
    ///
    /// # Errors
    ///
    /// Returns [`WireError::Unaddressable`] if the base URL cannot address
    /// the relay route.
    pub fn from_base(base: &url::Url, host: HostId, token: String) -> Result<Self, WireError> {
        Ok(Self {
            url: websocket_url(base, &format!("v1/hosts/{host}/relay"))?,
            token,
            host,
        })
    }

    /// Opens one connection and greets the room with this machine's facts.
    ///
    /// The facts are re-reported on every connection rather than only at
    /// enrollment, because they change: memory is added, a disk fills,
    /// Podman is upgraded.
    async fn connect(&self, facts: &HostFacts) -> Result<Socket, WireError> {
        let mut socket = connect_bearer(&self.url, &self.token).await?;
        send(
            &mut socket,
            &HostToControl::Hello {
                facts: Box::new(facts.clone()),
            },
        )
        .await?;
        tracing::info!(host = %self.host, "greeted this machine's room");
        Ok(socket)
    }
}

/// One finished job, on its way to the control plane.
#[derive(Debug, Clone)]
struct Finished {
    /// What is reported, both ways.
    report: ReportJobResult,
    /// Whether the durable half already landed, so a retry after a dropped
    /// socket does not file it twice.
    filed: bool,
}

/// Everything the relay needs to run one machine.
pub struct HostRelay<J, A> {
    /// Where this machine's room is.
    pub endpoint: HostEndpoint,
    /// What this machine says about itself.
    pub facts: HostFacts,
    /// Where container jobs are performed.
    pub jobs: Arc<J>,
    /// Where job results are filed durably.
    pub api: A,
}

impl<J, A> core::fmt::Debug for HostRelay<J, A> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HostRelay")
            .field("endpoint", &self.endpoint)
            .field("facts", &self.facts)
            .finish_non_exhaustive()
    }
}

/// Runs one machine until its token is revoked.
///
/// # Errors
///
/// Returns [`WireError`] if the endpoint cannot be addressed or the room
/// sends a frame this daemon cannot read. A dropped socket is not an error:
/// it is reconnected.
pub async fn run<J, A>(relay: HostRelay<J, A>) -> Result<Stopped, WireError>
where
    J: Jobs,
    A: JobResults,
{
    let (results, mut finished) = mpsc::channel(RESULT_QUEUE_DEPTH);
    let mut pump = Pump {
        jobs: relay.jobs,
        api: relay.api,
        results,
        outstanding: VecDeque::new(),
    };

    let mut attempt = 0_u32;
    loop {
        match relay.endpoint.connect(&relay.facts).await {
            Ok(mut socket) => {
                attempt = 0;
                match pump.pump(&mut socket, &mut finished).await? {
                    Ended::Disconnected => {
                        tracing::warn!("this machine's room disconnected; reconnecting");
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
            Err(error) => {
                let wait = backoff(attempt);
                tracing::warn!(%error, ?wait, attempt, "could not reach this machine's room");
                attempt = attempt.saturating_add(1);
                tokio::time::sleep(wait).await;
            }
        }
    }
}

/// Why one connection ended.
enum Ended {
    /// The socket dropped; reconnect.
    Disconnected,
    /// The control plane revoked this machine; stop.
    Revoked,
}

/// The socket half of the relay, across connections.
struct Pump<J, A> {
    jobs: Arc<J>,
    api: A,
    /// Where a finished job's task reports back.
    results: mpsc::Sender<ReportJobResult>,
    /// Results that have not been fully reported yet, oldest first.
    outstanding: VecDeque<Finished>,
}

impl<J: Jobs, A: JobResults> Pump<J, A> {
    /// Pumps one connection until it ends.
    async fn pump(
        &mut self,
        socket: &mut Socket,
        finished: &mut mpsc::Receiver<ReportJobResult>,
    ) -> Result<Ended, WireError> {
        let mut heartbeat = tokio::time::interval(HEARTBEAT);
        // The first tick is immediate, and a `Hello` has just been written:
        // nothing needs saying yet.
        heartbeat.tick().await;

        // A job that finished while the socket was down is reported the
        // moment there is a socket again.
        if !self.flush(socket).await {
            return Ok(Ended::Disconnected);
        }

        loop {
            tokio::select! {
                inbound = next_frame(socket) => {
                    let Some(command) = inbound? else {
                        return Ok(Ended::Disconnected);
                    };
                    match command {
                        ControlToHost::Run { job } => self.start(job),
                        ControlToHost::Revoked => return Ok(Ended::Revoked),
                    }
                }
                Some(report) = finished.recv() => {
                    self.outstanding.push_back(Finished { report, filed: false });
                    if !self.flush(socket).await {
                        return Ok(Ended::Disconnected);
                    }
                }
                _ = heartbeat.tick() => {
                    if send(socket, &HostToControl::Heartbeat).await.is_err() {
                        return Ok(Ended::Disconnected);
                    }
                    // A control plane that was refusing reports may be back.
                    if !self.flush(socket).await {
                        return Ok(Ended::Disconnected);
                    }
                }
            }
        }
    }

    /// Performs one job on its own task, so a slow `podman run` does not
    /// hold up the socket.
    fn start(&self, job: ContainerJob) {
        let Some(machine) = job.machine() else {
            // Unreachable against this control plane — every Podman name it
            // sends is derived from a machine id — and dropped rather than
            // guessed at if a later one changes that: the room keeps the
            // job, so a daemon that learns the new shape still runs it.
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
                .send(ReportJobResult {
                    job_id: machine,
                    outcome,
                })
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

    /// Reports every outstanding result: durable half first, then the frame
    /// that lets the room forget the job.
    ///
    /// Answers whether the socket is still usable. A result that could not
    /// be filed stays queued and stops the flush — it is retried on the next
    /// heartbeat or the next connection, and until it lands the room keeps
    /// the job, which is exactly the state the redelivery is for.
    async fn flush(&mut self, socket: &mut Socket) -> bool {
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
            let frame = HostToControl::JobResult {
                job_id: finished.report.job_id,
                outcome: finished.report.outcome.clone(),
            };
            if let Err(error) = send(socket, &frame).await {
                tracing::warn!(%error, "a job result did not reach this machine's room");
                return false;
            }
            self.outstanding.pop_front();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use core::future::Future;
    use std::sync::Arc;

    use flyco_core::host::{HostFacts, JobOutcome, ReportJobResult};
    use flyco_core::machine::CpuArchitecture;
    use flyco_core::{HostId, MachineId};
    use flyco_provider::host::{
        ContainerJob, ControlToHost, HostToControl, container_name, volume_name,
    };
    use tokio::sync::mpsc;

    use super::{HostEndpoint, HostRelay, Stopped, run};
    use crate::control::rest::ControlApiError;
    use crate::host::podman::Jobs;
    use crate::host::rest::JobResults;
    use crate::testing::{Directive, Handshake, HostRelay as Loopback, Seen};

    const TOKEN: &str = "fh_the-machines-own-token";

    fn facts() -> HostFacts {
        HostFacts {
            architecture: CpuArchitecture::Arm64,
            vcpus: 10,
            memory_mib: 32 * 1024,
            disk_free_gib: 400,
            podman_version: "5.4.0".to_owned(),
            kernel: "6.11.0-19-generic".to_owned(),
            hostname: "build.lexo.cool".to_owned(),
        }
    }

    /// Jobs that are never really run, recording what was asked.
    struct FakeJobs {
        performed: mpsc::UnboundedSender<ContainerJob>,
    }

    impl Jobs for FakeJobs {
        fn perform(&self, job: ContainerJob) -> impl Future<Output = JobOutcome> + Send {
            let outcome = match &job {
                ContainerJob::Create {
                    container, volume, ..
                } => JobOutcome::Running {
                    container: container.clone(),
                    volume: volume.clone(),
                },
                _ => JobOutcome::Done,
            };
            self.performed.send(job).expect("record");
            core::future::ready(outcome)
        }
    }

    /// A control plane that records what was filed, and can refuse.
    struct FakeApi {
        filed: mpsc::UnboundedSender<ReportJobResult>,
        refusals: std::sync::atomic::AtomicU32,
    }

    impl FakeApi {
        fn new(refusals: u32) -> (Arc<Self>, mpsc::UnboundedReceiver<ReportJobResult>) {
            let (filed, received) = mpsc::unbounded_channel();
            (
                Arc::new(Self {
                    filed,
                    refusals: std::sync::atomic::AtomicU32::new(refusals),
                }),
                received,
            )
        }
    }

    impl JobResults for Arc<FakeApi> {
        fn report(
            &self,
            report: ReportJobResult,
        ) -> impl Future<Output = Result<(), ControlApiError>> + Send {
            let refused = self
                .refusals
                .fetch_update(
                    core::sync::atomic::Ordering::SeqCst,
                    core::sync::atomic::Ordering::SeqCst,
                    |left| (left > 0).then(|| left - 1),
                )
                .is_ok();
            if !refused {
                self.filed.send(report).expect("record");
            }
            core::future::ready(if refused {
                Err(ControlApiError::Transport(
                    "the control plane is down".to_owned(),
                ))
            } else {
                Ok(())
            })
        }
    }

    fn create(machine: MachineId) -> ContainerJob {
        ContainerJob::Create {
            container: container_name(machine),
            volume: volume_name(machine),
            image: "ghcr.io/lexoliu/flyco-session:latest".to_owned(),
            machine,
            bootstrap: Box::new(crate::testing::bootstrap()),
        }
    }

    /// Starts a relay against a loopback room.
    fn start(
        room: &Loopback,
        api: Arc<FakeApi>,
    ) -> (
        tokio::task::JoinHandle<Result<Stopped, super::WireError>>,
        mpsc::UnboundedReceiver<ContainerJob>,
    ) {
        let (performed, received) = mpsc::unbounded_channel();
        let host = HostId::generate();
        let relay = HostRelay {
            endpoint: HostEndpoint::from_base(&room.base, host, TOKEN.to_owned())
                .expect("an endpoint"),
            facts: facts(),
            jobs: Arc::new(FakeJobs { performed }),
            api,
        };
        (tokio::spawn(run(relay)), received)
    }

    #[tokio::test]
    async fn a_machine_greets_its_room_with_its_own_token_and_its_facts() {
        let mut room = Loopback::listen(Handshake::Silent).await;
        let (api, _filed) = FakeApi::new(0);
        let (relay, _performed) = start(&room, api);

        let connected = room.next().await.expect("a connection");
        assert_eq!(
            connected,
            Seen::Connected(Some(format!("Bearer {TOKEN}"))),
            "the machine authenticates with its host token"
        );
        let HostToControl::Hello { facts: greeted } = room.next_frame().await else {
            panic!("the first frame is a hello");
        };
        assert_eq!(*greeted, facts());

        relay.abort();
    }

    #[tokio::test]
    async fn a_job_is_performed_and_answered_durably_before_the_frame_leaves() {
        let mut room = Loopback::listen(Handshake::Silent).await;
        let (api, mut filed) = FakeApi::new(0);
        let (relay, mut performed) = start(&room, Arc::clone(&api));
        let machine = MachineId::generate();

        assert!(matches!(
            room.next_frame().await,
            HostToControl::Hello { .. }
        ));
        room.directives
            .send(Directive::Send(ControlToHost::Run {
                job: create(machine),
            }))
            .expect("send a job");

        let job = performed.recv().await.expect("the job ran");
        assert_eq!(job.container(), container_name(machine));

        // Durable first: the machine row is completed before the room is
        // allowed to forget the job.
        let report = filed.recv().await.expect("the result was filed");
        assert_eq!(report.job_id, machine);
        assert_eq!(
            report.outcome,
            JobOutcome::Running {
                container: container_name(machine),
                volume: volume_name(machine),
            }
        );

        let HostToControl::JobResult { job_id, outcome } = room.next_frame().await else {
            panic!("the room is told the job is done");
        };
        assert_eq!(job_id, machine);
        assert_eq!(outcome, report.outcome);

        relay.abort();
    }

    #[tokio::test]
    async fn a_result_the_control_plane_refused_is_not_announced_to_the_room() {
        let mut room = Loopback::listen(Handshake::Silent).await;
        // The first report is refused; the retry, on the next connection,
        // is not.
        let (api, mut filed) = FakeApi::new(1);
        let (relay, mut performed) = start(&room, Arc::clone(&api));
        let machine = MachineId::generate();

        assert!(matches!(
            room.next_frame().await,
            HostToControl::Hello { .. }
        ));
        room.directives
            .send(Directive::Send(ControlToHost::Run {
                job: ContainerJob::Stop {
                    container: container_name(machine),
                },
            }))
            .expect("send a job");
        performed.recv().await.expect("the job ran");

        // The room is closed while the result is still unreported, which is
        // what a machine that lost its socket mid-answer looks like.
        room.directives.send(Directive::Close).expect("close");

        // The machine reconnects, greets again, and only then does the
        // result land — durably first, and in the room after.
        while !matches!(
            room.next().await.expect("the machine came back"),
            Seen::Frame(HostToControl::Hello { .. })
        ) {}
        let report = filed.recv().await.expect("the retry landed");
        assert_eq!(report.job_id, machine);
        assert_eq!(report.outcome, JobOutcome::Done);

        let HostToControl::JobResult { job_id, .. } = room.next_frame().await else {
            panic!("and only then is the room told");
        };
        assert_eq!(job_id, machine);

        relay.abort();
    }

    #[tokio::test]
    async fn a_revoked_machine_stops_rather_than_reconnecting() {
        let mut room = Loopback::listen(Handshake::Silent).await;
        let (api, _filed) = FakeApi::new(0);
        let (relay, _performed) = start(&room, api);

        assert!(matches!(
            room.next_frame().await,
            HostToControl::Hello { .. }
        ));
        room.directives
            .send(Directive::Send(ControlToHost::Revoked))
            .expect("revoke");

        let stopped = tokio::time::timeout(core::time::Duration::from_secs(5), relay)
            .await
            .expect("the relay stopped")
            .expect("the task did not panic")
            .expect("a revocation is not an error");
        assert_eq!(stopped, Stopped::Revoked);
    }
}
