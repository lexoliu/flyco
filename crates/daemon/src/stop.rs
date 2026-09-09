//! The platform asking this machine to go, and what the daemon does with
//! the seconds it is given.
//!
//! The counterpart of [`crate::spot`] for a machine with no disk. A spot
//! notice arrives on a provider's metadata endpoint and leaves a disk
//! behind; a `SIGTERM` on a managed container arrives on the process itself
//! and leaves nothing — Azure Container Apps, Cloud Run and ECS Fargate all
//! stop an execution by sending it, and all three follow with `SIGKILL`
//! about thirty seconds later.
//!
//! # Only where it means something
//!
//! [`watch`] returns a channel that never yields on a
//! [`Runtime::Vm`](flyco_core::Runtime::Vm), and that is the whole design
//! rather than an optimisation. A VM's `SIGTERM` is systemd stopping a unit
//! on a disk that will still be there when the machine starts again, so the
//! sequence a container needs — flush, snapshot the working tree, report —
//! would be thirty seconds of work to save something that was never at
//! risk. The signal handler is therefore installed on exactly the machines
//! whose filesystem is about to disappear.
//!
//! # One signal, once
//!
//! Like an eviction notice, a stop happens to a machine exactly once: the
//! channel holds one message and the task that fed it ends. A second
//! `SIGTERM` is the platform repeating itself, and the daemon is already
//! doing the only thing it can.

use flyco_core::{Runtime, StopReason};
use tokio::sync::mpsc;

/// How long the daemon may spend saving a session it is losing.
///
/// Five seconds under the shortest grace period of the three services that
/// send the signal — ACA, Cloud Run and Fargate all document thirty — so a
/// sequence that runs long is cut off by flyco, with a log line saying so,
/// rather than by a `SIGKILL` that leaves no trace at all.
pub const GRACE: core::time::Duration = core::time::Duration::from_secs(25);

/// Where a stop signal reaches the relay from.
///
/// A channel rather than a signal stream held by the relay: installing a
/// handler is a decision about the *machine*, made once where the
/// configuration is read, and what the relay needs from it is one value.
/// The receiver a machine whose disk survives gets is a closed one, which
/// never yields — see [`watch`].
pub type Stops = mpsc::Receiver<StopReason>;

/// Watches for the platform's stop signal, on the runtimes it means
/// something on.
///
/// # Panics
///
/// Panics if the `SIGTERM` handler cannot be installed, which is this
/// process failing to register with its own kernel: a container that
/// silently did not watch would lose the user's uncommitted work the first
/// time the platform stopped it, and failing to start is the smaller loss.
#[must_use]
pub fn watch(runtime: Runtime) -> Stops {
    let (stops, receiver) = mpsc::channel(1);
    if runtime.keeps_disk() {
        tracing::info!(
            "this machine's filesystem outlives a stop; SIGTERM needs no session snapshot"
        );
        drop(stops);
        return receiver;
    }

    let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("flycod must be able to watch for SIGTERM on a machine with no disk");
    tokio::spawn(async move {
        if signal.recv().await.is_none() {
            tracing::warn!("the SIGTERM stream ended; nothing will announce this machine's stop");
            return;
        }
        tracing::warn!("the platform asked this container to stop; saving the session");
        if stops.send(StopReason::Sigterm).await.is_err() {
            tracing::warn!("nothing was listening for the stop signal");
        }
    });
    receiver
}

/// A closed channel, for a session nothing will signal.
///
/// What the [REPL](crate::repl) and every test that drives a relay without a
/// platform under it gets.
#[must_use]
pub fn nothing_to_watch() -> Stops {
    let (stops, receiver) = mpsc::channel(1);
    drop(stops);
    receiver
}

#[cfg(test)]
mod tests {
    use super::{GRACE, nothing_to_watch, watch};
    use flyco_core::Runtime;

    #[tokio::test]
    async fn a_machine_with_a_disk_watches_nothing() {
        let mut stops = watch(Runtime::Vm);
        assert!(
            stops.recv().await.is_none(),
            "a VM's SIGTERM is a shutdown onto a disk that survives it"
        );
        assert!(nothing_to_watch().recv().await.is_none());
    }

    #[test]
    fn the_grace_period_leaves_room_under_the_platform_s_own() {
        // ACA, Cloud Run and Fargate all document thirty seconds. Anything
        // at or above that is a sequence `SIGKILL` interrupts.
        assert!(GRACE < core::time::Duration::from_secs(30));
    }
}
