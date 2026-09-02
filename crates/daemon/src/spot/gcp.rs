//! Compute Engine's `preempted` metadata key.
//!
//! The only one of the three that offers a *hanging* read: with
//! `wait_for_change=true` the metadata server holds the connection open and
//! answers when the value changes, so a preemption is learned about the
//! moment Google decides it rather than up to one poll interval later.
//!
//! Two reads per iteration, in this order, and the order is the whole
//! design: a plain read first, because a hanging read only answers on a
//! *change* and would block forever on a machine that is already flagged;
//! then a hanging read with a timeout, so a connection the network drops
//! silently is retried rather than waited on for ever.
//!
//! Google publishes no deadline with the flag — unlike Azure's `NotBefore`
//! and EC2's `time` — so the notice carries the grace period Google
//! documents instead: thirty seconds between the flag and the shutdown.
//! flyco provisions spot instances with `instanceTerminationAction: STOP`,
//! so what happens at the end of it is a stopped instance whose boot disk
//! is still there.

use core::time::Duration;

use super::{EvictionWatcher, MetadataMethod, MetadataRequest, SpotNotice, read};

/// Where Compute Engine publishes the flag.
pub const ENDPOINT: &str = "http://metadata.google.internal/computeMetadata/v1/instance/preempted";

/// The query that makes a read hang until the value changes.
///
/// `timeout_sec` bounds it: the server answers with the unchanged value
/// when it lapses, which is what keeps a dropped connection from becoming a
/// watcher that never looks again.
const WAIT: &str = "?wait_for_change=true&timeout_sec=60";

/// The header without which the metadata server refuses the read.
const FLAVOR_HEADER: (&str, &str) = ("Metadata-Flavor", "Google");

/// What the endpoint answers once this instance is being preempted.
const PREEMPTED: &str = "TRUE";

/// How long Compute Engine gives a preempted instance.
///
/// Google's published contract rather than a number flyco chose: the
/// shutdown follows the flag by thirty seconds, and the metadata server
/// carries no deadline of its own to read instead.
pub const GRACE: Duration = Duration::from_secs(30);

/// How long a hanging read may take before it is abandoned and reissued.
///
/// Longer than the `timeout_sec` it carries, so the server's own answer is
/// what normally ends the read and this only catches a connection that died
/// without one.
const WAIT_TIMEOUT: Duration = Duration::from_secs(75);

/// How long a plain read may take.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait before looking again after a failed read.
///
/// Only reached when the metadata server is unreachable, which on a healthy
/// instance never happens: the ordinary path spends its time inside a
/// hanging read rather than sleeping between polls.
pub const RETRY_INTERVAL: Duration = Duration::from_secs(1);

/// Compute Engine's eviction watcher.
#[derive(Debug, Clone)]
pub struct Preempted {
    endpoint: String,
    retry: Duration,
    grace: Duration,
}

impl Preempted {
    /// The watcher a provisioned Compute Engine instance runs.
    #[must_use]
    pub fn live() -> Self {
        Self {
            endpoint: ENDPOINT.to_owned(),
            retry: RETRY_INTERVAL,
            grace: GRACE,
        }
    }

    /// The same watcher pointed at another endpoint, for a test with its
    /// own server.
    #[cfg(test)]
    #[must_use]
    pub const fn at(endpoint: String, retry: Duration) -> Self {
        Self {
            endpoint,
            retry,
            grace: GRACE,
        }
    }

