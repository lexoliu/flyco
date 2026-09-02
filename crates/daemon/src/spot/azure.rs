//! Azure Scheduled Events, which is how an Azure Spot VM is told.
//!
//! One endpoint on the instance's own link-local address, one required
//! header, and a document listing every platform event scheduled against
//! this machine. Eviction is the `Preempt` event; a `NotBefore` on it is
//! the deadline, published as an RFC 1123 instant, and Azure's own guidance
//! is to poll once a second because the notice is only thirty seconds long.
//!
//! The event is re-listed on every read until the platform acts on it, so
//! the first one seen is the only one that matters — the watcher resolves
//! there and stops.

use core::time::Duration;

use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc2822;

use super::{
    EvictionWatcher, MetadataMethod, MetadataRequest, SpotNotice, now_unix, read, seconds_until,
};

/// Where every Azure VM publishes its scheduled events.
///
/// The `api-version` is pinned: the document's shape is the contract this
/// module parses, and a version negotiated at runtime would be a different
/// document arriving without warning.
pub const ENDPOINT: &str = "http://169.254.169.254/metadata/scheduledevents?api-version=2020-07-01";

/// The header without which the endpoint refuses the read.
///
/// Its purpose is to prove the request was not made by a browser or a
/// confused proxy on the VM's behalf: a cross-site request cannot set it.
const METADATA_HEADER: (&str, &str) = ("Metadata", "true");

/// How often the endpoint is asked.
///
/// Azure's own recommendation for a Spot VM, and it follows from the
/// window: an eviction is announced thirty seconds ahead, so a minute-long
/// poll would learn about it after the machine was gone.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How long one read may take before it is abandoned and retried.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// The event type that means this Spot VM is being taken back.
///
/// Azure schedules several kinds of platform maintenance against a VM —
/// `Reboot`, `Redeploy`, `Freeze` — and none of the others releases the
/// machine. Reacting to them would end the agent's turn for a pause it
/// would have survived.
const PREEMPT: &str = "Preempt";

/// The document the endpoint answers with.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ScheduledEventsDocument {
    events: Vec<ScheduledEvent>,
}

/// One platform event scheduled against this machine.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ScheduledEvent {
    event_id: String,
    event_type: String,
    /// When the platform may act, as an RFC 1123 instant.
    ///
    /// Azure documents it as possibly empty — an event with no notice at
    /// all — which is why it is a string here and a deadline only after it
    /// parses.
    #[serde(default)]
    not_before: String,
}

impl ScheduledEvent {
    /// How long this event leaves, in seconds.
    ///
    /// An unreadable or absent `NotBefore` is treated as *now*, which is
    /// the safe reading of a deadline flyco cannot see: the alternative is
    /// to invent a window the platform never promised and be halfway
    /// through flushing when the machine goes.
    fn seconds_remaining(&self) -> u32 {
        if self.not_before.is_empty() {
            tracing::warn!(
                event = %self.event_id,
                "a preemption event published no deadline; treating it as immediate"
            );
            return 0;
        }
        match OffsetDateTime::parse(&self.not_before, &Rfc2822) {
            Ok(deadline) => {
                let unix = u64::try_from(deadline.unix_timestamp()).unwrap_or_else(|_| now_unix());
                seconds_until(unix)
            }
            Err(error) => {
                tracing::warn!(
                    event = %self.event_id,
                    not_before = %self.not_before,
                    %error,
                    "a preemption event's deadline did not parse; treating it as immediate"
                );
                0
            }
        }
    }
}

/// Azure's eviction watcher.
#[derive(Debug, Clone)]
pub struct ScheduledEvents {
    endpoint: String,
    interval: Duration,
}

impl ScheduledEvents {
    /// The watcher a provisioned Azure VM runs.
    #[must_use]
    pub fn live() -> Self {
        Self {
            endpoint: ENDPOINT.to_owned(),
            interval: POLL_INTERVAL,
        }
    }

    /// The same watcher pointed at another endpoint, for a test with its
    /// own server.
    #[cfg(test)]
    #[must_use]
    pub const fn at(endpoint: String, interval: Duration) -> Self {
        Self { endpoint, interval }
    }

    /// One read: the notice, if the document carries one.
    async fn poll(&self) -> Option<SpotNotice> {
        let request = MetadataRequest {
            method: MetadataMethod::Get,
            url: &self.endpoint,
            headers: &[METADATA_HEADER],
        };
        let body = match read(request, READ_TIMEOUT).await {
            Ok(Some(body)) => body,
            Ok(None) => return None,
            Err(error) => {
                tracing::debug!(%error, "azure scheduled events could not be read");
                return None;
            }
        };

        let document: ScheduledEventsDocument = match serde_json::from_slice(&body) {
            Ok(document) => document,
            Err(error) => {
                tracing::warn!(%error, "azure scheduled events answered an unreadable document");
                return None;
            }
        };

        document
            .events
            .iter()
            .find(|event| event.event_type == PREEMPT)
            .map(|event| {
                tracing::warn!(event = %event.event_id, "azure scheduled a preemption of this VM");
                SpotNotice {
                    seconds_remaining: event.seconds_remaining(),
                }
            })
    }
}

