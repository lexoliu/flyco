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
    BranchName, ClientEvent, ControlToDaemon, HarnessKind, MachineId, MachineOrigin,
    PermissionMode, ProviderAccountId, ProvisioningStage, RepoSlug, SessionId, SessionState,
    UserId,
};
use flyco_provider::{DaemonBootstrap, GitIdentity, ProviderError, ProvisionRequest, RepoCheckout};
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
        /// When the provider announced the reclamation.
        ///
        /// Carried rather than read from the clock so the ledger entry this
        /// job writes is the same entry on every redelivery: an
        /// at-least-once queue delivers a recovery twice, and the second
        /// delivery must not bill the replacement a second time.
        reclaimed_at_unix: u64,
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
    pub const fn recovery(session: SessionId, machine: MachineId, reclaimed_at_unix: u64) -> Self {
        Self::Recover {
            session,
            machine,
            attempt: 1,
            reclaimed_at_unix,
        }
    }

    /// The session this job is for, when it is for one at all.
    #[must_use]
    pub const fn session(&self) -> Option<SessionId> {
        match self {
            Self::Provision { session, .. } | Self::Recover { session, .. } => Some(*session),
            Self::RefreshCatalog { .. } | Self::RefreshCatalogRegion { .. } => None,
        }
    }

    /// The machine row this job is allowed to act on, when it acts on one.
    #[must_use]
    pub const fn machine(&self) -> Option<MachineId> {
        match self {
            Self::Provision { machine, .. } | Self::Recover { machine, .. } => Some(*machine),
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
            Self::Provision { attempt, .. } | Self::Recover { attempt, .. } => Some(*attempt),
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
            Self::Recover {
                session,
                machine,
                attempt,
                reclaimed_at_unix,
            } => Self::Recover {
                session,
                machine,
                attempt: attempt.saturating_add(1),
                reclaimed_at_unix,
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
        "queued a recovery for after the provider takes the machine"
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
        ProvisioningJob::Provision { .. } | ProvisioningJob::Recover { .. } => {}
    }

    let outcome = match job {
        ProvisioningJob::Provision { .. } => match claim(db, rooms, job.clone()).await {
            Ok(None) => return Settled::Done,
            Ok(Some(claimed)) => build(db, config, rooms, clients, &claimed).await,
            Err(error) => return Settled::Redeliver(error),
        },
        ProvisioningJob::Recover {
            session,
            machine,
            reclaimed_at_unix,
            ..
        } => match recover(
            db,
            config,
            rooms,
            clients,
            session,
            machine,
            reclaimed_at_unix,
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

    Ok(Some(Claim {
        session,
        user: target.user_id,
        harness: target.harness,
        repo: target.repo,
        branch: target.branch,
        machine_origin: target.machine_origin,
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
    rooms: &Rooms,
    clients: &mut Clients<'_, impl Provisioner, impl GithubOauth>,
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

    let bootstrap = bootstrap(db, config, clients, claim, &entry, spec.spot).await?;
    announce(db, rooms, claim.session, ProvisioningStage::Reserving).await;
    let machine = clients
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
    // The provider handed back a machine, so it exists and is powering on.
    announce(db, rooms, claim.session, ProvisioningStage::Booting).await;

    // What the machine turned out to be, priced at the capacity it actually
    // holds — which is what the budget meters and what the agent's
    // `machine_status` reads back.
    let built = flyco_core::SessionMachine::of(&entry, machine.capacity_mode.is_spot());
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
    // Recorded, so the machine is durably flyco's; what happens on it next
    // is its bootstrap fetching and running the `flycod` installer. That is
    // the last stage the control plane can see — everything after it is
    // announced by the daemon on the machine itself.
    announce(db, rooms, claim.session, ProvisioningStage::Installing).await;

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

/// Whether a recovery had anything to do.
enum Recovered {
    /// It ran: the machine was started again.
    Ran,
    /// There was nothing to recover — the session is gone, archived, or
    /// already back on a machine — and the message is spent.
    Done,
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
/// `installing` is deliberately not announced: nothing is installed. The
/// image on the disk already has `flycod` on it, and claiming otherwise
/// would put a step in the user's timeline that never happens.
async fn recover(
    db: &Db,
    config: &ApiConfig,
    rooms: &Rooms,
    clients: &mut Clients<'_, impl Provisioner, impl GithubOauth>,
    session: SessionId,
    machine: MachineId,
    reclaimed_at_unix: u64,
) -> Result<Recovered, Provisioned> {
    let Some(target) = sessions::provisioning_target(db, session)
        .await
        .map_err(Provisioned::from)?
    else {
        tracing::info!(%session, "dropping a recovery for a session that no longer exists");
        return Ok(Recovered::Done);
    };
    if !target.state.holds_environment() {
        tracing::info!(
            %session,
            state = ?target.state,
            "dropping a recovery for a session that no longer holds a machine"
        );
        return Ok(Recovered::Done);
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
        return Ok(Recovered::Done);
    }

    sessions::recovering(db, session)
        .await
        .map_err(Provisioned::from)?;
    announce(db, rooms, session, ProvisioningStage::Reserving).await;

    let account = provisioning::account(db, config, target.user_id, row.provider_account_id)
        .await
        .map_err(Provisioned::from)?;
    let started = machines::restart(db, clients.provisioner, &account, &row)
        .await
        .map_err(|error| classify(&error))?;
    announce(db, rooms, session, ProvisioningStage::Booting).await;

    // The gap is what the user is being asked to pay for twice, so it is
    // named in the ledger rather than folded into the next metering window.
    budgets::record_replacement(db, session, machine, reclaimed_at_unix, &row.machine_type())
        .await
        .map_err(Provisioned::from)?;

    // Last, and only after the machine is on its way back: the agent is
    // told in the conversation, and the room holds the message until the
    // daemon on the restarted machine comes to take it.
    let notice = MachineReplaced {
        machine_type: row.machine_type(),
    }
    .render()
    .map_err(|error| Provisioned::Failed(format!("the reclaim notice did not render: {error}")))?;
    if let Err(error) = rooms
        .command(
            session,
            &ControlToDaemon::UserMessage {
                text: notice.trim_end().to_owned(),
            },
        )
        .await
    {
        // The machine is coming back either way; losing the sentence that
        // explains it is not worth failing the session over.
        tracing::warn!(%session, %error, "the reclaim notice did not reach the session room");
    }

    tracing::info!(
        %session,
        machine = %row.id,
        state = ?started.state,
        "restarted a reclaimed session's machine on its own disk"
    );
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
        control_plane_url: config.control_plane_url(),
        daemon_token: token.token,
        // Auto is the product default. Flyco's managed deny rules still bind
        // even in this mode, and anything the classifier does not auto-allow
        // still reaches the approval UI.
        permission_mode: PermissionMode::Auto,
        auth,
        repo,
        machine_origin: claim.machine_origin,
        // The capacity mode asked for, because the document is written
        // *into* the provision call and nothing has answered it yet. A
        // provider that cannot honour spot answers with on-demand, and the
        // live `GET /v1/sessions/{id}/agent/machine` the agent's
        // `machine_status` reads is what says which it got.
        machine: flyco_core::SessionMachine::of(entry, spot),
        resume_session_id: sessions::harness_session_id(db, claim.session)
            .await
            .map_err(Provisioned::from)?,
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
