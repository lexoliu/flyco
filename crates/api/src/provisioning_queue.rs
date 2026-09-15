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
//!
//! # The other thing this queue carries
//!
//! Reading a linked account's catalog is the same kind of work as building
//! a machine — minutes of provider calls that a request must never make —
//! so it runs here too rather than on a queue of its own. See
//! [`crate::catalog`] for what the reads produce and who reads it back.

use core::time::Duration;

use askama::Template;
use flyco_core::{
    BranchName, ClientEvent, CloudProviderKind, ControlToDaemon, HarnessKind, InterruptedReason,
    MachineId, MachineOrigin, ModelChoice, PermissionMode, ProviderAccountId, ProvisioningStage,
    RepoSlug, SessionId, SessionState, UserId,
};
use flyco_provider::{
    Continuation, DaemonBootstrap, GitIdentity, ProviderError, ProvisionRequest, Provisioning,
    RepoCheckout,
};
use serde::{Deserialize, Serialize};
use skyzen_services::queue::{
    QueueBatch, QueueBatchDisposition, QueueMessageDisposition, QueueRetry, SendOptions,
};
use skyzen_services::{Db, Kv, Queue};

use crate::clock::now_unix;
use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::github::{GithubOauth, REPO_SCOPE};
use crate::machines::MachineRow;
use crate::provisioning::Provisioner;
use crate::rooms::Rooms;
use crate::vendors::Vendors;
use crate::{
    budgets, catalog, daemon_tokens, harness_accounts, machines, mcp, provisioning, sessions, users,
};

/// How many times one machine is asked for before the session is failed.
///
/// Three, because the only failures retried here are transport ones, and a
/// destination that refuses three connections spread over a minute and a
/// half is down rather than busy. A fourth attempt would spend another
/// ninety seconds telling the user nothing new.
pub const MAX_ATTEMPTS: u32 = 3;

/// How long a retried job waits before it is delivered again.
const RETRY_DELAY: Duration = Duration::from_secs(30);

/// How long a build the provider handed back waits before it is resumed.
///
/// Short, because the provider is already working and every leg polls
/// under its own subrequest budget: the delay is what keeps the legs from
/// being one invocation, not a pause for the provider's sake.
const CONTINUE_DELAY: Duration = Duration::from_secs(10);

/// One job on the provisioning queue.
///
/// The machine id is carried rather than looked up, and that is what makes a
/// redelivery safe: every provider-native resource name is derived from it,
/// so two deliveries of one job address one machine. A job naming a machine
/// the session no longer has is a job from a superseded attempt, and is
/// dropped.
///
/// Two variants for compute, because there are two ways a session gets onto
/// it and they are not the same operation. A [`Provision`](Self::Provision)
/// builds a machine that does not exist. A [`Recover`](Self::Recover) starts
/// one that does — the disk is still there, with the checkout and the caches
/// on it, and the whole point is to keep it — so it must never take the path
/// that asks a provider for capacity.
///
/// The other two read a linked account's catalog into
/// [`crate::catalog`]'s cache. They name no session and no machine, which is
/// why [`session`](Self::session) and [`machine`](Self::machine) answer with
/// an [`Option`]: a catalog refresh is work for an *account*, and the
/// session-shaped bookkeeping around a provision — the claim, the attempt
/// counter, failing the session — is exactly what it must not go through.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "job", rename_all = "snake_case")]
pub enum ProvisioningJob {
    /// Build a session's machine.
    Provision {
        /// The session whose machine this builds.
        session: SessionId,
        /// The reserved `machines` row it fills in.
        machine: MachineId,
        /// Which attempt this is, counting from one.
        attempt: u32,
    },
    /// Carry on a build the provider handed back (issue #257).
    ///
    /// Its own job rather than a `Provision` delivered again, because a
    /// second `Provision` would start a second machine: the continuation
    /// names the one already being built.
    Continue {
        /// The session the machine is for.
        session: SessionId,
        /// The machine the continuation belongs to.
        machine: MachineId,
        /// How many times this leg has been asked for; counts like a
        /// provision's, and gives up the same way.
        attempt: u32,
        /// Where the provider's driver got to.
        continuation: Continuation,
    },
    /// Put a session back on the machine a provider reclaimed.
    ///
    /// Enqueued when the session's own daemon reports a spot notice, with a
    /// delivery delay covering the seconds the provider said were left: the
    /// machine is still running for that long, and a start issued against a
    /// running instance is not a restart.
    ///
    /// It **starts the same machine on the same disk** and never provisions
    /// a second one. Every provider flyco puts spot capacity on is
    /// configured to stop rather than delete — Azure deallocates, an AWS
    /// persistent spot request stops, Compute Engine is created with
    /// `instanceTerminationAction: STOP` — so the disk that survived is the
    /// session's working tree, exactly as its agent left it.
    Recover {
        /// The session being put back.
        session: SessionId,
        /// The machine to start again, on its own disk.
        machine: MachineId,
        /// Which attempt this is, counting from one.
        attempt: u32,
        /// Why the session is off its machine, which is everything the
        /// recovery does differently between the two.
        cause: RecoveryCause,
        /// When the session came off its machine.
        ///
        /// Carried rather than read from the clock so that whatever this job
        /// writes is the same on every redelivery: an at-least-once queue
        /// delivers a recovery twice, and the second delivery must not bill
        /// a replacement a second time.
        since_unix: u64,
    },
    /// Read one linked account's catalog into the cache.
    ///
    /// The fan-out: it asks the provider how its catalog is shaped and
    /// either reads the whole account in this message or enqueues one
    /// [`RefreshCatalogRegion`](Self::RefreshCatalogRegion) per region. The
    /// shape is the provider's answer rather than a constant, because an
    /// Azure subscription's regions are its own allowed-regions policy.
    RefreshCatalog {
        /// Who owns the account, which is what unseals its credentials.
        user: UserId,
        /// The account to read.
        account: ProviderAccountId,
    },
    /// Read one region of one linked account's catalog into the cache.
    ///
    /// One region per message because a region is what fits: a full SKU
    /// list, its quotas and a page-walk of retail prices took twenty to
    /// thirty seconds each against a live subscription, and three of those
    /// in one invocation is what exceeded the Worker's CPU limit.
    RefreshCatalogRegion {
        /// Who owns the account.
        user: UserId,
        /// The account to read.
        account: ProviderAccountId,
        /// The provider-native region this message covers.
        region: String,
    },
}