impl EvictionWatcher for ScheduledEvents {
    async fn watch(self) -> SpotNotice {
        loop {
            if let Some(notice) = self.poll().await {
                return notice;
            }
            tokio::time::sleep(self.interval).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::{ENDPOINT, POLL_INTERVAL, ScheduledEvents};
    use crate::spot::EvictionWatcher as _;
    use crate::testing::{MetadataEndpoint, Reply};

    /// The document Azure answers with when nothing is scheduled.
    const QUIET: &str = r#"{"DocumentIncarnation":1,"Events":[]}"#;

    fn preempt(not_before: &str) -> String {
        format!(
            r#"{{"DocumentIncarnation":2,"Events":[{{"EventId":"602d9444","EventStatus":"Scheduled",
                "EventType":"Preempt","ResourceType":"VirtualMachine","Resources":["flyco-vm"],
                "NotBefore":"{not_before}","Description":"","EventSource":"Platform",
                "DurationInSeconds":-1}}]}}"#
        )
    }

    /// An RFC 1123 instant `seconds` from now, the way Azure publishes one.
    fn deadline_in(seconds: u64) -> String {
        use time::format_description::well_known::Rfc2822;
        let at = time::OffsetDateTime::from_unix_timestamp(
            i64::try_from(crate::spot::now_unix() + seconds).expect("a plausible clock"),
        )
        .expect("a valid instant");
        at.format(&Rfc2822).expect("format an RFC 2822 instant")
    }

    async fn watcher(replies: Vec<Reply>) -> (MetadataEndpoint, ScheduledEvents) {
        let endpoint = MetadataEndpoint::start(replies).await;
        let url = endpoint
            .base
            .join("metadata/scheduledevents?api-version=2020-07-01")
            .expect("a loopback metadata URL")
            .to_string();
        let watcher = ScheduledEvents::at(url, Duration::from_millis(5));
        (endpoint, watcher)
    }

    #[tokio::test]
    async fn a_quiet_endpoint_is_polled_until_a_preemption_is_scheduled() {
        let (mut endpoint, watcher) = watcher(vec![
            Reply::text(QUIET),
            Reply::text(QUIET),
            Reply::text(&preempt(&deadline_in(30))),
        ])
        .await;

        let notice = tokio::time::timeout(Duration::from_secs(5), watcher.watch())
            .await
            .expect("the watcher announced the preemption");
        assert!(
            (25..=30).contains(&notice.seconds_remaining),
            "the deadline Azure published is what the countdown reports: {}",
            notice.seconds_remaining
        );

        let first = endpoint.next().await.expect("the endpoint was read");
        assert_eq!(first.method, "GET");
        assert!(first.target.contains("api-version=2020-07-01"));
    }

    #[tokio::test]
    async fn maintenance_that_keeps_the_machine_is_not_a_reclamation() {
        // A reboot or a freeze is not an eviction, and ending the agent's
        // turn for one would cost a turn the session would have survived.
        let reboot = r#"{"DocumentIncarnation":3,"Events":[{"EventId":"a","EventStatus":"Scheduled",
            "EventType":"Reboot","ResourceType":"VirtualMachine","Resources":["flyco-vm"],
            "NotBefore":"Mon, 19 Sep 2016 18:29:47 GMT","Description":"","EventSource":"Platform",
            "DurationInSeconds":-1}]}"#;
        let (_endpoint, watcher) = watcher(vec![
            Reply::text(reboot),
            Reply::text(reboot),
            Reply::text(&preempt(&deadline_in(30))),
        ])
        .await;

        tokio::time::timeout(Duration::from_secs(5), watcher.watch())
            .await
            .expect("only the preemption resolved the watcher");
    }

    #[tokio::test]
    async fn a_deadline_that_will_not_parse_is_read_as_immediate() {
        // Better to spend zero seconds believing there are none than to
        // invent a window the platform never promised.
        let (_endpoint, watcher) = watcher(vec![Reply::text(&preempt("not a date"))]).await;
        let notice = tokio::time::timeout(Duration::from_secs(5), watcher.watch())
            .await
            .expect("the watcher announced the preemption");
        assert_eq!(notice.seconds_remaining, 0);
    }

    #[test]
    fn the_live_watcher_polls_the_documented_endpoint_at_the_documented_cadence() {
        let live = ScheduledEvents::live();
        assert_eq!(live.endpoint, ENDPOINT);
        assert_eq!(live.interval, POLL_INTERVAL);
    }
}
