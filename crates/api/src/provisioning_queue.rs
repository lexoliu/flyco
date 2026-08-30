//! The provisioning queue: what opens a session's machine, and when.
//!
//! Provisioning is minutes long — an Azure machine took over five in a live
//! test — and a Worker request cannot wait on that, so `POST /v1/sessions`
//! writes its rows, enqueues one job, and answers `201`. This module is the
//! other end: it mints the session's daemon token, unseals the harness
//! credential the machine will run the agent under, asks the provider for a
//! machine, and records what actually came back.
//!
//! # Running twice is normal
//!
//! Cloudflare Queues delivers at least once, so a job that provisioned a
//! machine and then failed to be acknowledged arrives again. Provisioning a
//! second machine for a session would be a cloud resource nobody is billing
//! anybody for, so nothing here is written assuming a single delivery:
//!
//! * The `machines.session_id` UNIQUE index means a session has exactly one
//!   machine row for its whole life. The job names that row's id, so a
//!   redelivery addresses the same row rather than reserving another.
//! * Every provider-native resource name is derived from that machine id, so
//!   a second call to the provider is an *update* of the same machine — an
//!   Azure `PUT` against the names it already made, a podman script that
//!   removes the container before recreating it — rather than a second one.
//! * A job whose machine row is already running with a provider-native name
//!   is acknowledged without touching the provider at all.
//!
//! # Failure is visible, and retries are counted here
//!
//! A session whose provision failed must never sit in `provisioning`
//! for ever: it moves to [`SessionState::Failed`] carrying what the provider
//! said, which is what the user has to act on. Only a genuinely transient
//! failure is retried — [`ProviderError::is_transient`] is the whole rule —
//! and the attempt count travels *in the job* rather than in the queue's own
//! redelivery counter, because that counter is not visible to the handler
//! and a message Cloudflare quietly stops redelivering would leave the
//! session stuck in exactly the state this is here to prevent.

use core::time::Duration;

use flyco_core::{HarnessKind, MachineId, PermissionMode, SessionId, SessionState, UserId};
use flyco_provider::{DaemonBootstrap, ProviderError, ProvisionRequest};
use serde::{Deserialize, Serialize};
use skyzen_services::queue::{
    QueueBatch, QueueBatchDisposition, QueueMessageDisposition, QueueRetry, SendOptions,
};
use skyzen_services::{Db, Queue};

use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::machines::MachineRow;
use crate::provisioning::Provisioner;
use crate::{daemon_tokens, harness_accounts, machines, provisioning, sessions};

/// How many times one machine is asked for before the session is failed.
///
/// Three, because the only failures retried here are transport ones, and a
/// destination that refuses three connections spread over a minute and a
/// half is down rather than busy. A fourth attempt would spend another
/// ninety seconds telling the user nothing new.
pub const MAX_ATTEMPTS: u32 = 3;

/// How long a retried job waits before it is delivered again.
const RETRY_DELAY: Duration = Duration::from_secs(30);

/// One provisioning job, as the queue carries it.
///
/// The machine id is carried rather than looked up, and that is what makes a
/// redelivery safe: every provider-native resource name is derived from it,
/// so two deliveries of one job address one machine. A job naming a machine
/// the session no longer has is a job from a superseded attempt, and is
/// dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvisioningJob {
    /// The session whose machine this builds.
    pub session: SessionId,
    /// The reserved `machines` row it fills in.
    pub machine: MachineId,
    /// Which attempt this is, counting from one.
    pub attempt: u32,
}

impl ProvisioningJob {
    /// The first attempt at a session's machine.
    #[must_use]
    pub const fn first(session: SessionId, machine: MachineId) -> Self {
        Self {
            session,
            machine,
            attempt: 1,
        }
    }

    /// The same machine, asked for once more.
    const fn again(self) -> Self {
        Self {
            attempt: self.attempt.saturating_add(1),
            ..self
        }
    }
}