/// Why a session is off the machine a recovery is putting it back on.
///
/// The ways a session comes off a machine it still owns are a provider
/// taking its spot capacity, flyco releasing it to wait out a spent
/// harness plan window, a provider suspending it for idleness, and the
/// user simply asking for it back. *Starting the machine again is the same
/// operation for all of them* — same row, same disk, same provider-native
/// names — so they share the job and this is the whole of what differs:
/// what the recovery bills, and what it says to the agent when it lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryCause {
    /// The provider reclaimed the session's interruptible capacity.
    ///
    /// The gap is billed as a named zero-amount ledger entry and the agent
    /// is told its machine was replaced, because from the conversation's
    /// point of view something happened *to* it that it did not ask for.
    SpotReclaimed,
    /// The session released its own machine to wait out a spent plan
    /// window, and the window is about to turn over (issue #244).
    ///
    /// Nothing is billed — there is no replacement, the session simply
    /// stopped paying for a while — and nothing is said to the agent here.
    /// What it hears is the continuation [`crate::usage_limits`] sends when
    /// the window actually resets, which is a minute or ten later and is the
    /// message that matters.
    UsageLimit,
    /// The provider suspended the machine for idleness, keeping its disk.
    ///
    /// A codespace GitHub stopped on its own idle clock — the reconcile
    /// recorded it and the first thing to speak to the session enqueued
    /// this. Nothing is billed: the compute meter stopped at the suspend
    /// and restarts at the start, the storage meter never paused, and a
    /// routine suspend is not the kind of gap the ledger keeps a line for.
    /// The agent *is* told, because everything it had running died with the
    /// machine.
    Suspended,
    /// The user asked for the session back.
    ///
    /// `POST /v1/sessions/{id}/resume` on a machine the provider still
    /// held — a suspended codespace resumed by hand, say. Nothing is
    /// billed and nothing is said: the person who clicked it watched it
    /// happen.
    Resumed,
}

/// Why a session is off its machine, and since when.
///
/// One argument rather than two, because they are one fact and every use
/// reads them together: what the recovery bills is decided by the cause, and
/// the instant it bills *from* is the same instant the cause names.
#[derive(Debug, Clone, Copy)]
struct Interruption {
    /// What took the session off its machine.
    cause: RecoveryCause,
    /// When that happened, seconds since the Unix epoch.
    since_unix: u64,
}

impl ProvisioningJob {
    /// The first attempt at a session's machine.
    #[must_use]
    pub const fn first(session: SessionId, machine: MachineId) -> Self {
        Self::Provision {
            session,
            machine,
            attempt: 1,
        }
    }

    /// The first attempt at putting a reclaimed session back.
    #[must_use]
    pub const fn continuing(
        session: SessionId,
        machine: MachineId,
        continuation: Continuation,
    ) -> Self {
        Self::Continue {
            session,
            machine,
            attempt: 1,
            continuation,
        }
    }

    /// The first attempt at putting a reclaimed session back.
    #[must_use]
    pub const fn recovery(session: SessionId, machine: MachineId, reclaimed_at_unix: u64) -> Self {
        Self::Recover {
            session,
            machine,
            attempt: 1,
            cause: RecoveryCause::SpotReclaimed,
            since_unix: reclaimed_at_unix,
        }
    }

    /// The first attempt at starting a machine a session released itself to
    /// wait out a spent harness plan window (issue #244).
    ///
    /// The same operation as a recovery from a reclamation — start this
    /// machine, on this disk — and it goes through the same job for that
    /// reason. What differs is only what the recovery says about itself, and
    /// [`RecoveryCause`] is the whole of that difference.
    #[must_use]
    pub const fn waking(session: SessionId, machine: MachineId, at_unix: u64) -> Self {
        Self::Recover {
            session,
            machine,
            attempt: 1,
            cause: RecoveryCause::UsageLimit,
            since_unix: at_unix,
        }
    }

    /// The first attempt at starting a machine the provider suspended for
    /// idleness.
    ///
    /// A suspended codespace is woken by the first thing that speaks to its
    /// session — a message, a resume click — and this is the job that thing
    /// enqueues. Same disk, same start as every other recovery.
    #[must_use]
    pub const fn resuming(session: SessionId, machine: MachineId, at_unix: u64) -> Self {
        Self::Recover {
            session,
            machine,
            attempt: 1,
            cause: RecoveryCause::Suspended,
            since_unix: at_unix,
        }
    }

    /// The first attempt at starting a machine the user asked for back.
    ///
    /// `POST /v1/sessions/{id}/resume` on a machine the provider still
    /// holds: the same start as every recovery, said nothing about and
    /// billed nothing for, because the person who clicked it watched it
    /// happen.
    #[must_use]
    pub const fn resumed(session: SessionId, machine: MachineId, at_unix: u64) -> Self {
        Self::Recover {
            session,
            machine,
            attempt: 1,
            cause: RecoveryCause::Resumed,
            since_unix: at_unix,
        }
    }

    /// The session this job is for, when it is for one at all.
    #[must_use]
    pub const fn session(&self) -> Option<SessionId> {
        match self {
            Self::Provision { session, .. }
            | Self::Continue { session, .. }
            | Self::Recover { session, .. } => Some(*session),
            Self::RefreshCatalog { .. } | Self::RefreshCatalogRegion { .. } => None,
        }
    }

    /// The machine row this job is allowed to act on, when it acts on one.
    #[must_use]
    pub const fn machine(&self) -> Option<MachineId> {
        match self {
            Self::Provision { machine, .. }
            | Self::Continue { machine, .. }
            | Self::Recover { machine, .. } => Some(*machine),
            Self::RefreshCatalog { .. } | Self::RefreshCatalogRegion { .. } => None,
        }
    }

    /// Which attempt this delivery is, counting from one.
    ///
    /// Only a job that can fail a session counts attempts. A catalog
    /// refresh records its own failure in the document instead, and the
    /// scheduled sweep is what asks again — see
    /// [`crate::catalog::FAILURE_TTL_SECONDS`].
    #[must_use]
    pub const fn attempt(&self) -> Option<u32> {
        match self {
            Self::Provision { attempt, .. }
            | Self::Continue { attempt, .. }
            | Self::Recover { attempt, .. } => Some(*attempt),
            Self::RefreshCatalog { .. } | Self::RefreshCatalogRegion { .. } => None,
        }
    }

