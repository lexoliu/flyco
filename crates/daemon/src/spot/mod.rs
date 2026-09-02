//! Watching for the provider's eviction notice, and what the daemon does
//! with the seconds it buys.
//!
//! Spot is flyco's default capacity, so every session VM is one a provider
//! may take back. All three clouds announce it the same way in principle
//! and in no way in practice: the notice arrives on the machine's own
//! instance-metadata endpoint, at an address, behind headers, and in a
//! document that are each the provider's own. [`EvictionWatcher`] is that
//! one fact — *this machine is being reclaimed, in this many seconds* —
//! stated once, with an implementation per provider ([`azure`], [`aws`],
//! [`gcp`]) and a [`FakeEviction`] for tests.
//!
//! # The agent is never asked
//!
//! Nothing here reaches the model. An LLM is slow and unpredictable, and
//! the window is thirty seconds on two of the three providers; it is spent
//! entirely by flyco, ending the turn, getting the transcript to the
//! control plane, and flushing the disk. What the agent is told comes
//! *afterwards*, from the control plane, once the session is running again.
//!
//! # Why the disk is not snapshotted
//!
//! Reclaim releases the machine and never the disk: Azure deallocates, an
//! AWS persistent spot request stops the instance, and Compute Engine is
//! provisioned with `instanceTerminationAction: STOP`. So the working tree
//! survives untouched and there is nothing to snapshot — the workdir patch
//! is the safety net before an *archive* releases the disk, which is a
//! different event. [`Disk::sync`] is what makes that survival true rather
//! than assumed: it flushes the page cache before the compute disappears.

pub mod aws;
pub mod azure;
pub mod gcp;

use core::future::Future;
use core::time::Duration;

use flyco_core::CloudProviderKind;
use tokio::sync::mpsc;

/// What a provider announced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpotNotice {
    /// Seconds until the machine loses its compute, as announced.
    ///
    /// The provider's own deadline where it publishes one — Azure's
    /// `NotBefore`, EC2's `time` — and the documented grace period where it
    /// does not. It reaches the UI as a countdown and the control plane as
    /// the delay before the replacement is asked for, so a number invented
    /// here would be a promise flyco cannot keep.
    pub seconds_remaining: u32,
}

/// The machine's own view of whether it is about to be reclaimed.
///
/// `watch` consumes the watcher and resolves *once*, on the first notice:
/// a reclamation is not a stream of events to be handled repeatedly, it is
/// the last thing that happens to this machine. Every provider re-reports
/// the same event on every poll until it acts on it, and a watcher that
/// resolved twice would have the daemon interrupt a session that is already
/// shutting down.
///
/// There is no error in the signature, and that is the contract rather than
/// an omission: a metadata endpoint that refuses, times out, or answers
/// nonsense says nothing about whether this machine is being reclaimed, so
/// the only correct response is to log it and ask again. The future
/// resolves when — and only when — the provider has actually said so.
pub trait EvictionWatcher: Send + 'static {
    /// Polls until the provider announces this machine's reclamation.
    fn watch(self) -> impl Future<Output = SpotNotice> + Send;
}

/// Flushing what is in memory to the disk that outlives the machine.
///
/// A trait with one production implementation, because the ordering it
/// takes part in is the feature: a test asserts that the filesystem was
/// synced *after* the transcript reached the control plane and *before* the
/// notice went out, and it cannot assert that against a bare function call.
pub trait Disk: Send + Sync + 'static {
    /// Flushes every filesystem, and does not return until it lands.
    ///
    /// # Errors
    ///
    /// Returns [`DiskError`] if the flush could not be performed or did not
    /// report success.
    fn sync(&self) -> impl Future<Output = Result<(), DiskError>> + Send;
}

