//! EC2's spot instance-action notice, read over `IMDSv2`.
//!
//! Two requests rather than one: `IMDSv2` refuses an unauthenticated read, so
//! a session token is minted with a `PUT` and presented on every `GET`
//! afterwards. The token outlives thousands of polls, so it is minted once
//! and re-minted only when a read stops being accepted.
//!
//! The notice itself is the absence of a `404`: `/latest/meta-data/spot/
//! instance-action` answers `404` for the whole life of an instance that is
//! not being interrupted, and a small document the moment it is. EC2 gives
//! two minutes, and publishes the deadline as an RFC 3339 instant.
//!
//! flyco asks for *persistent* spot with an interruption behaviour of
//! `stop`, so what follows the deadline is a stopped instance with its EBS
//! root volume intact — never a terminated one.

use core::time::Duration;

use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::{
    EvictionWatcher, MetadataError, MetadataMethod, MetadataRequest, SpotNotice, now_unix, read,
    seconds_until,
};

/// Where an `IMDSv2` session token is minted.
pub const TOKEN_ENDPOINT: &str = "http://169.254.169.254/latest/api/token";

/// Where the interruption notice appears.
pub const ACTION_ENDPOINT: &str = "http://169.254.169.254/latest/meta-data/spot/instance-action";

/// Header naming how long a minted token stays valid.
const TOKEN_TTL_HEADER: &str = "X-aws-ec2-metadata-token-ttl-seconds";

/// Header carrying the token on a read.
const TOKEN_HEADER: &str = "X-aws-ec2-metadata-token";

/// How long a minted token lasts, in seconds.
///
/// The maximum `IMDSv2` allows. A session outlives it, so the token is
/// re-minted when a read is refused rather than on a timer flyco keeps —
/// the refusal is the only thing that actually says the token is spent.
const TOKEN_TTL_SECONDS: &str = "21600";

/// How often the notice endpoint is asked.
///
/// AWS's own recommendation. The window is two minutes, so five seconds
/// costs at most 4% of it and saves twelve requests a minute against an
/// endpoint that answers `404` for hours at a time.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How long one read may take before it is abandoned and retried.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// The document the endpoint answers once an interruption is scheduled.
#[derive(Debug, Deserialize)]
struct InstanceActionDocument {
    /// What EC2 will do: `stop`, `hibernate` or `terminate`.
    ///
    /// Recorded for the log rather than acted on. flyco asks for `stop` and
    /// the daemon's response is the same whichever arrives — the machine is
    /// going, and everything that outlives it has to be off the box before
    /// it does.
    action: String,
    /// When it will happen, as an RFC 3339 instant.
    time: String,
}

impl InstanceActionDocument {
    /// How long the notice leaves, in seconds.
    ///
    /// An unreadable instant is treated as *now*, for the same reason
    /// Azure's is: a window flyco invents is one the provider never
    /// promised.
    fn seconds_remaining(&self) -> u32 {
        match OffsetDateTime::parse(&self.time, &Rfc3339) {
            Ok(deadline) => {
                let unix = u64::try_from(deadline.unix_timestamp()).unwrap_or_else(|_| now_unix());
                seconds_until(unix)
            }
            Err(error) => {
                tracing::warn!(
                    time = %self.time,
                    %error,
                    "an instance-action notice's deadline did not parse; treating it as immediate"
                );
                0
            }
        }
    }
}

/// EC2's eviction watcher.
#[derive(Debug, Clone)]
pub struct InstanceAction {
    token_endpoint: String,
    action_endpoint: String,
    interval: Duration,
}

impl InstanceAction {
    /// The watcher a provisioned EC2 instance runs.
    #[must_use]
    pub fn live() -> Self {
        Self {
            token_endpoint: TOKEN_ENDPOINT.to_owned(),
            action_endpoint: ACTION_ENDPOINT.to_owned(),
            interval: POLL_INTERVAL,
        }
    }

    /// The same watcher pointed at another endpoint, for a test with its
    /// own server.
    #[cfg(test)]
    #[must_use]
    pub const fn at(token_endpoint: String, action_endpoint: String, interval: Duration) -> Self {
        Self {
            token_endpoint,
            action_endpoint,
            interval,
        }
    }

    /// Mints an `IMDSv2` session token.
    async fn token(&self) -> Result<String, MetadataError> {
        let request = MetadataRequest {
            method: MetadataMethod::Put,
            url: &self.token_endpoint,
            headers: &[(TOKEN_TTL_HEADER, TOKEN_TTL_SECONDS)],
        };
        let body = read(request, READ_TIMEOUT).await?.ok_or_else(|| {
            MetadataError::Undecodable("the token endpoint minted none".to_owned())
        })?;
        String::from_utf8(body)
            .map_err(|_| MetadataError::Undecodable("the minted token is not UTF-8".to_owned()))
    }

