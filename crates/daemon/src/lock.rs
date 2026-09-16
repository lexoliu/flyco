//! One `flycod` per claim, enforced by the OS rather than by convention.
//!
//! Two session daemons for one session on one machine attach to the same
//! room, and every attach supersedes the last: each daemon's re-attach
//! ends the other's command stream, which is the ping-pong that produced
//! issue #336's request flood. Two `flycod host` processes are the same
//! shape. Convention cannot stop it — `postStart` fires on every
//! codespace start and a systemd unit restarts on failure — so the guard
//! is a `flock` on a stable file: held for exactly the process's
//! lifetime, released by the kernel the moment the holder exits, crashed
//! or clean.
//!
//! The lock lives in a directory the daemon already owns rather than
//! beside the configuration: the session unit's `ProtectSystem=strict`
//! leaves `/etc/flycod` read-only, while the transcript directory is
//! writable in every configuration. And it is keyed by the session, not
//! the machine — two daemons for *different* sessions are a legitimate
//! thing to run side by side, and they attach different rooms.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation, flock};

use crate::config::DaemonConfig;

/// The file a session daemon locks inside its transcript directory.
///
/// `transcript_dir` is the one path every configuration must name and the
/// daemon must be able to write — on a provisioned machine it is
/// `/var/lib/flyco/transcripts`, one of the directories the unit's
/// `ProtectSystem=strict` still allows. The session's name is in the
/// file's so a second daemon for *another* session is not refused.
#[must_use]
pub fn session(config: &DaemonConfig) -> PathBuf {
    config
        .transcript_dir
        .join(format!("{}.lock", config.session))
}

/// The file a host daemon locks: its configuration's sibling.
///
/// `flycod host run` owns `/etc/flyco` — it is where the enrollment is
/// recorded — so `host.toml`'s lock is `host.lock` beside it. The
/// configuration itself is not the lock: `HostConfig::save` rewrites it,
/// and the lock has to outlive the file being replaced.
#[must_use]
pub fn host(config: &Path) -> PathBuf {
    config.with_extension("lock")
}

/// Proof this process is the claim's owner.
///
/// Dropping releases the lock — which is why the guard exists as a value
/// at all: the lock's lifetime *is* the run's, and a lock checked once
/// and released is a second daemon let in.
#[derive(Debug)]
pub struct DaemonLock(File);

impl DaemonLock {
    /// Records who holds the lock inside the file itself, so `cat` on it
    /// is the answer to "whose daemon is this".
    ///
    /// Best effort — the lock is held either way, and a filesystem that
    /// will not take the note is not a reason to drop the claim.
    fn note_owner(&mut self) {
        use std::io::Write as _;
        let _ = self.0.write_all(std::process::id().to_string().as_bytes());
    }
}

/// Tries to become `path`'s owner.
///
/// `Ok(Some(_))` is the guard to hold for the whole run. `Ok(None)` means
/// another live process already holds it — and the answer to that is to
/// stand down, not to retry: the holder is not going to hand the claim
/// over. `Err` is the filesystem's answer — the directory could not be
/// made or the file not opened — which the caller fails on, because
/// running unlocked is how the flood happened.
///
/// # Errors
///
/// Returns the `io::Error` the filesystem gave: the parent directory could
/// not be made, the file could not be opened, or `flock` itself failed
/// with anything but contention.
pub fn acquire(path: &Path) -> io::Result<Option<DaemonLock>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {
            let mut lock = DaemonLock(file);
            lock.note_owner();
            Ok(Some(lock))
        }
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
        Err(error) => Err(io::Error::from(error)),
    }
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;
    use std::path::PathBuf;

    /// A private directory under the test process's own temporary root.
    fn tempdir() -> PathBuf {
        let path = std::env::temp_dir().join(format!("flycod-lock-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create a temporary directory");
        path
    }

    /// The second claimant is refused while the first holds the lock, and
    /// allowed once it is dropped — the whole contract in one test.
    #[test]
    fn one_owner_at_a_time() {
        let dir = tempdir();
        let lock = dir.join("daemon.lock");

        let guard = super::acquire(&lock)
            .expect("the first acquire")
            .expect("an uncontended lock is held");
        assert!(
            super::acquire(&lock).expect("the second acquire").is_none(),
            "a held lock refuses a second claimant"
        );

        drop(guard);
        assert!(
            super::acquire(&lock).expect("the re-acquire").is_some(),
            "a dropped lock is taken again"
        );
    }

    /// A parent that does not exist yet is made rather than failed: the
    /// transcript directory is created lazily everywhere else, and the
    /// lock is no different.
    #[test]
    fn the_state_directory_is_made() {
        let lock = tempdir().join("nested").join("daemon.lock");

        assert!(
            super::acquire(&lock)
                .expect("acquire in a missing dir")
                .is_some()
        );
        assert!(lock.exists());
    }

    /// An `Err` is reserved for the filesystem itself: a lock path that
    /// is a directory cannot be opened, and that answer must not be read
    /// as "another daemon holds it".
    #[test]
    fn filesystem_failures_are_not_contention() {
        let error = super::acquire(&tempdir()).expect_err("a directory cannot be a lock");
        assert_eq!(error.kind(), ErrorKind::IsADirectory);
    }
}