/// The filesystem flush did not happen.
#[derive(Debug, thiserror::Error)]
pub enum DiskError {
    /// `sync` could not be run at all.
    #[error("could not run `sync`")]
    Spawn(#[source] std::io::Error),
    /// `sync` ran and reported failure.
    #[error("`sync` exited with {status}")]
    Failed {
        /// What it exited with.
        status: std::process::ExitStatus,
    },
}

/// The real disk: coreutils' `sync`, which is `sync(2)` and a wait.
///
/// A child process rather than a raw syscall because the syscall would be
/// the daemon's first `unsafe` block and its first libc dependency, to save
/// one `fork` in the last thirty seconds of a machine's life. `sync` is on
/// every image flyco provisions.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostDisk;

impl Disk for HostDisk {
    async fn sync(&self) -> Result<(), DiskError> {
        let status = tokio::process::Command::new("sync")
            .status()
            .await
            .map_err(DiskError::Spawn)?;
        if status.success() {
            Ok(())
        } else {
            Err(DiskError::Failed { status })
        }
    }
}

/// Where a notice reaches the relay from.
///
/// A channel rather than a fifth generic parameter on the relay: the
/// watcher is a task with no relationship to the session it interrupts, and
/// what the relay needs from it is one value. The receiver a machine that
/// cannot be reclaimed gets is a closed one, which never yields — see
/// [`nothing_to_watch`].
pub type Notices = mpsc::Receiver<SpotNotice>;

/// Watches the endpoint the configured provider publishes notices on.
///
/// A machine with no `spot_provider` in its configuration is one no notice
/// can arrive for — on-demand capacity, or hardware the user owns — and
/// gets [`nothing_to_watch`].
#[must_use]
pub fn watch(provider: Option<CloudProviderKind>) -> Notices {
    let (notices, receiver) = mpsc::channel(1);
    match provider {
        Some(CloudProviderKind::Azure) => spawn(azure::ScheduledEvents::live(), notices),
        Some(CloudProviderKind::Aws) => spawn(aws::InstanceAction::live(), notices),
        Some(CloudProviderKind::Gcp) => spawn(gcp::Preempted::live(), notices),
        // A container on hardware the user registered is started and
        // stopped by its owner: there is no metadata endpoint, and nothing
        // to poll for a notice that cannot arrive.
        Some(CloudProviderKind::ByoSsh) | None => {
            tracing::info!("this machine holds capacity nobody can reclaim; watching no endpoint");
            drop(notices);
        }
    }
    receiver
}

/// A closed channel, for a session that can never be reclaimed.
#[must_use]
pub fn nothing_to_watch() -> Notices {
    watch(None)
}

/// Runs one watcher until it has something to say, then stops.
pub fn spawn<W: EvictionWatcher>(watcher: W, notices: mpsc::Sender<SpotNotice>) {
    tokio::spawn(async move {
        let notice = watcher.watch().await;
        tracing::warn!(
            seconds_remaining = notice.seconds_remaining,
            "the provider announced this machine's reclamation"
        );
        if notices.send(notice).await.is_err() {
            tracing::warn!("nothing was listening for the eviction notice");
        }
    });
}

/// The host clock, in seconds since the Unix epoch.
///
/// # Panics
///
/// Panics on a clock set before 1970, which is not a state a session VM can
/// be in and not one this daemon could do anything sensible about.
#[must_use]
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the host clock is set before the Unix epoch")
        .as_secs()
}

/// How long a deadline leaves, as the notice reports it.
///
/// Saturating in both directions: a deadline that has already passed is
/// zero seconds rather than a wrap-around, and a provider that announced a
/// reclamation years away would still be announcing one.
#[must_use]
pub fn seconds_until(deadline_unix: u64) -> u32 {
    u32::try_from(deadline_unix.saturating_sub(now_unix())).unwrap_or(u32::MAX)
}

/// One read of an instance-metadata endpoint.
///
/// `Ok(None)` is the endpoint answering "nothing to announce" with a status
/// rather than a document, which is how EC2 says it. Every other non-2xx is
/// an error, because a metadata endpoint that refuses a read is not the
/// same as one that has nothing to say.
///
/// # Errors
///
/// Returns [`MetadataError`] if the endpoint could not be reached, took
/// longer than `timeout`, or answered a status that is neither success nor
/// [`NOT_FOUND`].
pub(crate) async fn read(
    request: MetadataRequest<'_>,
    timeout: Duration,
) -> Result<Option<Vec<u8>>, MetadataError> {
    let exchange = perform(request);
    match tokio::time::timeout(timeout, exchange).await {
        Ok(result) => result,
        Err(_elapsed) => Err(MetadataError::TimedOut { timeout }),
    }
}

/// The status a metadata endpoint answers with while there is nothing to
/// announce.
const NOT_FOUND: u16 = 404;

/// What a metadata endpoint is asked, in the shape all three need.
///
/// A `PUT` is here for one reason: EC2's `IMDSv2` mints its session token
/// with one, and a token-less read of that endpoint is refused outright.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MetadataRequest<'a> {
    /// Whether this mints a token or reads a document.
    pub method: MetadataMethod,
    /// Absolute URL, query string included.
    pub url: &'a str,
    /// Headers the endpoint requires, in the order they are set.
    pub headers: &'a [(&'a str, &'a str)],
}

