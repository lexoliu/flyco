//! Waiting for a provider to finish something it accepted.
//!
//! Every driver has the same problem in a different dialect — Azure hands
//! back an `Azure-AsyncOperation` URL, GCP a zonal operation resource, EC2
//! nothing at all but an instance state that has to be read until it settles
//! — and every one of them needs the same two decisions: how long to wait
//! before the next poll, and when to give up. Those decisions are here so
//! there is one of each rather than three that drift.
//!
//! The provider's own `Retry-After` always wins. The backoff below is what a
//! driver falls back to when the service states nothing, and it is short at
//! first because most operations finish in single-digit seconds and capped
//! low because a machine creation is minutes and polling it every ten
//! seconds costs nothing.

/// Backoff between polls when the service states no `Retry-After`.
pub const DEFAULT_BACKOFF_SECONDS: [u32; 4] = [1, 2, 5, 10];

/// Longest a single asynchronous operation is followed for.
///
/// At the ten-second polling cap this is forty minutes, which is an order of
/// magnitude beyond the minute or two a machine takes — and a measured Azure
/// spot machine sat in `Creating` for over five, so the ceiling is not
/// theoretical. Past it a driver gives up rather than polling for the life
/// of the process.
pub const MAX_POLL_ATTEMPTS: usize = 240;

/// How many polls one control-plane invocation spends on a build before
/// handing back a [`Continuation`](crate::Continuation).
///
/// A Cloudflare Worker on the free plan may make fifty subrequests in one
/// invocation, and a provision spends some of them before it starts
/// polling: the token, the region policy, the environment, the job. Twelve
/// polls is about a hundred seconds of the default backoff — the minute or
/// two most builds take, so most never yield — and leaves the invocation
/// well inside the ceiling that ended a six-minute cold-image build
/// (issue #257). The continuation that carries on gets twelve more under
/// its own budget, up to [`MAX_POLL_ATTEMPTS`] across all of them.
pub const POLLS_PER_INVOCATION: usize = 12;

/// The delay before the `attempt`-th poll, honouring `Retry-After` first.
#[must_use]
pub const fn poll_delay(retry_after: Option<u32>, attempt: usize) -> u32 {
    if let Some(seconds) = retry_after {
        return seconds;
    }
    let last = DEFAULT_BACKOFF_SECONDS.len() - 1;
    DEFAULT_BACKOFF_SECONDS[if attempt < last { attempt } else { last }]
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_BACKOFF_SECONDS, poll_delay};

    #[test]
    fn a_retry_after_beats_the_default_backoff() {
        assert_eq!(poll_delay(Some(30), 0), 30);
        assert_eq!(poll_delay(None, 0), DEFAULT_BACKOFF_SECONDS[0]);
        assert_eq!(poll_delay(None, 2), DEFAULT_BACKOFF_SECONDS[2]);
        // The backoff is capped rather than unbounded.
        assert_eq!(poll_delay(None, 99), 10);
    }
}
