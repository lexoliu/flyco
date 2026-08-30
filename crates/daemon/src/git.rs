//! The session checkout's working tree, as `git status --short` reports it.
//!
//! Dirtiness is load-bearing: an agent may not stop while the tree is
//! dirty (unless the compute budget is exhausted), a manual archive of a
//! dirty tree requires confirmation and discards the work, and an automatic
//! archive snapshots the uncommitted changes before the disk is released.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::{Duration, Interval};

/// How often a live checkout is polled.
pub const POLL: Duration = Duration::from_secs(5);

/// The working tree of a session checkout.
pub trait WorkingTree: Send {
    /// Next `git status --short` summary, when it changes.
    ///
    /// An empty string is a clean tree. `None` means the tree can no
    /// longer be observed.
    fn next_status(&mut self) -> impl Future<Output = Option<String>> + Send;

    /// A binary diff of every uncommitted change, including untracked
    /// files, or `None` when the tree is clean.
    ///
    /// Staging is temporary: the index is reset even if the diff fails, so
    /// a snapshot never leaves the checkout dirty in a new way.
    fn snapshot(&self) -> impl Future<Output = Result<Option<Vec<u8>>, GitError>> + Send;

    /// Applies a previously stored snapshot onto this checkout.
    fn apply(&self, patch: &[u8]) -> impl Future<Output = Result<(), GitError>> + Send;
}

/// Why a git command failed.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// The git process could not be started, or its pipes failed.
    #[error("could not run git: {0}")]
    Spawn(#[source] std::io::Error),
    /// git exited with a non-zero status.
    #[error("git {command} failed: {detail}")]
    Failed {
        /// The git subcommand that failed.
        command: String,
        /// stderr, or a note when it was empty.
        detail: String,
    },
}

/// A checkout on disk, polled with `git status --short`.
pub struct GitWorkdir {
    path: PathBuf,
    interval: Interval,
    last: Option<String>,
}