/// Puts a job on the queue.
///
/// # Errors
///
/// Returns [`ApiError::Queue`] if the queue refuses the message, which the
/// caller must treat as the provision having failed: a session whose job was
/// never enqueued would wait for a consumer that is never going to run.
pub async fn enqueue(queue: &Queue, job: ProvisioningJob) -> Result<(), ApiError> {
    queue.send_json(&job).await?;
    tracing::info!(
        session = %job.session,
        machine = %job.machine,
        attempt = job.attempt,
        "queued a provisioning job"
    );
    Ok(())
}

/// Why one job stopped, and what the queue should do about the message.
#[derive(Debug)]
enum Settled {
    /// The job is finished with, whether or not a machine came out of it.
    Done,
    /// The job could not be *read* — the database was unreachable, a sealed
    /// credential would not open. Nothing was decided, so the message is
    /// held for redelivery rather than acknowledged into silence.
    Redeliver(ApiError),
}

/// Performs a batch of provisioning jobs.
///
/// The dual-target half of the consumer: the exported Cloudflare `queue`
/// handler below is a shim that opens the environment's bindings and calls
/// this, and the crate's tests call it directly against an in-memory queue.
///
/// One decision per message, because a batch that failed as a unit would
/// re-run the jobs that succeeded, and each of those is a live cloud
/// machine.
pub async fn consume(
    db: &Db,
    config: &ApiConfig,
    queue: &Queue,
    provisioner: &mut impl Provisioner,
    batch: QueueBatch<ProvisioningJob>,
) -> QueueBatchDisposition {
    let mut decisions = Vec::with_capacity(batch.messages.len());
    for message in batch.messages {
        decisions.push(
            match perform(db, config, queue, provisioner, message.body).await {
                Settled::Done => QueueMessageDisposition::Ack,
                Settled::Redeliver(error) => {
                    tracing::error!(
                        session = %message.body.session,
                        %error,
                        "holding a provisioning job for redelivery: nothing was decided"
                    );
                    QueueMessageDisposition::Retry(
                        QueueRetry::new().with_delay_seconds(
                            RETRY_DELAY.as_secs().try_into().unwrap_or(u32::MAX),
                        ),
                    )
                }
            },
        );
    }
    QueueBatchDisposition::PerMessage(decisions)
}

/// Performs exactly one job.
async fn perform(
    db: &Db,
    config: &ApiConfig,
    queue: &Queue,
    provisioner: &mut impl Provisioner,
    job: ProvisioningJob,
) -> Settled {
    match claim(db, job).await {
        Ok(None) => Settled::Done,
        Ok(Some(claimed)) => match build(db, config, provisioner, &claimed).await {
            Ok(()) => Settled::Done,
            Err(Provisioned::Failed(reason)) => {
                match sessions::fail(db, job.session, &reason).await {
                    Ok(()) => Settled::Done,
                    Err(error) => Settled::Redeliver(error),
                }
            }
            Err(Provisioned::Retry(reason)) => retry(db, queue, job, &reason).await,
        },
        Err(error) => Settled::Redeliver(error),
    }
}

/// A session and the machine row this job is allowed to fill in.
struct Claim {
    session: SessionId,
    user: UserId,
    harness: HarnessKind,
    machine: MachineRow,
}

/// Decides whether this delivery still has work to do.
///
/// `None` means the job is spent — the session is gone, it is no longer
/// waiting for a machine, the job names a machine that was superseded, or
/// the machine already exists — and the message is acknowledged rather than
/// redelivered into the same answer.
async fn claim(db: &Db, job: ProvisioningJob) -> Result<Option<Claim>, ApiError> {
    let Some(target) = sessions::provisioning_target(db, job.session).await? else {
        tracing::info!(session = %job.session, "dropping a job for a session that no longer exists");
        return Ok(None);
    };
    if target.state != SessionState::Provisioning {
        tracing::info!(
            session = %job.session,
            state = ?target.state,
            "dropping a job for a session that is no longer waiting for a machine"
        );
        return Ok(None);
    }

    let Some(machine) = machines::for_session(db, job.session).await? else {
        // Creation reserves the row before it enqueues, so there is no
        // ordering in which this is a race. It is a session that was written
        // by something other than `POST /v1/sessions`.
        sessions::fail(
            db,
            job.session,
            "this session has no machine row to fill in",
        )
        .await?;
        return Ok(None);
    };
    if machine.id != job.machine {
        tracing::info!(
            session = %job.session,
            job = %job.machine,
            machine = %machine.id,
            "dropping a job superseded by a later attempt"
        );
        return Ok(None);
    }
    if machine.is_provisioned() {
        tracing::info!(
            session = %job.session,
            machine = %machine.id,
            "this session's machine already exists; the job was delivered twice"
        );
        return Ok(None);
    }

    Ok(Some(Claim {
        session: job.session,
        user: target.user_id,
        harness: target.harness,
        machine,
    }))
}