    /// The same job, asked for once more.
    fn again(self) -> Self {
        match self {
            Self::Provision {
                session,
                machine,
                attempt,
            } => Self::Provision {
                session,
                machine,
                attempt: attempt.saturating_add(1),
            },
            Self::Continue {
                session,
                machine,
                attempt,
                continuation,
            } => Self::Continue {
                session,
                machine,
                attempt: attempt.saturating_add(1),
                continuation,
            },
            Self::Recover {
                session,
                machine,
                attempt,
                cause,
                since_unix,
            } => Self::Recover {
                session,
                machine,
                attempt: attempt.saturating_add(1),
                cause,
                since_unix,
            },
            // A catalog refresh has no attempt to raise: nothing retries it
            // here, and asking for it again is the scheduled sweep's job.
            job @ (Self::RefreshCatalog { .. } | Self::RefreshCatalogRegion { .. }) => job,
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
    tracing::info!(job = ?job, "queued a provisioning job");
    Ok(())
}

/// Puts a job on the queue for a machine that is still, briefly, running.
///
/// The delay is the provider's own countdown: the machine holds its disk
/// for that long, and a start issued against a running instance is not a
/// restart. Cloudflare Queues delivers on or after the delay rather than
/// exactly at it, which is the right direction to be wrong in — a
/// recovery that runs a few seconds late finds a stopped machine, and one
/// that ran early would find a live one.
///
/// # Errors
///
/// Returns [`ApiError::Queue`] if the queue refuses the message, which
/// leaves the session interrupted with nothing coming to recover it — so
/// the caller must treat it as a failure rather than as a delay.
pub async fn enqueue_after(
    queue: &Queue,
    job: ProvisioningJob,
    delay: Duration,
) -> Result<(), ApiError> {
    let body = serde_json::to_vec(&job).map_err(|error| ApiError::Queue(error.into()))?;
    queue
        .send_with(&body, SendOptions::new().with_delay(delay))
        .await
        .map_err(ApiError::Queue)?;
    tracing::info!(
        job = ?job,
        delay_secs = delay.as_secs(),
        "queued a provisioning job for later"
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

/// The three services a provisioning job reaches outside its own database.
///
/// One argument rather than three, because they arrive and travel together
/// through every step of a job: the provider builds the machine, the
/// harness vendor keeps the credential fresh, and GitHub says who the user
/// is, what their token may do, and where the repository's default branch
/// points. A call site that swapped two of them would still compile.
#[derive(Debug)]
pub struct Clients<'a, P: Provisioner, G: GithubOauth> {
    /// Builds the machine.
    ///
    /// `&mut` because a driver is stateful: it caches an access token it
    /// must be able to replace.
    pub provisioner: &'a mut P,
    /// Renews a subscription grant that is near its end, either vendor's.
    pub vendors: &'a Vendors,
    /// Reads the user's account, what their token may do, and the
    /// repository's default branch.
    pub github: &'a G,
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
    kv: &Kv,
    queue: &Queue,
    rooms: &Rooms,
    clients: &mut Clients<'_, impl Provisioner, impl GithubOauth>,
    batch: QueueBatch<ProvisioningJob>,
) -> QueueBatchDisposition {
    let mut decisions = Vec::with_capacity(batch.messages.len());
    for message in batch.messages {
        let job = message.body.clone();
        decisions.push(
            match perform(db, config, kv, queue, rooms, clients, message.body).await {
                Settled::Done => QueueMessageDisposition::Ack,
                Settled::Redeliver(error) => {
                    tracing::error!(
                        job = ?job,
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
    kv: &Kv,
    queue: &Queue,
    rooms: &Rooms,
    clients: &mut Clients<'_, impl Provisioner, impl GithubOauth>,
    job: ProvisioningJob,
) -> Settled {
    // A catalog refresh is decided entirely by itself: it never claims a
    // machine row, never counts attempts, and never fails a session.
    match &job {
        ProvisioningJob::RefreshCatalog { user, account } => {
            return settle(refresh_catalog(db, config, kv, queue, *user, *account).await);
        }
        ProvisioningJob::RefreshCatalogRegion {
            user,
            account,
            region,
        } => {
            return settle(refresh_region(db, config, kv, *user, *account, region).await);
        }
        ProvisioningJob::Provision { .. }
        | ProvisioningJob::Continue { .. }
        | ProvisioningJob::Recover { .. } => {}
    }

    let outcome = match job {
        ProvisioningJob::Provision { .. } => match claim(db, rooms, job.clone()).await {
            Ok(None) => return Settled::Done,
            Ok(Some(claimed)) => build(db, config, kv, rooms, queue, clients, &claimed).await,
            Err(error) => return Settled::Redeliver(error),
        },
        ProvisioningJob::Continue {
            ref continuation, ..
        } => match claim(db, rooms, job.clone()).await {
            Ok(None) => return Settled::Done,
            Ok(Some(claimed)) => {
                carry_on(
                    db,
                    config,
                    kv,
                    rooms,
                    queue,
                    clients,
                    &claimed,
                    continuation,
                )
                .await
            }
            Err(error) => return Settled::Redeliver(error),
        },
        ProvisioningJob::Recover {
            session,
            machine,
            cause,
            since_unix,
            ..
        } => match recover(
            db,
            config,
            rooms,
            queue,
            clients,
            Recovery {
                session,
                machine,
                interruption: Interruption { cause, since_unix },
            },
        )
        .await
        {
            Ok(Recovered::Done) => return Settled::Done,
            Ok(Recovered::Ran) => Ok(()),
            Err(failure) => Err(failure),
        },
        // Answered above, before a machine job's bookkeeping was reached.
        ProvisioningJob::RefreshCatalog { .. } | ProvisioningJob::RefreshCatalogRegion { .. } => {
            return Settled::Done;
        }
    };

    // Unreachable: every arm above that produces an outcome is a job that
    // names a session. Written as a disposition rather than an `expect`
    // because a panic here kills the consumer for the whole batch.
    let Some(session) = job.session() else {
        return Settled::Done;
    };
    match outcome {
        Ok(()) => Settled::Done,
        Err(Provisioned::Failed(reason)) => {
            match sessions::fail(db, rooms, session, &reason).await {
                Ok(()) => Settled::Done,
                Err(error) => Settled::Redeliver(error),
            }
        }
        Err(Provisioned::Retry(reason)) => retry(db, queue, rooms, job, &reason).await,
    }
}

/// A catalog refresh's disposition: it either wrote what it learned, or the
/// store was unreachable and nothing was recorded.
///
/// Everything a *provider* said is written into the document, failures
/// included, so only a flyco-side failure holds the message. A refresh that
/// keeps failing therefore stops asking rather than looping.
fn settle(outcome: Result<(), ApiError>) -> Settled {
    match outcome {
        Ok(()) => Settled::Done,
        Err(error) => Settled::Redeliver(error),
    }
}

/// Reads one account's catalog, or fans it out a region at a time.
///
/// Everything the provider says — including a refusal — ends up in the
/// account's document, because "read, and it refused" is an answer the
/// request path can serve and "not read yet" is not. Only a store failure
/// is held for redelivery.
async fn refresh_catalog(
    db: &Db,
    config: &ApiConfig,
    kv: &Kv,
    queue: &Queue,
    user: UserId,
    account: ProviderAccountId,
) -> Result<(), ApiError> {
    let linked = match provisioning::account(db, config, user, account).await {
        Ok(linked) => linked,
        // Unlinked between the ask and the read. There is nothing to write
        // and nothing to keep: the document expires on its own.
        Err(ApiError::ProviderAccountNotFound) => {
            tracing::info!(%account, "dropping a catalog refresh for an account that is gone");
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if !linked.catalog_is_remote() {
        // A host's catalog is never cached, so there is nothing here to
        // write; the account was enqueued by something that should not have.
        tracing::warn!(%account, "ignoring a catalog refresh for a machine the user owns");
        return Ok(());
    }

    let at_unix = now_unix();
    let reads = match provisioning::catalog_reads(&linked).await {
        Ok(reads) => reads,
        Err(error) => {
            tracing::warn!(%account, %error, "an account could not say how to read its catalog");
            return catalog::record_failure(kv, account, error.to_string(), at_unix).await;
        }
    };

    match reads {
        provisioning::CatalogReads::PerRegion(regions) if !regions.is_empty() => {
            for region in regions {
                enqueue(
                    queue,
                    ProvisioningJob::RefreshCatalogRegion {
                        user,
                        account,
                        region,
                    },
                )
                .await?;
            }
            Ok(())
        }
        // A subscription whose policy allows no region flyco can read is a
        // complete answer, not a pending one: without a document written
        // here the account would stay pending for ever.
        provisioning::CatalogReads::PerRegion(_) => {
            tracing::info!(%account, "an account's policy leaves no region to read");
            catalog::record_account(kv, account, Vec::new(), at_unix).await
        }
        provisioning::CatalogReads::Whole => match provisioning::catalog(&linked).await {
            Ok(entries) => {
                tracing::info!(%account, offered = entries.len(), "read an account's catalog");
                catalog::record_account(kv, account, entries, at_unix).await
            }
            Err(error) => {
                tracing::warn!(%account, %error, "an account's catalog could not be read");
                catalog::record_failure(kv, account, error.to_string(), at_unix).await
            }
        },
    }
}

/// Reads exactly one region of one account's catalog into the document.
async fn refresh_region(
    db: &Db,
    config: &ApiConfig,
    kv: &Kv,
    user: UserId,
    account: ProviderAccountId,
    region: &str,
) -> Result<(), ApiError> {
    let linked = match provisioning::account(db, config, user, account).await {
        Ok(linked) => linked,
        Err(ApiError::ProviderAccountNotFound) => {
            tracing::info!(%account, "dropping a catalog refresh for an account that is gone");
            return Ok(());
        }
        Err(error) => return Err(error),
    };

    let outcome = match provisioning::region_catalog(&linked, region).await {
        Ok(entries) => {
            tracing::info!(%account, %region, offered = entries.len(), "read a region's catalog");
            catalog::RegionOutcome::Offered { entries }
        }
        // One region, one hole: a region the provider refuses must not take
        // the regions beside it off the menu.
        Err(error) => {
            tracing::warn!(%account, %region, %error, "a region's catalog could not be read");
            catalog::RegionOutcome::Failed {
                error: error.to_string(),
            }
        }
    };

    catalog::record_region(
        kv,
        account,
        catalog::RegionCatalog {
            region: region.to_owned(),
            read_at_unix: now_unix(),
            outcome,
        },
    )
    .await
}

/// A session and the machine row this job is allowed to fill in.
struct Claim {
    session: SessionId,
    user: UserId,
    harness: HarnessKind,
    repo: RepoSlug,
    /// The branch, when the session already records one. `None` is a
    /// session opened before flyco recorded branches; [`checkout`] resolves
    /// the repository's default and writes it back.
    branch: Option<BranchName>,
    machine_origin: MachineOrigin,
    machine: MachineRow,
    /// What the session runs on, as the control plane records it.
    ///
    /// Carried on the claim rather than re-read in [`bootstrap`], so the
    /// model written into the machine's configuration is the one this job
    /// read the session at.
    model: ModelChoice,
    /// The mode the session runs under, on the same terms.
    permission_mode: PermissionMode,
}

/// Decides whether this delivery still has work to do.
///
/// `None` means the job is spent — the session is gone, it is no longer
/// waiting for a machine, the job names a machine that was superseded, or
/// the machine already exists — and the message is acknowledged rather than
/// redelivered into the same answer.
async fn claim(db: &Db, rooms: &Rooms, job: ProvisioningJob) -> Result<Option<Claim>, ApiError> {
    // Only a job that names a session and a machine reaches here; both
    // reads are refusals rather than unwraps so a panic cannot take the
    // batch down over a shape the type already rules out.
    let (Some(session), Some(wanted)) = (job.session(), job.machine()) else {
        return Ok(None);
    };
    let Some(target) = sessions::provisioning_target(db, session).await? else {
        tracing::info!(%session, "dropping a job for a session that no longer exists");
        return Ok(None);
    };
    if target.state != SessionState::Provisioning {
        tracing::info!(
            %session,
            state = ?target.state,
            "dropping a job for a session that is no longer waiting for a machine"
        );
        return Ok(None);
    }

    let Some(machine) = machines::for_session(db, session).await? else {
        // Creation reserves the row before it enqueues, so there is no
        // ordering in which this is a race. It is a session that was written
        // by something other than `POST /v1/sessions`.
        sessions::fail(
            db,
            rooms,
            session,
            "this session has no machine row to fill in",
        )
        .await?;
        return Ok(None);
    };
    if machine.id != wanted {
        tracing::info!(
            %session,
            job = %wanted,
            machine = %machine.id,
            "dropping a job superseded by a later attempt"
        );
        return Ok(None);
    }
    if machine.is_provisioned() {
        tracing::info!(
            %session,
            machine = %machine.id,
            "this session's machine already exists; the job was delivered twice"
        );
        return Ok(None);
    }
    // A row with a provider-native id but no running machine is a build the
    // provider handed back: a `Continue` owns it, and a `Provision`
    // delivered again would start a second machine beside it.
    let pending = machine.native_id.is_some();
    match &job {
        ProvisioningJob::Provision { .. } if pending => {
            tracing::info!(
                %session,
                machine = %machine.id,
                "this session's machine is being built by a continuation; the job was delivered twice"
            );
            return Ok(None);
        }
        ProvisioningJob::Continue { .. } if !pending => {
            tracing::info!(
                %session,
                machine = %machine.id,
                "dropping a continuation for a build the provider never handed back"
            );
            return Ok(None);
        }
        _ => {}
    }

    let model = target.model_choice();
    let permission_mode = target.permission_mode();
    Ok(Some(Claim {
        session,
        user: target.user_id,
        harness: target.harness,
        repo: target.repo,
        branch: target.branch,
        machine_origin: target.machine_origin,
        machine,
        model,
        permission_mode,
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

/// The catalog entry a machine is being provisioned under, resolved the way
/// the picker's document answers it rather than by asking the provider
/// again.
///
/// The cached document is the authority here for the same reason
/// [`machines::deployable`] reads it at creation: it is the catalog the user
/// chose from, so it is also where the price of the capacity actually
/// obtained comes from. Asking the provider instead cost every provision —
/// and every continuation leg — the SKU list, the quota read and the
/// page-walk of retail prices, most of which a container never uses; on the
/// measured path that was the whole delay between a session's creation and
/// its `reserving` stage.
///
/// A remote account whose document cannot price this machine is read *on
/// demand*: just the region the machine is in for a provider read a region
/// at a time, the whole catalog for one that answers in a single pass — and
/// what the read learns is recorded back so the next leg and the picker
/// both see it. A host's catalog is row-sourced and never cached, so it
/// keeps the direct read.
async fn deployable_entry(
    kv: &Kv,
    queue: &Queue,
    claim: &Claim,
    account: &provisioning::LinkedAccount,
    spec: &flyco_core::MachineSpec,
) -> Result<flyco_core::MachineCatalogEntry, Provisioned> {
    if !account.catalog_is_remote() {
        return provisioning::deployable(account, spec)
            .await
            .map_err(|error| classify(&error));
    }

    let wanted = |entry: &&flyco_core::MachineCatalogEntry| {
        entry.machine_type == spec.machine_type
            && entry.region.eq_ignore_ascii_case(&spec.region)
            && entry.runtime == spec.runtime
    };

    match catalog::read(kv, account.id).await {
        Ok(Some(document)) => {
            if let Some(entry) = document.entries().find(wanted).cloned() {
                // Served stale, refreshed anyway — the same answer the
                // picker gives while a refresh is under way.
                if document.is_stale(now_unix())
                    && let Err(error) =
                        catalog::ask_for_refresh(kv, queue, claim.user, account.id).await
                {
                    tracing::warn!(account = %account.id, %error, "could not ask for a catalog refresh");
                }
                return Ok(entry);
            }
        }
        Ok(None) => {}
        // A store that cannot be read is not a reason to keep the provider
        // from being asked: the cache is an optimisation, not a dependency.
        Err(error) => {
            tracing::warn!(account = %account.id, %error, "the catalog cache could not be read");
        }
    }

    let entries = match provisioning::catalog_reads(account)
        .await
        .map_err(|error| classify(&error))?
    {
        provisioning::CatalogReads::PerRegion(_) => {
            match provisioning::region_catalog(account, &spec.region).await {
                Ok(entries) => {
                    if let Err(error) = catalog::record_region(
                        kv,
                        account.id,
                        catalog::RegionCatalog {
                            region: spec.region.clone(),
                            read_at_unix: now_unix(),
                            outcome: catalog::RegionOutcome::Offered {
                                entries: entries.clone(),
                            },
                        },
                    )
                    .await
                    {
                        tracing::warn!(account = %account.id, %error, "a region read could not be recorded");
                    }
                    entries
                }
                Err(error) => {
                    if let Err(recorded) = catalog::record_region(
                        kv,
                        account.id,
                        catalog::RegionCatalog {
                            region: spec.region.clone(),
                            read_at_unix: now_unix(),
                            outcome: catalog::RegionOutcome::Failed {
                                error: error.to_string(),
                            },
                        },
                    )
                    .await
                    {
                        tracing::warn!(account = %account.id, %recorded, "a region's failure could not be recorded");
                    }
                    return Err(classify(&error));
                }
            }
        }
        provisioning::CatalogReads::Whole => match provisioning::catalog(account).await {
            Ok(entries) => {
                if let Err(error) =
                    catalog::record_account(kv, account.id, entries.clone(), now_unix()).await
                {
                    tracing::warn!(account = %account.id, %error, "an account read could not be recorded");
                }
                entries
            }
            Err(error) => {
                if let Err(recorded) =
                    catalog::record_failure(kv, account.id, error.to_string(), now_unix()).await
                {
                    tracing::warn!(account = %account.id, %recorded, "an account's failure could not be recorded");
                }
                return Err(classify(&error));
            }
        },
    };

    entries.iter().find(wanted).cloned().ok_or_else(|| {
        Provisioned::Failed(format!(
            "{} in {} is not something this account can deploy",
            spec.machine_type, spec.region
        ))
    })
}

/// Mints the credentials, asks the provider for the machine, and records it.
async fn build(
    db: &Db,
    config: &ApiConfig,
    kv: &Kv,
    rooms: &Rooms,
    queue: &Queue,
    clients: &mut Clients<'_, impl Provisioner, impl GithubOauth>,
    claim: &Claim,
) -> Result<(), Provisioned> {
    let account = provisioning::account(db, config, claim.user, claim.machine.provider_account_id)
        .await
        .map_err(Provisioned::from)?;
    let spec = claim.machine.spec();

    let entry = deployable_entry(kv, queue, claim, &account, &spec).await?;

    let bootstrap = bootstrap(db, config, clients, claim, &entry, spec.spot).await?;

    // A codespace has no user-data channel — GitHub offers no
    // per-codespace secret, and a repository-level one is shared by every
    // concurrent session — so what its `postStart` fetches is sealed onto
    // the machine row *before* the provider is asked: the machine may boot
    // and call `codespaces::bootstrap` before the provision has even
    // answered, and the row it finds by name must already be holding the
    // document it is there for.
    if entry.provider == CloudProviderKind::Codespaces {
        let sealed = config
            .token_cipher()
            .seal(
                &flyco_provider::flycod::render(&bootstrap).map_err(|error| {
                    Provisioned::Failed(format!(
                        "the daemon configuration cannot be rendered: {error}"
                    ))
                })?,
            )
            .map_err(|error| Provisioned::from(ApiError::from(error)))?;
        machines::store_bootstrap(db, claim.machine.id, &sealed)
            .await
            .map_err(Provisioned::from)?;
    }

    announce(db, rooms, claim.session, ProvisioningStage::Reserving).await;
    let outcome = clients
        .provisioner
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
    settle_build(db, rooms, queue, claim, &entry, outcome).await
}

/// Carries on a build the provider handed back (issue #257).
///
/// The same ending as [`build`], reached without a new bootstrap or a new
/// `reserving` stage: the machine is the one already being built, and the
/// user has been watching it since the first leg.
#[expect(
    clippy::too_many_arguments,
    reason = "a continuation names the claim, the continuation token, and \
              every service the settle touches"
)]
async fn carry_on(
    db: &Db,
    config: &ApiConfig,
    kv: &Kv,
    rooms: &Rooms,
    queue: &Queue,
    clients: &mut Clients<'_, impl Provisioner, impl GithubOauth>,
    claim: &Claim,
    continuation: &Continuation,
) -> Result<(), Provisioned> {
    let account = provisioning::account(db, config, claim.user, claim.machine.provider_account_id)
        .await
        .map_err(Provisioned::from)?;
    let entry = deployable_entry(kv, queue, claim, &account, &claim.machine.spec()).await?;
    let pending = claim
        .machine
        .as_provider_machine()
        .map_err(Provisioned::from)?;

    // Progress, as far as the stall sweep is concerned: the provider is
    // still working, and a session it is working on is not stalled.
    sessions::note_progress(db, claim.session)
        .await
        .map_err(Provisioned::from)?;
    let outcome = clients
        .provisioner
        .resume(&account, &pending, continuation)
        .await
        .map_err(|error| classify(&error))?;
    settle_build(db, rooms, queue, claim, &entry, outcome).await
}

/// Records what a provision or a resumed build answered.
///
/// A machine is recorded and the session told it is booting; a build still
/// in progress is recorded by what exists of it and asked for again after
/// [`CONTINUE_DELAY`], so the next leg polls under its own budget.
async fn settle_build(
    db: &Db,
    rooms: &Rooms,
    queue: &Queue,
    claim: &Claim,
    entry: &flyco_core::MachineCatalogEntry,
    outcome: Provisioning,
) -> Result<(), Provisioned> {
    let machine = match outcome {
        Provisioning::Ready(machine) => machine,
        Provisioning::Pending {
            machine,
            continuation,
        } => {
            machines::record_pending(db, &machine).await?;
            enqueue_after(
                queue,
                ProvisioningJob::continuing(claim.session, machine.id, continuation),
                CONTINUE_DELAY,
            )
            .await?;
            tracing::info!(
                session = %claim.session,
                machine = %machine.id,
                "the provider is still building a session's machine; the build carries on \
                 in its next leg"
            );
            return Ok(());
        }
    };
    // What the machine turned out to be, priced at the capacity it actually
    // holds — which is what the budget meters and what the agent's
    // `machine_status` reads back.
    let built = flyco_core::SessionMachine::of(entry, machine.capacity_mode.is_spot());
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
    machines::record(db, &machine, &built, storage_hourly).await?;
    // Recorded, so the machine is durably flyco's and is powering on; what
    // happens on it next is its bootstrap fetching and running the `flycod`
    // installer. That is the last thing the control plane can see —
    // everything after it is announced by the daemon on the machine itself,
    // which is why boot and install are one stage rather than two.
    announce(db, rooms, claim.session, ProvisioningStage::Booting).await;

    tracing::info!(
        session = %claim.session,
        machine = %machine.id,
        spot = machine.capacity_mode.is_spot(),
        "provisioned a session's machine; it goes active when its daemon arrives"
    );
    Ok(())
}

/// What the agent is told once its session is running again.
///
/// The only thing the agent is ever told about a reclamation, and it
/// arrives *afterwards*: the thirty seconds of the notice were spent by
/// flyco, and an LLM asked to take part in them would still have been
/// thinking when the machine went. A rendered template rather than an
/// assembled string, like every other sentence flyco says to an agent —
/// the wording is the contract, and a dropped field should be a compile
/// error rather than a notice with a hole in it.
#[derive(Debug, Template)]
#[template(path = "spot/machine_replaced.txt", escape = "none")]
struct MachineReplaced {
    /// The provider-native type the session is running on now.
    machine_type: String,
}

/// What the agent is told once a suspended machine is running again.
///
/// The suspension counterpart of [`MachineReplaced`]: a codespace that
/// idled out under GitHub's own timer kept its disk, so the checkout and
/// the caches survived exactly — and every process on it did not, which is
/// the one fact the agent needs before it carries on.
#[derive(Debug, Template)]
#[template(path = "codespaces/machine_suspended.txt", escape = "none")]
struct MachineSuspended {
    /// The provider-native type the session is running on now.
    machine_type: String,
}

/// A `Recover` job's identity: which session, which machine row, and what
/// provoked it.
struct Recovery {
    /// The session being put back.
    session: SessionId,
    /// The machine row the start must land on — any other is a superseded
    /// attempt and the job is spent.
    machine: MachineId,
    /// Why the machine was gone and when it went.
    interruption: Interruption,
}

/// Whether a recovery had anything to do.
enum Recovered {
    /// It ran: the machine was started again.
    Ran,
    /// There was nothing to recover — the session is gone, archived, or
    /// already back on a machine — and the message is spent.
    Done,
}

/// The machine row a recovery is cleared to start, or none.
///
/// `None` is the spent cases: the session is gone, no longer holds an
/// environment, is already live, or its machine row is a different or
/// nameless one. The nameless case is the sweep having already released the
/// row — there is nothing to start — and the session is marked
/// `machine_lost` for the wake that provisions.
///
/// # Errors
///
/// Returns [`Provisioned`] if a read or the `machine_lost` write fails.
async fn recoverable(
    db: &Db,
    session: SessionId,
    machine: MachineId,
) -> Result<Option<(sessions::ProvisioningTarget, MachineRow)>, Provisioned> {
    let Some(target) = sessions::provisioning_target(db, session)
        .await
        .map_err(Provisioned::from)?
    else {
        tracing::info!(%session, "dropping a recovery for a session that no longer exists");
        return Ok(None);
    };
    if !target.state.holds_environment() {
        tracing::info!(
            %session,
            state = ?target.state,
            "dropping a recovery for a session that no longer holds a machine"
        );
        return Ok(None);
    }
    if target.state == SessionState::Active {
        // Back on a machine already — a suspended codespace somebody opened
        // on github.com, say, whose daemon attached while this job was in
        // flight. `Recover` is the one job that does not first claim its
        // session is `provisioning`, so this is the check that keeps a late
        // delivery from restarting a live machine.
        tracing::info!(%session, "dropping a recovery for a session that is already live");
        return Ok(None);
    }

    let Some(row) = machines::for_session(db, session)
        .await
        .map_err(Provisioned::from)?
    else {
        return Err(Provisioned::Failed(
            "this session has no machine row to recover".to_owned(),
        ));
    };
    if row.id != machine {
        tracing::info!(
            %session,
            job = %machine,
            machine = %row.id,
            "dropping a recovery superseded by a later machine"
        );
        return Ok(None);
    }
    if row.native_id.is_none() {
        // Whatever took the name off this row — the reconcile's
        // `mark_lost` — already decided there is no machine to start, and
        // a start against a name that answers 404 is a retry forever. The
        // session is marked `machine_lost`; what speaks to it next
        // provisions.
        sessions::machine_lost(db, session)
            .await
            .map_err(Provisioned::from)?;
        return Ok(None);
    }
    Ok(Some((target, row)))
}

/// Releases the row of a machine the provider's own start reported as gone,
/// and queues the provision there is no longer anything to recover into.
///
/// The start is the call that learned it — a codespace deleted between the
/// last reconcile and now — and the disk went with it. This job exists only
/// where somebody was waiting on the session, and "interrupted until the
/// next message" is not an answer to the message that just arrived.
///
/// # Errors
///
/// Returns [`Provisioned`] if the row release or the enqueue fails.
async fn reprovision_gone(
    db: &Db,
    queue: &Queue,
    session: SessionId,
    row: &MachineRow,
    reason: &str,
) -> Result<Recovered, Provisioned> {
    machines::mark_lost(db, row.id)
        .await
        .map_err(Provisioned::from)?;
    // The session is `provisioning` — `recovering` ran before the start —
    // so this writes only the reason, which is what keeps the UI reading
    // `Migrating · machine lost` rather than suspending.
    sessions::interrupted(db, session, InterruptedReason::MachineLost)
        .await
        .map_err(Provisioned::from)?;
    machines::reset_for_resume(db, session)
        .await
        .map_err(Provisioned::from)?;
    enqueue(queue, ProvisioningJob::first(session, row.id))
        .await
        .map_err(Provisioned::from)?;
    tracing::warn!(
        %session,
        machine = %row.id,
        %reason,
        "the machine a recovery was to start is gone; a fresh one is being built"
    );
    Ok(Recovered::Ran)
}

/// Tells the agent what happened to its machine, once it is back to hear it.
///
/// Best-effort by place: the machine is coming back either way, and losing
/// the sentence that explains it is not worth failing the session over.
async fn tell_the_agent(db: &Db, rooms: &Rooms, session: SessionId, notice: String) {
    if let Err(error) = rooms
        .command(
            db,
            session,
            &ControlToDaemon::UserMessage {
                text: notice.trim_end().to_owned(),
                // Flyco speaking: the machine changed under the agent,
                // which is not something the user said.
                origin: flyco_core::MessageOrigin::Flyco,
            },
        )
        .await
    {
        tracing::warn!(%session, %error, "the machine notice did not reach the session room");
    }
}

/// Puts a reclaimed session back on the machine it was taken off.
///
/// Everything here follows from one fact: **the disk was never released.**
/// Every provider flyco puts spot capacity on stops the machine instead of
/// deleting it, so recovering is a `start` against the same machine row and
/// the same provider-native resources — not a provision, not a new disk,
/// and not a working-tree patch, because the working tree never went
/// anywhere.
///
/// The steps, in order:
///
/// 1. The session moves back to `provisioning`, keeping the reason it lost
///    its machine — which is what the UI renders as `Migrating` rather than
///    as an ordinary provision (docs/ux.md §6, §9.2).
/// 2. `reserving` and `booting` are announced as they happen, so the
///    timeline in the transcript says what the wait is being spent on.
/// 3. The provider starts the machine. `flycod` comes back with it: its
///    unit is enabled on the image, so the boot that follows a start runs
///    the daemon against the configuration already on the disk, and the
///    daemon asks the control plane which harness conversation to continue.
/// 4. The ledger records the replacement, and the agent is told — once,
///    afterwards, as an ordinary message in the conversation.
///
/// Nothing is installed on this path: the image on the disk already has
/// `flycod` on it. `booting` covers boot and install on a fresh machine and
/// the restart alone here, and the timeline says which it is — a recovery
/// reads `Starting the machine` rather than claiming an install that never
/// happens.
async fn recover(
    db: &Db,
    config: &ApiConfig,
    rooms: &Rooms,
    queue: &Queue,
    clients: &mut Clients<'_, impl Provisioner, impl GithubOauth>,
    recovery: Recovery,
) -> Result<Recovered, Provisioned> {
    let session = recovery.session;
    let Some((target, row)) = recoverable(db, session, recovery.machine).await? else {
        return Ok(Recovered::Done);
    };

    sessions::recovering(db, session)
        .await
        .map_err(Provisioned::from)?;
    announce(db, rooms, session, ProvisioningStage::Reserving).await;

    let account = provisioning::account(db, config, target.user_id, row.provider_account_id)
        .await
        .map_err(Provisioned::from)?;
    let started = match machines::restart(db, clients.provisioner, &account, &row).await {
        Ok(started) => started,
        Err(ProviderError::Gone(reason)) => {
            return reprovision_gone(db, queue, session, &row, &reason).await;
        }
        Err(error) => return Err(classify(&error)),
    };
    announce(db, rooms, session, ProvisioningStage::Booting).await;

    // What the recovery says about itself is the cause's whole difference.
    match recovery.interruption.cause {
        // A session waking to meet its plan window's reset is not a
        // reclamation: nothing was replaced, nobody is being billed twice,
        // and the agent's next message is the continuation
        // `crate::usage_limits` sends at the reset itself. A machine the
        // user resumed by hand is the same shape minus the wait.
        RecoveryCause::UsageLimit => {
            tracing::info!(
                %session,
                machine = %row.id,
                state = ?started.state,
                "started a waiting session's machine ahead of its plan window reset"
            );
        }
        RecoveryCause::Resumed => {
            tracing::info!(
                %session,
                machine = %row.id,
                state = ?started.state,
                "started a resumed session's machine on its own disk"
            );
        }
        RecoveryCause::Suspended => {
            // Nothing is billed — the compute meter stopped at the suspend
            // and the storage meter never paused — but the agent is told,
            // because everything it had running died with the machine.
            let notice = MachineSuspended {
                machine_type: row.machine_type(),
            }
            .render()
            .map_err(|error| {
                Provisioned::Failed(format!("the suspension notice did not render: {error}"))
            })?;
            tell_the_agent(db, rooms, session, notice).await;
            tracing::info!(
                %session,
                machine = %row.id,
                state = ?started.state,
                "started a suspended session's machine on its own disk"
            );
        }
        RecoveryCause::SpotReclaimed => {
            // The gap is what the user is being asked to pay for twice, so
            // it is named in the ledger rather than folded into the next
            // metering window.
            budgets::record_replacement(
                db,
                session,
                recovery.machine,
                recovery.interruption.since_unix,
                &row.machine_type(),
            )
            .await
            .map_err(Provisioned::from)?;

            // Last, and only after the machine is on its way back: the
            // agent is told in the conversation, and the room holds the
            // message until the daemon on the restarted machine comes to
            // take it.
            let notice = MachineReplaced {
                machine_type: row.machine_type(),
            }
            .render()
            .map_err(|error| {
                Provisioned::Failed(format!("the reclaim notice did not render: {error}"))
            })?;
            tell_the_agent(db, rooms, session, notice).await;
            tracing::info!(
                %session,
                machine = %row.id,
                state = ?started.state,
                "restarted a reclaimed session's machine on its own disk"
            );
        }
    }
    Ok(Recovered::Ran)
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
    clients: &Clients<'_, impl Provisioner, impl GithubOauth>,
    claim: &Claim,
    entry: &flyco_core::MachineCatalogEntry,
    spot: bool,
) -> Result<DaemonBootstrap, Provisioned> {
    let token = daemon_tokens::issue(db, claim.user, claim.session)
        .await
        .map_err(Provisioned::from)?;
    // Refreshes a subscription grant that is near its end, either vendor's,
    // so the machine this boots is handed a token good for longer than the
    // provision.
    let auth = harness_accounts::credential(db, config, clients.vendors, claim.user, claim.harness)
        .await
        .map_err(Provisioned::from)?;
    let repo = checkout(db, config, clients.github, claim).await?;

    Ok(DaemonBootstrap {
        session: claim.session,
        // Which endpoint the daemon watches for an eviction notice follows
        // from whose machine it is on, and nothing on the machine can tell
        // it that.
        provider: claim.machine.spec().provider,
        // And whether the disk survives a stop, which is what the daemon
        // needs to know what to do with the platform's SIGTERM.
        runtime: claim.machine.spec().runtime,
        control_plane_url: config.control_plane_url(),
        daemon_token: token.token,
        // What the session is recorded as running under, which for a
        // machine being rebuilt is whatever the user last changed it to
        // rather than the product default the previous machine booted on —
        // the same claim-carried fact `model` is.
        permission_mode: claim.permission_mode,
        auth,
        repo,
        machine_origin: claim.machine_origin,
        // The capacity mode asked for, because the document is written
        // *into* the provision call and nothing has answered it yet. A
        // provider that cannot honour spot answers with on-demand, and the
        // live `GET /v1/sessions/{id}/agent/machine` the agent's
        // `machine_status` reads is what says which it got.
        machine: flyco_core::SessionMachine::of(entry, spot),
        resume_session_id: sessions::harness_session(db, claim.session)
            .await
            .map_err(Provisioned::from)?
            .harness_session_id,
        // What the session is recorded as running, which for a machine
        // being rebuilt is whatever the user last changed it to rather than
        // what the previous machine booted on.
        model: claim.model.clone(),
        // The user's whole MCP registry, resolved once here: the machine
        // writes it into the harness's root-owned configuration, and that
        // file is the allowlist. A server missing from this list is one the
        // agent has no way to reach.
        mcp_servers: mcp::mounts(db, claim.user)
            .await
            .map_err(Provisioned::from)?,
    })
}

/// Builds the checkout the machine comes up holding.
///
/// Flyco's GitHub integration behaves *as the user* (docs/proposal.md), so
/// this is the user's own OAuth token and the user's own commit identity —
/// not a bot's. Three things happen here, and each is a session failure if
/// it does not:
///
/// * The stored token is opened and its scopes are read. A token that does
///   not grant `repo` cannot clone a private repository or push anything, so
///   the session fails *here* with
///   [`ApiError::GithubTokenInsufficient`](crate::error::ApiError::GithubTokenInsufficient)
///   — which tells the user to sign in again — rather than five minutes
///   later on a machine, with a git error about authentication.
/// * The commit identity is read from the same response, so a machine's
///   commits carry the user's name rather than `flyco@<hostname>`.
/// * A session that records no branch — one opened before flyco recorded
///   any — has the repository's default resolved and *written back*, so the
///   answer is stable for every later provision of that session.
async fn checkout(
    db: &Db,
    config: &ApiConfig,
    github: &impl GithubOauth,
    claim: &Claim,
) -> Result<RepoCheckout, Provisioned> {
    let token = users::github_token(db, config, claim.user)
        .await
        .map_err(Provisioned::from)?;
    let identity = github
        .current_user(&token)
        .await
        .map_err(|error| Provisioned::from(ApiError::from(error)))?;
    if !identity.grants_repo_scope() {
        return Err(Provisioned::from(ApiError::GithubTokenInsufficient {
            scope: REPO_SCOPE,
            repo: claim.repo.clone(),
        }));
    }

    let branch = if let Some(branch) = claim.branch.clone() {
        branch
    } else {
        let default_branch = github
            .get_repo(&token, &claim.repo)
            .await
            .map_err(|error| Provisioned::from(ApiError::from(error)))?
            .default_branch;
        sessions::record_branch(db, claim.session, &default_branch)
            .await
            .map_err(Provisioned::from)?;
        tracing::info!(
            session = %claim.session,
            repo = %claim.repo,
            branch = %default_branch,
            "recorded the default branch for a session opened before flyco tracked one"
        );
        default_branch
    };

    Ok(RepoCheckout {
        slug: claim.repo.clone(),
        branch,
        token: token.access_token,
        identity: GitIdentity {
            name: identity.user.commit_name().to_owned(),
            email: identity.user.commit_email(),
        },
    })
}

/// Tells the session's watchers how far its machine has got.
///
/// The timeline is what a user reads instead of a five-minute spinner
/// (docs/ux.md §9.2), and it is *only* that: a room that cannot be reached
/// is logged and stepped over rather than failing a provision that is
/// otherwise going fine. Losing a line of the timeline costs the user a
/// progress report; failing the job over it costs them the machine.
async fn announce(db: &Db, rooms: &Rooms, session: SessionId, stage: ProvisioningStage) {
    // The stage is progress, and progress is what the stall sweep counts:
    // a session whose machine is still moving is never called stalled. A
    // clock that cannot be written is logged rather than raised — the same
    // reason the broadcast below is.
    if let Err(error) = sessions::note_progress(db, session).await {
        tracing::warn!(%session, %error, "a provisioning stage did not move the session's clock");
    }
    if let Err(error) = rooms
        .broadcast(
            db,
            session,
            &ClientEvent::ProvisioningStage {
                stage,
                at_unix: now_unix(),
            },
        )
        .await
    {
        tracing::warn!(
            %session,
            ?stage,
            %error,
            "a provisioning stage did not reach the session room"
        );
    }
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
async fn retry(
    db: &Db,
    queue: &Queue,
    rooms: &Rooms,
    job: ProvisioningJob,
    reason: &str,
) -> Settled {
    // Only a job that names a session reaches here, and only such a job
    // counts attempts; both reads are written as refusals rather than
    // unwraps because a panic would take the whole batch down.
    let (Some(session), Some(attempt)) = (job.session(), job.attempt()) else {
        return Settled::Done;
    };
    if attempt >= MAX_ATTEMPTS {
        let exhausted =
            format!("gave up after {MAX_ATTEMPTS} attempts to reach the provider: {reason}");
        return match sessions::fail(db, rooms, session, &exhausted).await {
            Ok(()) => Settled::Done,
            Err(error) => Settled::Redeliver(error),
        };
    }

    let next = job.again();
    tracing::warn!(
        %session,
        attempt = attempt.saturating_add(1),
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
    use skyzen_services::{Db, Kv, Queue};

    // `#[wasm_bindgen]` expands an async export into a `future_to_promise`
    // call written unqualified, so the crate it lives in has to be nameable
    // from here. `#[skyzen::main]` gets this for free in the crate root.
    use skyzen::wasm_bindgen_futures;

    use super::{Clients, ProvisioningJob, consume};
    use crate::config::{ApiConfig, binding};
    use crate::github::GithubClient;
    use crate::provisioning::CloudProvisioner;
    use crate::rooms::{HostRooms, Rooms};
    use crate::vendors::Vendors;

    #[skyzen::queue]
    async fn provisioning_jobs(
        batch: QueueBatch<ProvisioningJob>,
        env: skyzen::runtime::wasm::Env,
    ) -> QueueBatchDisposition {
        // A fresh isolate may see a queue or cron event before any request,
        // and without this it would log nothing of what it did.
        crate::telemetry::install();
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
        // The catalog cache lives in the same namespace the sessions and
        // OAuth attempts do, and a consumer that cannot open it cannot
        // record what it read.
        let kv = match skyzen_cloudflare::CfKv::from_env(&env, binding::AUTH_KV) {
            Ok(cf) => Kv::new(cf),
            Err(error) => {
                tracing::error!(%error, "the provisioning consumer could not open KV");
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

        // One environment, two namespaces: a session's room takes the
        // provisioning timeline, and a host's room takes the container job
        // that *is* the machine.
        let wasm = skyzen::runtime::wasm::WasmEnv::new(env);
        let rooms = Rooms::from_wasm_env(wasm.clone());
        consume(
            &db,
            &config,
            &kv,
            &queue,
            &rooms,
            &mut Clients {
                provisioner: &mut CloudProvisioner::new(HostRooms::from_wasm_env(wasm)),
                vendors: &Vendors::default(),
                github: &GithubClient::default(),
            },
            batch,
        )
        .await
    }
}