    /// One read with a token in hand.
    ///
    /// `Ok(None)` is the `404` an instance that is not being interrupted
    /// answers with, which is the state it is in for almost its whole life.
    async fn poll(&self, token: &str) -> Result<Option<SpotNotice>, MetadataError> {
        let request = MetadataRequest {
            method: MetadataMethod::Get,
            url: &self.action_endpoint,
            headers: &[(TOKEN_HEADER, token)],
        };
        let Some(body) = read(request, READ_TIMEOUT).await? else {
            return Ok(None);
        };

        let document: InstanceActionDocument = serde_json::from_slice(&body).map_err(|error| {
            MetadataError::Undecodable(format!("an instance-action notice did not parse: {error}"))
        })?;
        tracing::warn!(
            action = %document.action,
            at = %document.time,
            "EC2 scheduled an interruption of this instance"
        );
        Ok(Some(SpotNotice {
            seconds_remaining: document.seconds_remaining(),
        }))
    }
}

impl EvictionWatcher for InstanceAction {
    async fn watch(self) -> SpotNotice {
        // Held across polls: a token is good for hours, and re-minting one
        // every five seconds would double the traffic to say nothing new.
        // A read that stops being accepted is what expires it, because that
        // is the only evidence that it is spent.
        let mut token: Option<String> = None;
        loop {
            let held = match token.clone() {
                Some(held) => held,
                None => match self.token().await {
                    Ok(minted) => {
                        token = Some(minted.clone());
                        minted
                    }
                    Err(error) => {
                        tracing::debug!(%error, "could not mint an IMDSv2 token");
                        tokio::time::sleep(self.interval).await;
                        continue;
                    }
                },
            };

            match self.poll(&held).await {
                Ok(Some(notice)) => return notice,
                Ok(None) => {}
                Err(error) => {
                    tracing::debug!(%error, "the instance-action endpoint could not be read");
                    token = None;
                }
            }
            tokio::time::sleep(self.interval).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::{ACTION_ENDPOINT, InstanceAction, POLL_INTERVAL, TOKEN_ENDPOINT};
    use crate::spot::EvictionWatcher as _;
    use crate::testing::{MetadataEndpoint, Reply};

    /// The instant EC2 publishes, `seconds` from now.
    fn deadline_in(seconds: u64) -> String {
        use time::format_description::well_known::Rfc3339;
        time::OffsetDateTime::from_unix_timestamp(
            i64::try_from(crate::spot::now_unix() + seconds).expect("a plausible clock"),
        )
        .expect("a valid instant")
        .format(&Rfc3339)
        .expect("format an RFC 3339 instant")
    }

    async fn watcher(replies: Vec<Reply>) -> (MetadataEndpoint, InstanceAction) {
        let endpoint = MetadataEndpoint::start(replies).await;
        let token = endpoint
            .base
            .join("latest/api/token")
            .expect("a loopback token URL")
            .to_string();
        let action = endpoint
            .base
            .join("latest/meta-data/spot/instance-action")
            .expect("a loopback action URL")
            .to_string();
        let watcher = InstanceAction::at(token, action, Duration::from_millis(5));
        (endpoint, watcher)
    }

    #[tokio::test]
    async fn the_token_is_minted_once_and_presented_on_every_read() {
        let (mut endpoint, watcher) = watcher(vec![
            Reply::text("imds-session-token"),
            Reply::problem(404, "no-interruption", "nothing is scheduled"),
            Reply::text(&format!(
                r#"{{"action":"stop","time":"{}"}}"#,
                deadline_in(120)
            )),
        ])
        .await;

        let notice = tokio::time::timeout(Duration::from_secs(5), watcher.watch())
            .await
            .expect("the watcher announced the interruption");
        assert!(
            (115..=120).contains(&notice.seconds_remaining),
            "the deadline EC2 published is what the countdown reports: {}",
            notice.seconds_remaining
        );

        let minted = endpoint.next().await.expect("the token was minted");
        assert_eq!(minted.method, "PUT");
        assert!(minted.target.ends_with("/latest/api/token"));

        for _ in 0..2 {
            let polled = endpoint.next().await.expect("the endpoint was read");
            assert_eq!(polled.method, "GET");
            assert!(polled.target.ends_with("/spot/instance-action"));
        }
    }

    #[tokio::test]
    async fn a_notice_is_read_even_when_the_first_token_is_refused() {
        // A refused mint is not evidence that this instance is being
        // interrupted, so the watcher asks again rather than announcing
        // anything.
        let (_endpoint, watcher) = watcher(vec![
            Reply::problem(
                500,
                "imds-unavailable",
                "the metadata service is restarting",
            ),
            Reply::text("imds-session-token"),
            Reply::text(&format!(
                r#"{{"action":"stop","time":"{}"}}"#,
                deadline_in(120)
            )),
        ])
        .await;

        tokio::time::timeout(Duration::from_secs(5), watcher.watch())
            .await
            .expect("the watcher recovered from the refused mint");
    }

    #[test]
    fn the_live_watcher_reads_the_documented_endpoints_at_the_documented_cadence() {
        let live = InstanceAction::live();
        assert_eq!(live.token_endpoint, TOKEN_ENDPOINT);
        assert_eq!(live.action_endpoint, ACTION_ENDPOINT);
        assert_eq!(live.interval, POLL_INTERVAL);
    }
}