    /// Reads the flag, hanging until it changes if `wait`.
    ///
    /// `None` is a read that said nothing — the flag is still false, or the
    /// server could not be reached, which are the same instruction: look
    /// again.
    async fn poll(&self, wait: bool) -> Option<bool> {
        let url = if wait {
            let mut url = self.endpoint.clone();
            url.push_str(WAIT);
            url
        } else {
            self.endpoint.clone()
        };
        let request = MetadataRequest {
            method: MetadataMethod::Get,
            url: &url,
            headers: &[FLAVOR_HEADER],
        };
        let timeout = if wait { WAIT_TIMEOUT } else { READ_TIMEOUT };

        match read(request, timeout).await {
            Ok(Some(body)) => {
                let value = String::from_utf8_lossy(&body);
                Some(value.trim().eq_ignore_ascii_case(PREEMPTED))
            }
            Ok(None) => Some(false),
            Err(error) => {
                tracing::debug!(%error, "the preempted metadata key could not be read");
                None
            }
        }
    }

    /// The notice this instance's grace period amounts to.
    fn notice(&self) -> SpotNotice {
        SpotNotice {
            seconds_remaining: u32::try_from(self.grace.as_secs()).unwrap_or(u32::MAX),
        }
    }
}

impl EvictionWatcher for Preempted {
    async fn watch(self) -> SpotNotice {
        loop {
            match self.poll(false).await {
                Some(true) => {
                    tracing::warn!("compute engine flagged this instance as preempted");
                    return self.notice();
                }
                Some(false) => {}
                None => {
                    tokio::time::sleep(self.retry).await;
                    continue;
                }
            }

            // Held open by the metadata server until the flag changes.
            match self.poll(true).await {
                Some(true) => {
                    tracing::warn!("compute engine flagged this instance as preempted");
                    return self.notice();
                }
                // The hanging read lapsed or the value is still false;
                // either way the next iteration re-reads it plainly, which
                // is what re-establishes the etag the wait is against.
                Some(false) => {}
                None => tokio::time::sleep(self.retry).await,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::{ENDPOINT, GRACE, Preempted, RETRY_INTERVAL};
    use crate::spot::EvictionWatcher as _;
    use crate::testing::{MetadataEndpoint, Reply};

    async fn watcher(replies: Vec<Reply>) -> (MetadataEndpoint, Preempted) {
        let endpoint = MetadataEndpoint::start(replies).await;
        let url = endpoint
            .base
            .join("computeMetadata/v1/instance/preempted")
            .expect("a loopback metadata URL")
            .to_string();
        let watcher = Preempted::at(url, Duration::from_millis(5));
        (endpoint, watcher)
    }

    #[tokio::test]
    async fn the_flag_turning_true_is_the_notice() {
        let (mut endpoint, watcher) = watcher(vec![
            Reply::text("FALSE"),
            Reply::text("FALSE"),
            Reply::text("TRUE"),
        ])
        .await;

        let notice = tokio::time::timeout(Duration::from_secs(5), watcher.watch())
            .await
            .expect("the watcher announced the preemption");
        assert_eq!(
            u64::from(notice.seconds_remaining),
            GRACE.as_secs(),
            "google publishes no deadline, so the notice carries the documented grace"
        );

        let first = endpoint.next().await.expect("the endpoint was read");
        assert_eq!(first.method, "GET");
        assert!(first.target.ends_with("/instance/preempted"));

        let second = endpoint.next().await.expect("the endpoint was read again");
        assert!(
            second.target.contains("wait_for_change=true"),
            "the second read of each round hangs until the value changes: {}",
            second.target
        );
    }

    #[tokio::test]
    async fn a_metadata_server_that_will_not_answer_is_asked_again() {
        let (_endpoint, watcher) = watcher(vec![
            Reply::problem(500, "metadata-unavailable", "the server is restarting"),
            Reply::text("TRUE"),
        ])
        .await;

        tokio::time::timeout(Duration::from_secs(5), watcher.watch())
            .await
            .expect("the watcher recovered from the refused read");
    }

    #[test]
    fn the_live_watcher_reads_the_documented_endpoint() {
        let live = Preempted::live();
        assert_eq!(live.endpoint, ENDPOINT);
        assert_eq!(live.retry, RETRY_INTERVAL);
        assert_eq!(live.grace, GRACE);
    }
}