/// What went wrong once the job was known to be live.
enum Provisioned {
    /// Nothing is going to change; the session is failed with this reason.
    Failed(String),
    /// A transient failure worth one more attempt.
    Retry(String),
}

impl From<ApiError> for Provisioned {
    fn from(error: ApiError) -> Self {
        // A control-plane failure at this point — D1 refusing a write,
        // a credential that will not unseal — is not something the provider
        // will resolve on the next attempt, so it fails the session and
        // says so rather than looping.
        Self::Failed(error.to_string())
    }
}

/// Mints the credentials, asks the provider for the machine, and records it.
async fn build(
    db: &Db,
    config: &ApiConfig,
    provisioner: &mut impl Provisioner,
    claim: &Claim,
) -> Result<(), Provisioned> {
    let account = provisioning::account(db, config, claim.user, claim.machine.provider_account_id)
        .await
        .map_err(Provisioned::from)?;
    let spec = claim.machine.spec();

    // Re-read the catalog rather than trusting the check made at creation:
    // minutes have passed, quota is shared with everything else in the
    // subscription, and this is also where the price of the capacity
    // actually obtained comes from.
    let entry = provisioning::deployable(&account, &spec)
        .await
        .map_err(|error| classify(&error))?;

    let bootstrap = bootstrap(db, config, claim).await?;
    let machine = provisioner
        .provision(
            &account,
            &ProvisionRequest {
                machine: claim.machine.id,
                spec,
                bootstrap,
            },
        )
        .await
        .map_err(|error| classify(&error))?;

    let hourly = entry.pricing.hourly(machine.capacity_mode.is_spot());
    let storage_hourly = match &entry.pricing {
        flyco_core::MachinePricing::UserOwned => None,
        flyco_core::MachinePricing::Metered { .. } => Some(
            entry
                .pricing
                .storage_hourly(claim.machine.spec().disk_gib)
                .ok_or_else(|| {
                    Provisioned::Failed(format!(
                        "the provider published no storage tier for a {} GiB disk",
                        claim.machine.spec().disk_gib
                    ))
                })?,
        ),
    };
    machines::record(db, &machine, hourly, storage_hourly).await?;

    tracing::info!(
        session = %claim.session,
        machine = %machine.id,
        spot = machine.capacity_mode.is_spot(),
        "provisioned a session's machine; it goes active when its daemon arrives"
    );
    Ok(())
}

/// Builds everything the machine's `flycod` needs to come up already paired
/// with its session.
///
/// The daemon token is minted here rather than at creation because minting
/// replaces whatever the session had: a token issued before a provision that
/// later failed and was retried would be revoked by the retry, and the
/// machine that eventually boots must hold the live one.
async fn bootstrap(
    db: &Db,
    config: &ApiConfig,
    claim: &Claim,
) -> Result<DaemonBootstrap, Provisioned> {
    let token = daemon_tokens::issue(db, claim.user, claim.session)
        .await
        .map_err(Provisioned::from)?;
    let claude_auth =
        harness_accounts::credential(db, &config.token_cipher(), claim.user, claim.harness)
            .await
            .map_err(Provisioned::from)?;

    Ok(DaemonBootstrap {
        session: claim.session,
        control_plane_url: config.control_plane_url(),
        daemon_token: token.token,
        harness: claim.harness,
        // Auto is the product default. Flyco's managed deny rules still bind
        // even in this mode, and anything the classifier does not auto-allow
        // still reaches the approval UI.
        permission_mode: PermissionMode::Auto,
        claude_auth,
        resume_session_id: sessions::harness_session_id(db, claim.session)
            .await
            .map_err(Provisioned::from)?,
    })
}