impl core::fmt::Debug for GitWorkdir {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GitWorkdir")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl GitWorkdir {
    /// Watches `path` as a git checkout.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            interval: tokio::time::interval(POLL),
            last: None,
        }
    }

    /// A snapshot handle plus a stream of status summaries.
    ///
    /// The watcher runs on its own task so the relay can poll the stream
    /// without borrowing the handle it snapshots from.
    #[must_use]
    pub fn spawn(path: PathBuf) -> (Self, mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut watch = Self::new(path.clone());
        tokio::spawn(async move {
            while let Some(summary) = watch.next_status().await {
                if tx.send(summary).is_err() {
                    break;
                }
            }
        });
        (Self::new(path), rx)
    }

    async fn status_of(&self) -> Result<String, GitError> {
        let output = git(&self.path, &["status", "--short"]).await?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

impl WorkingTree for GitWorkdir {
    async fn next_status(&mut self) -> Option<String> {
        loop {
            self.interval.tick().await;
            match self.status_of().await {
                Ok(summary) if self.last.as_ref() != Some(&summary) => {
                    self.last = Some(summary.clone());
                    return Some(summary);
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::debug!(
                        path = %self.path.display(),
                        %error,
                        "git status was not readable; retrying"
                    );
                }
            }
        }
    }

    async fn snapshot(&self) -> Result<Option<Vec<u8>>, GitError> {
        git(&self.path, &["add", "-A"]).await?;
        let diff = git(&self.path, &["diff", "--cached", "--binary"]).await;
        let reset = git(&self.path, &["reset"]).await;
        reset?;
        let output = diff?;
        if output.stdout.is_empty() {
            Ok(None)
        } else {
            Ok(Some(output.stdout))
        }
    }

    async fn apply(&self, patch: &[u8]) -> Result<(), GitError> {
        let mut child = Command::new("git")
            .current_dir(&self.path)
            .args(["apply", "--binary", "--whitespace=nowarn"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(GitError::Spawn)?;
        let mut stdin = child.stdin.take().ok_or_else(|| GitError::Failed {
            command: "apply".to_owned(),
            detail: "stdin was not piped".to_owned(),
        })?;
        stdin.write_all(patch).await.map_err(GitError::Spawn)?;
        drop(stdin);
        let output = child.wait_with_output().await.map_err(GitError::Spawn)?;
        finished("apply".to_owned(), output).map(|_| ())
    }
}

/// A stand-in checkout for relay tests.
pub struct FakeWorkdir {
    snapshot: Option<Vec<u8>>,
}

impl core::fmt::Debug for FakeWorkdir {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FakeWorkdir").finish_non_exhaustive()
    }
}

impl FakeWorkdir {
    /// A pair: the handle the relay snapshots from, the sender tests inject
    /// status summaries through, and the stream the relay reads.
    #[must_use]
    pub fn pair() -> (
        Self,
        mpsc::UnboundedSender<String>,
        mpsc::UnboundedReceiver<String>,
    ) {
        Self::with_snapshot(None)
    }

    /// Like [`Self::pair`], but [`WorkingTree::snapshot`] returns `patch`.
    #[must_use]
    pub fn with_snapshot(
        patch: Option<Vec<u8>>,
    ) -> (
        Self,
        mpsc::UnboundedSender<String>,
        mpsc::UnboundedReceiver<String>,
    ) {
        let (sender, statuses) = mpsc::unbounded_channel();
        (Self { snapshot: patch }, sender, statuses)
    }
}

impl WorkingTree for FakeWorkdir {
    fn next_status(&mut self) -> impl Future<Output = Option<String>> + Send {
        core::future::ready(None)
    }

    fn snapshot(&self) -> impl Future<Output = Result<Option<Vec<u8>>, GitError>> + Send {
        core::future::ready(Ok(self.snapshot.clone()))
    }

    fn apply(&self, _patch: &[u8]) -> impl Future<Output = Result<(), GitError>> + Send {
        core::future::ready(Ok(()))
    }
}

async fn git(path: &Path, args: &[&str]) -> Result<std::process::Output, GitError> {
    let command = args.first().copied().unwrap_or("git").to_owned();
    let output = Command::new("git")
        .current_dir(path)
        .args(args)
        .output()
        .await
        .map_err(GitError::Spawn)?;
    finished(command, output)
}

fn finished(
    command: String,
    output: std::process::Output,
) -> Result<std::process::Output, GitError> {
    if output.status.success() {
        Ok(output)
    } else {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(GitError::Failed {
            command,
            detail: if detail.is_empty() {
                output.status.to_string()
            } else {
                detail
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{GitWorkdir, WorkingTree};
    use uuid::Uuid;

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("flyco-git-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("scratch checkout");
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn git(path: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(path)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init(path: &std::path::Path) {
        git(path, &["init"]);
        git(path, &["config", "user.email", "me@lexo.cool"]);
        git(path, &["config", "user.name", "Lexo Liu"]);
        std::fs::write(path.join("README.md"), "flyco\n").expect("write");
        git(path, &["add", "README.md"]);
        git(path, &["commit", "-m", "init"]);
    }

    #[tokio::test]
    async fn a_clean_tree_snapshots_as_none() {
        let scratch = Scratch::new();
        init(&scratch.0);
        let workdir = GitWorkdir::new(scratch.0.clone());
        assert_eq!(workdir.snapshot().await.expect("snapshot"), None);
    }

    #[tokio::test]
    async fn an_uncommitted_file_is_dirty_and_in_the_snapshot() {
        let scratch = Scratch::new();
        init(&scratch.0);
        std::fs::write(scratch.0.join("new.txt"), "hello\n").expect("write");
        let workdir = GitWorkdir::new(scratch.0.clone());
        let before = std::process::Command::new("git")
            .current_dir(&scratch.0)
            .args(["status", "--short"])
            .output()
            .expect("status");
        assert!(
            String::from_utf8_lossy(&before.stdout).contains("new.txt"),
            "short status names the untracked file"
        );
        let patch = workdir
            .snapshot()
            .await
            .expect("snapshot")
            .expect("dirty trees produce a patch");
        let text = String::from_utf8_lossy(&patch);
        assert!(
            text.contains("new.txt"),
            "the binary diff names the new file: {text}"
        );
        let after = std::process::Command::new("git")
            .current_dir(&scratch.0)
            .args(["status", "--short"])
            .output()
            .expect("status after snapshot");
        assert_eq!(
            after.stdout, before.stdout,
            "a snapshot must not leave the index staged"
        );
    }
}