/// The two verbs an instance-metadata endpoint answers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MetadataMethod {
    /// Read a document.
    Get,
    /// Mint an `IMDSv2` session token.
    Put,
}

async fn perform(request: MetadataRequest<'_>) -> Result<Option<Vec<u8>>, MetadataError> {
    use zenwave::{Client as _, ResponseExt as _};

    let mut client = zenwave::client();
    let mut builder = match request.method {
        MetadataMethod::Get => client.get(request.url),
        MetadataMethod::Put => client.put(request.url),
    }
    .map_err(|error| MetadataError::Unreachable(error.to_string()))?;
    for (name, value) in request.headers {
        builder = builder
            .header(*name, *value)
            .map_err(|error| MetadataError::Unreachable(error.to_string()))?;
    }

    let response = match builder.bytes_body(Vec::new()).await {
        Ok(response) => response,
        Err(error) => {
            if let zenwave::Error::Http { status, .. } = &error
                && status.as_u16() == NOT_FOUND
            {
                return Ok(None);
            }
            return Err(MetadataError::Unreachable(error.to_string()));
        }
    };

    let body = response
        .into_bytes()
        .await
        .map_err(|error| MetadataError::Unreachable(error.to_string()))?;
    Ok(Some(body.to_vec()))
}

/// A metadata read that says nothing about whether this machine is being
/// reclaimed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum MetadataError {
    /// The endpoint could not be reached or refused the read.
    #[error("the instance-metadata endpoint did not answer: {0}")]
    Unreachable(String),
    /// The read outlived its timeout.
    #[error("the instance-metadata endpoint did not answer within {}s", timeout.as_secs())]
    TimedOut {
        /// How long it was given.
        timeout: Duration,
    },
    /// The endpoint answered a document this watcher cannot read.
    #[error("the instance-metadata endpoint answered a document flycod cannot read: {0}")]
    Undecodable(String),
}

/// An [`EvictionWatcher`] that announces exactly what a test tells it to.
#[cfg(test)]
#[derive(Debug)]
pub struct FakeEviction(tokio::sync::oneshot::Receiver<SpotNotice>);

#[cfg(test)]
impl FakeEviction {
    /// A watcher and the trigger that makes it announce.
    #[must_use]
    pub fn pair() -> (Self, tokio::sync::oneshot::Sender<SpotNotice>) {
        let (announce, watched) = tokio::sync::oneshot::channel();
        (Self(watched), announce)
    }
}

#[cfg(test)]
impl EvictionWatcher for FakeEviction {
    async fn watch(self) -> SpotNotice {
        match self.0.await {
            Ok(notice) => notice,
            // Nothing announced anything and the trigger is gone: this
            // machine is not being reclaimed, so the watcher never resolves.
            Err(_) => core::future::pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Disk as _, HostDisk, SpotNotice, seconds_until, spawn, watch};
    use core::time::Duration;
    use flyco_core::CloudProviderKind;

    #[tokio::test]
    async fn a_machine_nobody_can_reclaim_watches_nothing() {
        // The receiver is closed rather than idle, so the relay can tell
        // "no watcher" from "no notice yet" and stop selecting on it.
        for provider in [None, Some(CloudProviderKind::ByoSsh)] {
            let mut notices = watch(provider);
            assert_eq!(notices.recv().await, None);
        }
    }

    #[tokio::test]
    async fn a_watcher_that_announces_reaches_the_channel() {
        let (watcher, announce) = super::FakeEviction::pair();
        let (notices, mut receiver) = tokio::sync::mpsc::channel(1);
        spawn(watcher, notices);
        announce
            .send(SpotNotice {
                seconds_remaining: 30,
            })
            .expect("the watcher is live");

        assert_eq!(
            receiver.recv().await,
            Some(SpotNotice {
                seconds_remaining: 30
            })
        );
    }

    #[test]
    fn a_deadline_that_has_passed_leaves_no_seconds() {
        assert_eq!(seconds_until(0), 0);
        assert!(seconds_until(super::now_unix() + 30) >= 29);
    }

    #[tokio::test]
    async fn the_host_disk_flushes_through_the_real_command() {
        // `sync` is a coreutils binary on every image flyco provisions, and
        // on the developer machines this suite runs on.
        tokio::time::timeout(Duration::from_secs(10), HostDisk.sync())
            .await
            .expect("sync returned")
            .expect("sync succeeded");
    }
}