/// Sorts a provider failure into "try again" and "tell the user".
fn classify(error: &ProviderError) -> Provisioned {
    if error.is_transient() {
        Provisioned::Retry(error.to_string())
    } else {
        Provisioned::Failed(error.to_string())
    }
}

/// Asks for the same machine once more, or gives up and says why.
async fn retry(db: &Db, queue: &Queue, job: ProvisioningJob, reason: &str) -> Settled {
    if job.attempt >= MAX_ATTEMPTS {
        let exhausted =
            format!("gave up after {MAX_ATTEMPTS} attempts to reach the provider: {reason}");
        return match sessions::fail(db, job.session, &exhausted).await {
            Ok(()) => Settled::Done,
            Err(error) => Settled::Redeliver(error),
        };
    }

    let next = job.again();
    tracing::warn!(
        session = %job.session,
        attempt = next.attempt,
        %reason,
        "retrying a provision that could not reach the provider"
    );

    match queue
        .send_with(
            &match serde_json::to_vec(&next) {
                Ok(body) => body,
                Err(error) => return Settled::Redeliver(ApiError::Queue(error.into())),
            },
            SendOptions::new().with_delay(RETRY_DELAY),
        )
        .await
    {
        Ok(()) => Settled::Done,
        // The re-enqueue is the retry; if it did not happen, holding the
        // message is what keeps the session from waiting on a job that no
        // longer exists.
        Err(error) => Settled::Redeliver(ApiError::Queue(error)),
    }
}

/// The Cloudflare `queue` entry point.
///
/// Its own module for one reason: `#[skyzen::queue]` generates the exported
/// `queue` wrapper itself, and there is nowhere to hang a doc comment on a
/// generated item — so the allow is scoped to this module rather than to the
/// whole file.
///
/// Wasm only, because it is the one place in the crate that reads raw
/// Cloudflare bindings: a queue handler is not a request, so
/// `#[skyzen::main]`'s service wiring never reaches it and it opens D1 and
/// its own producer binding itself. Everything it then does is [`consume`],
/// which is target-independent and is what the tests drive.
#[cfg(target_arch = "wasm32")]
mod worker {
    #![allow(
        missing_docs,
        reason = "`#[skyzen::queue]` generates the exported wrapper, docs and all"
    )]

    use skyzen_services::queue::{QueueBatch, QueueBatchDisposition, QueueRetry};
    use skyzen_services::{Db, Queue};

    // `#[wasm_bindgen]` expands an async export into a `future_to_promise`
    // call written unqualified, so the crate it lives in has to be nameable
    // from here. `#[skyzen::main]` gets this for free in the crate root.
    use skyzen::wasm_bindgen_futures;

    use super::{ProvisioningJob, consume};
    use crate::config::{ApiConfig, binding};
    use crate::provisioning::CloudProvisioner;

    #[skyzen::queue]
    async fn provisioning_jobs(
        batch: QueueBatch<ProvisioningJob>,
        env: skyzen::runtime::wasm::Env,
    ) -> QueueBatchDisposition {
        let db = match skyzen_cloudflare::CfD1::from_env(&env, binding::DATABASE) {
            Ok(d1) => Db::new(d1),
            Err(error) => {
                tracing::error!(%error, "the provisioning consumer could not open D1");
                return QueueBatchDisposition::retry_all(QueueRetry::new());
            }
        };
        let queue = match skyzen_cloudflare::CfQueue::from_env(&env, binding::PROVISIONING) {
            Ok(cf) => Queue::new(cf),
            Err(error) => {
                tracing::error!(%error, "the provisioning consumer could not open its own queue");
                return QueueBatchDisposition::retry_all(QueueRetry::new());
            }
        };
        let config = match ApiConfig::from_worker_env(&env) {
            Ok(config) => config,
            Err(error) => {
                tracing::error!(%error, "the provisioning consumer is misconfigured");
                return QueueBatchDisposition::retry_all(QueueRetry::new());
            }
        };

        consume(&db, &config, &queue, &mut CloudProvisioner, batch).await
    }
}
