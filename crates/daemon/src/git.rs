//! The session checkout: putting it on the machine, and watching it.
//!
//! [`clone_into`] is what makes a session VM more than an empty directory —
//! the agent's whole job is the repository, so the checkout has to exist
//! before the harness is started in it.
//!
//! After that, dirtiness is load-bearing: an agent may not stop while the
//! tree is dirty (unless the compute budget is exhausted), a manual archive
//! of a dirty tree requires confirmation and discards the work, and an
//! automatic archive snapshots everything the clone does not already have —
//! unpushed commits included — before the disk is released.
//!
//! # Where the GitHub token lives
//!
//! Nowhere on disk, and in no log line. `git clone` over HTTPS needs a
//! credential, and every obvious way of supplying one leaves it somewhere it
//! outlives the clone: in the remote URL it writes into `.git/config`, in a
//! `~/.git-credentials` file, in the process listing of the machine the
//! agent itself has a shell on. So the token is passed to the child process
//! as an *environment variable* and read back out of the environment by
//! [`CREDENTIAL_HELPER`], a one-line shell helper installed with `-c` for
//! the duration of that one command. Nothing it writes contains the token,
//! and nothing that survives the command can produce it.

use std::ffi::OsStr;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::{Duration, Interval};

use std::collections::BTreeMap;

use crate::config::{GithubAccess, RepoConfig};

/// How often a live checkout is polled.
pub const POLL: Duration = Duration::from_secs(5);

/// Environment variable the credential helper reads the token out of.
pub const TOKEN_VAR: &str = "FLYCO_GIT_TOKEN";

/// The credential helper git runs when a remote asks who is calling.
///
/// Git appends the operation (`get`, `store`, `erase`) to a `!`-prefixed
/// helper and runs the result through `sh`, so `$1` is the operation and
/// only `get` answers: flyco has nothing to store and nothing to erase,
/// because there is no store.
///
/// `x-access-token` is the username GitHub documents for a token presented
/// over HTTPS basic auth; the token is the password, and it is read from the
/// environment rather than baked in here, so this string is safe to log,
/// print, or write to a config file — which is precisely why it is the thing
/// git sees.
const CREDENTIAL_HELPER: &str = "!f() { test \"$1\" = get && printf 'username=x-access-token\\npassword=%s\\n' \
     \"$FLYCO_GIT_TOKEN\"; }; f";

/// Whether `workdir` already holds a git checkout.
///
/// What tells a machine's first boot from its next one. A session VM whose
/// compute was reclaimed keeps its disk — every provider flyco puts spot
/// capacity on stops the machine rather than deleting it — so the daemon
/// that comes up after the restart finds the repository, the agent's
/// uncommitted work, and every cache exactly where they were. Cloning over
/// that is not possible (git refuses a non-empty destination) and would not
/// be wanted if it were: the working tree is the thing the reclamation was
/// careful to keep.
///
/// The same check makes an `AddRepo` command idempotent: the control plane
/// holds the command for a disconnected daemon and may deliver it again
/// after a reconnect, so a directory that is already a checkout is adopted
/// rather than cloned over.
pub async fn has_checkout(workdir: &Path) -> bool {
    tokio::fs::metadata(workdir.join(".git")).await.is_ok()
}

/// Clones a session's repository into `checkout`.
///
/// `checkout` is the directory the clone creates — `workdir/<dir>` for the
/// repository's configured `dir` — not the shared workdir itself: a session
/// can carry several repositories, and each keeps its own root.
///
/// The branch is checked out by name rather than fetched and switched to:
/// a session names one branch per checkout for its whole life, and a clone
/// that pulled every branch would spend a session VM's first minute on
/// history nothing is going to read.
///
/// The commit identity is written into the checkout's own `.git/config`
/// rather than a global one, so it applies to this repository and says who
/// the session is on behalf of.
///
/// # Errors
///
/// Returns [`GitError`] if git could not be started or refused — an
/// unreachable remote, a token the repository does not admit, a branch that
/// does not exist, or a checkout that already holds something. Every one of
/// those is a session failure rather than something to work around, and the
/// error carries git's own words so the user is told which.
pub async fn clone_into(
    repo: &RepoConfig,
    access: &GithubAccess,
    checkout: &Path,
) -> Result<(), GitError> {
    let remote = repo.remote_url();
    tracing::info!(
        slug = %repo.slug,
        branch = %repo.branch,
        checkout = %checkout.display(),
        "cloning the session's repository"
    );
    clone_from(&remote, repo, access, checkout).await
}

/// Clones `remote` — the URL split out from [`clone_into`] so a test can
/// point it at a bare repository on disk instead of at `github.com`.
///
/// # Errors
///
/// Returns [`GitError`] exactly as [`clone_into`] does.
pub async fn clone_from(
    remote: &str,
    repo: &RepoConfig,
    access: &GithubAccess,
    checkout: &Path,
) -> Result<(), GitError> {
    // `--` before the positional arguments, and `--branch=` rather than a
    // separate value: a branch name is user input, and neither it nor a
    // remote URL may be read as an option. `BranchName` already refuses a
    // leading dash; this is the second lock on the same door.
    //
    // `--recurse-submodules` because a checkout without them is one whose
    // manifest lists members that are not there — cargo refuses to even
    // load the workspace. The clone's `-c credential.helper` and the token
    // environment reach each submodule fetch through `GIT_CONFIG_PARAMETERS`
    // and ordinary environment inheritance, so a private submodule
    // authenticates exactly as its parent did.
    let branch = format!("--branch={}", repo.branch);
    authenticated(
        access,
        &[
            OsStr::new("clone"),
            OsStr::new("--recurse-submodules"),
            OsStr::new(&branch),
            OsStr::new("--"),
            OsStr::new(remote),
            checkout.as_os_str(),
        ],
        Path::new("."),
    )
    .await?;

    git(
        checkout,
        &["config", "user.name", access.identity.name.as_str()],
    )
    .await?;
    git(
        checkout,
        &["config", "user.email", access.identity.email.as_str()],
    )
    .await?;
    // The tip the clone landed on is the base a later snapshot diffs
    // against — a workdir patch must carry the session's commits, not just
    // whatever was staged when the disk went away.
    git(checkout, &["update-ref", BASE_REF, "HEAD"]).await?;
    Ok(())
}

/// The ref a clone records its landing tip under.
///
/// Snapshots diff against it rather than `HEAD`: an agent that committed
/// its work is exactly the one with the most to lose when the machine goes
/// away, and a diff against `HEAD` would call that work clean.
const BASE_REF: &str = "refs/flyco/base";

/// Runs one git command that may have to authenticate to GitHub.
///
/// The token reaches git through [`TOKEN_VAR`] and [`CREDENTIAL_HELPER`] and
/// through nothing else. `credential.helper=` (empty) first, because git
/// *appends* helpers: without the reset, a helper configured system-wide on
/// the machine would be consulted before flyco's and could answer with
/// somebody else's credential.
///
/// `GIT_TERMINAL_PROMPT=0` turns a credential git cannot satisfy into an
/// immediate failure rather than a process waiting on a terminal no session
/// VM has.
async fn authenticated(
    access: &GithubAccess,
    args: &[&OsStr],
    current_dir: &Path,
) -> Result<std::process::Output, GitError> {
    let command = args
        .first()
        .and_then(|arg| arg.to_str())
        .unwrap_or("git")
        .to_owned();
    let output = Command::new("git")
        .current_dir(current_dir)
        .arg("-c")
        .arg("credential.helper=")
        .arg("-c")
        .arg(format!("credential.helper={CREDENTIAL_HELPER}"))
        .args(args)
        .env(TOKEN_VAR, &access.token)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await
        .map_err(GitError::Spawn)?;
    finished(command, output)
}

/// Rewinds `workdir`'s checked-out branch to `commit`.
///
/// A stored patch was diffed against the commit its provenance names — a
/// handoff's merge-base, or a snapshot's recorded base — not the tip this
/// clone landed on, so the branch is wound back before the patch applies.
/// On a fresh clone the tree is clean, and `reset --hard` only moves the
/// ref.
///
/// # Errors
///
/// Returns [`GitError`] if `commit` is not in the clone — a base a
/// force-push removed between send and provision is a session failure,
/// not something to patch around.
pub async fn reset_to(workdir: &Path, commit: &str) -> Result<(), GitError> {
    git(workdir, &["reset", "--hard", commit]).await.map(|_| ())
}

/// The commit a snapshot diffs against.
///
/// [`BASE_REF`] is the answer a flyco clone leaves behind. A checkout it
/// never wrote — one made before the marker existed — falls back to the
/// nearest ancestor `origin/HEAD` still shares, and finally to `HEAD`
/// itself, which is the uncommitted-only shape a baseless tree can still
/// honestly produce.
async fn snapshot_base(path: &Path) -> Result<String, GitError> {
    if let Some(base) = rev_parse(path, BASE_REF).await? {
        return Ok(base);
    }
    if let Some(base) = merge_base(path).await? {
        return Ok(base);
    }
    rev_parse(path, "HEAD")
        .await?
        .ok_or_else(|| GitError::Failed {
            command: "rev-parse".to_owned(),
            detail: "HEAD does not resolve".to_owned(),
        })
}

/// The commit `rev` resolves to in `path`, or `None` when it does not.
///
/// `rev-parse --verify --quiet` exits `1` on an unresolved name — an
/// answer, not a failure — so anything louder still propagates.
async fn rev_parse(path: &Path, rev: &str) -> Result<Option<String>, GitError> {
    let output = run_git(
        path,
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new("--quiet"),
            OsStr::new(rev),
        ],
        &[],
        &[1],
    )
    .await?;
    let resolved = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((!resolved.is_empty()).then_some(resolved))
}

/// The nearest ancestor `HEAD` and `origin/HEAD` still share, when they
/// share one — `merge-base` exits `1` when they do not, and `128` when
/// `origin/HEAD` is a name the clone never recorded.
async fn merge_base(path: &Path) -> Result<Option<String>, GitError> {
    let output = run_git(
        path,
        &[
            OsStr::new("merge-base"),
            OsStr::new("HEAD"),
            OsStr::new("origin/HEAD"),
        ],
        &[],
        &[1, 128],
    )
    .await?;
    let shared = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((!shared.is_empty()).then_some(shared))
}

/// The working tree of one session checkout.
pub trait WorkingTree: Send {
    /// Next `git status --short` summary, when it changes.
    ///
    /// An empty string is a clean tree. `None` means the tree can no
    /// longer be observed.
    fn next_status(&mut self) -> impl Future<Output = Option<String>> + Send;

    /// A binary diff of everything the session added on top of its clone —
    /// committed work included — or `None` when the tree still matches the
    /// clone's landing commit.
    ///
    /// Staging is temporary: the index is reset even if the diff fails, so
    /// a snapshot never leaves the checkout dirty in a new way.
    fn snapshot(&self) -> impl Future<Output = Result<Option<WorkdirSnapshot>, GitError>> + Send;

    /// Applies a previously stored snapshot onto this checkout.
    fn apply(&self, patch: &[u8]) -> impl Future<Output = Result<(), GitError>> + Send;
}

/// A working tree carried off its machine: the commit it was diffed
/// against and the binary patch that reproduces it.
///
/// `base_commit` is the tip the session's clone landed on, recorded as
/// [`BASE_REF`]. Diffing against it — rather than `HEAD` — is what puts
/// the session's own commits into the patch; a diff against `HEAD` reads
/// a fully committed tree as clean and would carry nothing.
#[derive(Clone)]
pub struct WorkdirSnapshot {
    /// The commit `patch` was diffed against — the clone's landing tip.
    pub base_commit: String,
    /// `git diff --cached --binary <base_commit>` of the staged tree.
    pub patch: Vec<u8>,
}

impl WorkdirSnapshot {
    /// The stored shape: one line of lowercase hex — the base commit —
    /// then the patch verbatim. Carrying the base inside the object means
    /// the resume path needs nothing else to rewind the fresh clone before
    /// it applies the diff.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(self.base_commit.len() + 1 + self.patch.len());
        body.extend_from_slice(self.base_commit.as_bytes());
        body.push(b'\n');
        body.extend_from_slice(&self.patch);
        body
    }
}

impl core::fmt::Debug for WorkdirSnapshot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WorkdirSnapshot")
            .field("base_commit", &self.base_commit)
            .field("patch_bytes", &self.patch.len())
            .finish()
    }
}

/// Splits a stored workdir patch back into its base commit and diff.
///
/// A `git diff` body opens with `diff --git`, never a bare hash, so a
/// first line of exactly forty hex characters is unambiguously the base
/// line. Anything else is a patch stored before snapshots carried one —
/// `None`, and the whole body is the diff.
#[must_use]
pub fn decode_snapshot(body: &[u8]) -> (Option<&str>, &[u8]) {
    let Some(line_end) = body.iter().position(|byte| *byte == b'\n') else {
        return (None, body);
    };
    let (head, rest) = body.split_at(line_end + 1);
    let is_base = head.len() == 41 && head[..40].iter().all(u8::is_ascii_hexdigit);
    if !is_base {
        return (None, body);
    }
    // Forty hex digits are ASCII — the line reads as text by construction.
    std::str::from_utf8(&head[..40]).map_or((None, body), |base| (Some(base), rest))
}

/// The session's checkouts, as the relay drives them.
///
/// The plural counterpart of [`WorkingTree`]: a session can work across
/// several repositories, and everything the relay does with the working
/// tree — dirty reporting, snapshotting on stop, applying a stored patch —
/// is answered per checkout directory, the `dir` the control plane
/// recorded for it. The directory key is `Option<String>` because a
/// developer machine's checkout *is* the workspace root, which has no
/// name of its own — `None` keys it, matching the wire's
/// [`RepoDirty`](flyco_core::DaemonToControl::RepoDirty).
pub trait WorkingSet: Send {
    /// Next `(dir, status)` pair from any checkout, when it changes.
    ///
    /// `dir` is the checkout's workspace-relative directory, or `None` for
    /// the root checkout of a developer machine; an empty summary is a
    /// clean tree. `None` as the whole answer means no checkout can be
    /// observed any more.
    fn next_status(&mut self) -> impl Future<Output = Option<(Option<String>, String)>> + Send;

    /// Every checkout directory this set watches.
    fn dirs(&self) -> Vec<Option<String>>;

    /// Clones `repo` into its configured directory under the workspace and
    /// begins watching it.
    ///
    /// `Ok(true)` is a checkout the set did not know — the wire's cue to
    /// announce it. `Ok(false)` means the directory was already watched:
    /// an `AddRepo` replay, which must not announce again. A directory
    /// whose contents already form a checkout is adopted rather than
    /// cloned over.
    fn clone_repo(
        &mut self,
        repo: &RepoConfig,
        access: &GithubAccess,
    ) -> impl Future<Output = Result<bool, GitError>> + Send;

    /// A [`WorkdirSnapshot`] of `dir`, or `None` when it is clean.
    ///
    /// Same staging discipline as [`WorkingTree::snapshot`]: the index is
    /// reset even when the diff fails.
    fn snapshot(
        &self,
        dir: &Option<String>,
    ) -> impl Future<Output = Result<Option<WorkdirSnapshot>, GitError>> + Send;

    /// Applies a previously stored snapshot onto `dir`.
    fn apply(
        &self,
        dir: &Option<String>,
        patch: &[u8],
    ) -> impl Future<Output = Result<(), GitError>> + Send;
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
    /// The named directory holds none of the session's checkouts.
    ///
    /// Raised by [`WorkingSet::snapshot`] and [`WorkingSet::apply`] when a
    /// `dir` names no watched checkout — a stale `AddRepo`, or a stored
    /// patch written for a directory this boot never cloned.
    #[error("no checkout lives in `{0}`")]
    UnknownCheckout(String),
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

    /// `git status --short`, read once.
    ///
    /// What the [watcher](WorkingTree::next_status) polls, and what a
    /// one-shot caller asks for instead of subscribing: the MCP server is a
    /// process that answers one tool call and exits, and it has to know
    /// whether a resize would take uncommitted work with it.
    ///
    /// # Errors
    ///
    /// Returns [`GitError`] if git could not be run or the directory is not
    /// a checkout.
    pub async fn read_status(&self) -> Result<String, GitError> {
        let output = git(&self.path, &["status", "--short"]).await?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

impl WorkingTree for GitWorkdir {
    async fn next_status(&mut self) -> Option<String> {
        loop {
            self.interval.tick().await;
            match self.read_status().await {
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

    async fn snapshot(&self) -> Result<Option<WorkdirSnapshot>, GitError> {
        let base_commit = snapshot_base(&self.path).await?;
        git(&self.path, &["add", "-A"]).await?;
        let diff = git(
            &self.path,
            &["diff", "--cached", "--binary", base_commit.as_str()],
        )
        .await;
        let reset = git(&self.path, &["reset"]).await;
        reset?;
        let output = diff?;
        if output.stdout.is_empty() {
            Ok(None)
        } else {
            Ok(Some(WorkdirSnapshot {
                base_commit,
                patch: output.stdout,
            }))
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

/// The session's checkouts on disk: one [`GitWorkdir`] per `dir`, with
/// their status streams folded into one.
///
/// Built at boot from `[[repos]]` — [`crate::main`] clones or adopts each
/// configured checkout — and grown by `clone_repo` as the control plane
/// approves more. `root` is the workspace every `dir` lives under; the
/// `None` key is the developer-machine shape, where the root itself is
/// the checkout.
pub struct GitRepos {
    root: PathBuf,
    workdirs: BTreeMap<Option<String>, GitWorkdir>,
    status_tx: mpsc::UnboundedSender<(Option<String>, String)>,
    status_rx: mpsc::UnboundedReceiver<(Option<String>, String)>,
}

impl core::fmt::Debug for GitRepos {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GitRepos")
            .field("dirs", &self.workdirs.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl GitRepos {
    /// An empty set over `root`; checkouts join through
    /// [`Self::watch`] or [`WorkingSet::clone_repo`].
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        let (status_tx, status_rx) = mpsc::unbounded_channel();
        Self {
            root,
            workdirs: BTreeMap::new(),
            status_tx,
            status_rx,
        }
    }

    /// Begins watching the checkout at `path` under the name `dir`.
    ///
    /// Boot-time registration: [`crate::main`] calls it once per
    /// configured repo after its clone-or-adopt, and once with `None` on
    /// a developer machine whose workspace root is itself the checkout.
    pub fn watch(&mut self, dir: Option<String>, path: PathBuf) {
        let (workdir, mut statuses) = GitWorkdir::spawn(path);
        let tx = self.status_tx.clone();
        let label = dir.clone();
        tokio::spawn(async move {
            while let Some(summary) = statuses.recv().await {
                if tx.send((label.clone(), summary)).is_err() {
                    break;
                }
            }
        });
        self.workdirs.insert(dir, workdir);
    }
}

impl WorkingSet for GitRepos {
    async fn next_status(&mut self) -> Option<(Option<String>, String)> {
        self.status_rx.recv().await
    }

    fn dirs(&self) -> Vec<Option<String>> {
        self.workdirs.keys().cloned().collect()
    }

    async fn clone_repo(
        &mut self,
        repo: &RepoConfig,
        access: &GithubAccess,
    ) -> Result<bool, GitError> {
        let dir = Some(repo.dir.clone());
        if self.workdirs.contains_key(&dir) {
            return Ok(false);
        }
        clone_into(repo, access, &repo.checkout_path(&self.root)).await?;
        self.watch(dir, repo.checkout_path(&self.root));
        Ok(true)
    }

    async fn snapshot(&self, dir: &Option<String>) -> Result<Option<WorkdirSnapshot>, GitError> {
        self.workdirs
            .get(dir)
            .ok_or_else(|| {
                GitError::UnknownCheckout(dir.clone().unwrap_or_else(|| ".".to_owned()))
            })?
            .snapshot()
            .await
    }

    async fn apply(&self, dir: &Option<String>, patch: &[u8]) -> Result<(), GitError> {
        self.workdirs
            .get(dir)
            .ok_or_else(|| {
                GitError::UnknownCheckout(dir.clone().unwrap_or_else(|| ".".to_owned()))
            })?
            .apply(patch)
            .await
    }
}

/// A stand-in checkout set for relay tests.
///
/// One status sender serves every directory: a test injects a dirty report
/// by sending `(dir, summary)`, the same pair the wire carries, so a test
/// does not have to hold a channel per checkout.
pub struct FakeRepos {
    snapshots: BTreeMap<Option<String>, Option<WorkdirSnapshot>>,
    /// Dirs a `clone_repo` must refuse, so a test can drive the
    /// failed-clone path without a network.
    unclonable: std::collections::BTreeSet<String>,
    status_rx: mpsc::UnboundedReceiver<(Option<String>, String)>,
}

impl core::fmt::Debug for FakeRepos {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FakeRepos")
            .field("dirs", &self.snapshots.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl FakeRepos {
    /// A pair: the set the relay drives, and the sender tests inject
    /// `(dir, summary)` status reports through. Every `dir` starts clean.
    #[must_use]
    pub fn pair(
        dirs: &[Option<String>],
    ) -> (Self, mpsc::UnboundedSender<(Option<String>, String)>) {
        Self::with_snapshots(dirs.iter().map(|dir| (dir.clone(), None)))
    }

    /// Like [`Self::pair`], but each `dir`'s [`WorkingSet::snapshot`]
    /// returns its patch, diffed against a stand-in base.
    #[must_use]
    pub fn with_snapshots(
        snapshots: impl IntoIterator<Item = (Option<String>, Option<Vec<u8>>)>,
    ) -> (Self, mpsc::UnboundedSender<(Option<String>, String)>) {
        let (status_tx, status_rx) = mpsc::unbounded_channel();
        (
            Self {
                snapshots: snapshots
                    .into_iter()
                    .map(|(dir, patch)| {
                        (
                            dir,
                            patch.map(|patch| WorkdirSnapshot {
                                base_commit: "0".repeat(40),
                                patch,
                            }),
                        )
                    })
                    .collect(),
                unclonable: std::collections::BTreeSet::new(),
                status_rx,
            },
            status_tx,
        )
    }

    /// Every `clone_repo` naming one of these directories fails.
    #[must_use]
    pub fn unclonable(mut self, dirs: &[&str]) -> Self {
        self.unclonable
            .extend(dirs.iter().map(|dir| (*dir).to_owned()));
        self
    }
}

impl WorkingSet for FakeRepos {
    async fn next_status(&mut self) -> Option<(Option<String>, String)> {
        self.status_rx.recv().await
    }

    fn dirs(&self) -> Vec<Option<String>> {
        self.snapshots.keys().cloned().collect()
    }

    fn clone_repo(
        &mut self,
        repo: &RepoConfig,
        _access: &GithubAccess,
    ) -> impl Future<Output = Result<bool, GitError>> + Send {
        let answer = if self.snapshots.contains_key(&Some(repo.dir.clone())) {
            Ok(false)
        } else if self.unclonable.contains(&repo.dir) {
            Err(GitError::Failed {
                command: "clone".to_owned(),
                detail: format!("{} was refused by the test", repo.slug),
            })
        } else {
            self.snapshots.insert(Some(repo.dir.clone()), None);
            Ok(true)
        };
        core::future::ready(answer)
    }

    fn snapshot(
        &self,
        dir: &Option<String>,
    ) -> impl Future<Output = Result<Option<WorkdirSnapshot>, GitError>> + Send {
        core::future::ready(self.snapshots.get(dir).cloned().ok_or_else(|| {
            GitError::UnknownCheckout(dir.clone().unwrap_or_else(|| ".".to_owned()))
        }))
    }

    fn apply(
        &self,
        dir: &Option<String>,
        _patch: &[u8],
    ) -> impl Future<Output = Result<(), GitError>> + Send {
        core::future::ready(if self.snapshots.contains_key(dir) {
            Ok(())
        } else {
            Err(GitError::UnknownCheckout(
                dir.clone().unwrap_or_else(|| ".".to_owned()),
            ))
        })
    }
}

async fn git(path: &Path, args: &[&str]) -> Result<std::process::Output, GitError> {
    let args: Vec<&OsStr> = args.iter().copied().map(OsStr::new).collect();
    run_git(path, &args, &[], &[]).await
}

/// Runs one git command in `path`, and says what it wrote.
///
/// The shared runner behind every plain git call this crate makes.
/// [`crate::workdir`] needs the two things a status poll does not: an
/// environment (`GIT_INDEX_FILE`, so a diff can stage into an index that is
/// not the checkout's) and a set of exit codes that are answers rather than
/// failures — `check-ignore` exits 1 when nothing is ignored, and
/// `rev-parse --verify` exits 1 when a ref does not exist.
pub(crate) async fn run_git(
    path: &Path,
    args: &[&OsStr],
    env: &[(&str, &OsStr)],
    also_ok: &[i32],
) -> Result<std::process::Output, GitError> {
    let command = args
        .first()
        .and_then(|arg| arg.to_str())
        .unwrap_or("git")
        .to_owned();
    let mut process = Command::new("git");
    process.current_dir(path).args(args);
    for (name, value) in env {
        process.env(name, value);
    }
    let output = process.output().await.map_err(GitError::Spawn)?;
    if output
        .status
        .code()
        .is_some_and(|code| also_ok.contains(&code))
    {
        return Ok(output);
    }
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
    use super::{GitWorkdir, Stdio, WorkingTree, clone_from};
    use crate::config::{GitIdentity, GithubAccess, RepoConfig};
    use uuid::Uuid;

    /// The token every clone test authenticates with.
    ///
    /// A test's origin is a bare repository on disk, which needs no
    /// credential — so a token that reaches git at all would be one flyco
    /// leaked. That is exactly what these tests look for.
    const TOKEN: &str = "gho_a-token-that-must-not-escape";

    /// The identity a cloned checkout must be configured to commit as.
    const COMMIT_NAME: &str = "lexoliu";
    const COMMIT_EMAIL: &str = "4242+lexoliu@users.noreply.github.com";

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("flyco-git-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("scratch checkout");
            Self(path)
        }

        /// A path inside the scratch directory that does not exist yet.
        fn child(&self, name: &str) -> std::path::PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A `git daemon` that dies with the test that started it.
    ///
    /// The kill has to survive a panic: an assertion between the spawn and
    /// the end of the test would otherwise leave the daemon running for as
    /// long as the machine is up, holding the port and the scratch
    /// directory it was given.
    ///
    /// Killing the spawned child is not enough. `git` is reached through a
    /// shim that forks, so the process this fixture holds a handle to exits
    /// at once and the process actually listening on the port is reparented
    /// to `init` — which is why the daemon is asked for a `--pid-file` and
    /// that is what gets signalled.
    struct Daemon {
        child: std::process::Child,
        pid_file: std::path::PathBuf,
    }

    impl Drop for Daemon {
        fn drop(&mut self) {
            if let Ok(recorded) = std::fs::read_to_string(&self.pid_file)
                && let Ok(pid) = recorded.trim().parse::<u32>()
            {
                let _ = std::process::Command::new("kill")
                    .arg(pid.to_string())
                    .status();
            }
            let _ = self.child.kill();
            // `kill` only sends the signal; `wait` is what reaps the child
            // — without it the shim stays a zombie until the test binary
            // exits.
            let _ = self.child.wait();
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

    /// A repository to clone from, on disk, with one commit on `branch`.
    ///
    /// A bare clone of a scratch checkout rather than a fixture directory
    /// committed into this repository: nothing here touches the network, and
    /// a bare repository is what a real origin is.
    fn origin(scratch: &Scratch, branch: &str) -> String {
        let source = scratch.child("source");
        std::fs::create_dir_all(&source).expect("source checkout");
        init(&source);
        git(&source, &["branch", "-M", branch]);

        let bare = scratch.child("origin.git");
        git(
            &scratch.0,
            &[
                "clone",
                "--bare",
                source.to_str().expect("a UTF-8 path"),
                bare.to_str().expect("a UTF-8 path"),
            ],
        );
        bare.to_str().expect("a UTF-8 path").to_owned()
    }

    fn repo_config(branch: &str) -> RepoConfig {
        RepoConfig {
            slug: "lexoliu/flyco".parse().expect("a valid slug"),
            branch: branch.parse().expect("a valid branch"),
            dir: "flyco".to_owned(),
        }
    }

    /// The shared `[github]` access the clone tests authenticate with.
    fn access() -> GithubAccess {
        GithubAccess {
            token: TOKEN.to_owned(),
            identity: GitIdentity {
                name: COMMIT_NAME.to_owned(),
                email: COMMIT_EMAIL.to_owned(),
            },
        }
    }

    /// Every regular file under `root`, `.git` included.
    fn files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut found = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(&directory).expect("read a directory") {
                let path = entry.expect("a directory entry").path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    found.push(path);
                }
            }
        }
        found
    }

    #[tokio::test]
    async fn a_clone_brings_the_repositorys_submodules() {
        let scratch = Scratch::new();

        // The submodule's own repository, exported over `git://` by a
        // loopback `git daemon`. The fixture cannot use a `file:` URL or a
        // local path: git denies the file transport for fetches that are
        // not a direct user request, and a recursive clone's submodule
        // fetch is exactly that. `git` transport is allowed by default.
        let sub_source = scratch.child("sub-source");
        std::fs::create_dir_all(&sub_source).expect("submodule source");
        init(&sub_source);
        let sub_bare = scratch.child("sub.git");
        git(
            &scratch.0,
            &[
                "clone",
                "--bare",
                sub_source.to_str().expect("a UTF-8 path"),
                sub_bare.to_str().expect("a UTF-8 path"),
            ],
        );
        // A port that was free a moment ago: fine for a fixture whose whole
        // life is this test.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind an ephemeral port")
            .local_addr()
            .expect("read the bound port")
            .port();
        let pid_file = scratch.child("git-daemon.pid");
        let daemon = Daemon {
            child: std::process::Command::new("git")
                .args([
                    "daemon",
                    "--export-all",
                    "--reuseaddr",
                    "--listen=127.0.0.1",
                    &format!("--port={port}"),
                    &format!("--base-path={}", scratch.0.display()),
                    &format!("--pid-file={}", pid_file.display()),
                ])
                .arg(&scratch.0)
                // Every stream is closed, stdout included. A daemon that
                // outlives this test inherits whatever the test runner's
                // stdout is, and `cargo test | tail` then waits on a pipe
                // nobody will ever close: the run looks hung for as long as
                // the stray daemon lives.
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn git daemon"),
            pid_file,
        };
        // `spawn` returns before the daemon has bound its port — a client
        // that lands first is refused, so the fixture waits for the
        // listener it asked for.
        let mut listening = false;
        for _ in 0..50 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                listening = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(listening, "git daemon never listened on 127.0.0.1:{port}");
        let sub_url = format!("git://127.0.0.1:{port}/sub.git");

        let source = scratch.child("source");
        std::fs::create_dir_all(&source).expect("source checkout");
        init(&source);
        git(&source, &["branch", "-M", "main"]);
        // The fixture-side `submodule add` is a direct request, so its own
        // fetch of the loopback URL is allowed without relaxing anything.
        git(&source, &["submodule", "add", &sub_url, "deps/sub"]);
        git(&source, &["commit", "-m", "record the submodule"]);

        let bare = scratch.child("origin.git");
        git(
            &scratch.0,
            &[
                "clone",
                "--bare",
                source.to_str().expect("a UTF-8 path"),
                bare.to_str().expect("a UTF-8 path"),
            ],
        );

        let workdir = scratch.child("work");
        clone_from(
            bare.to_str().expect("a UTF-8 path"),
            &repo_config("main"),
            &access(),
            &workdir,
        )
        .await
        .expect("the recursive clone succeeds");
        drop(daemon);

        assert!(
            workdir.join("deps/sub/README.md").is_file(),
            "the submodule's content is in the checkout, not just its gitlink"
        );
    }

    #[tokio::test]
    async fn a_clone_lands_the_named_branch_in_the_workdir() {
        let scratch = Scratch::new();
        let remote = origin(&scratch, "dev");
        let workdir = scratch.child("work");

        clone_from(&remote, &repo_config("dev"), &access(), &workdir)
            .await
            .expect("the clone succeeds against a bare repository on disk");

        assert!(
            workdir.join("README.md").is_file(),
            "the checkout holds the repository's content"
        );
        let head = std::process::Command::new("git")
            .current_dir(&workdir)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .expect("read HEAD");
        assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "dev");
    }

    #[tokio::test]
    async fn a_disk_that_already_holds_the_checkout_is_recognised_as_one() {
        // The question a daemon asks on a machine whose compute was
        // reclaimed and given back: the disk came through, so there is
        // nothing to clone and the uncommitted work on it must not be
        // cloned over.
        let scratch = Scratch::new();
        let remote = origin(&scratch, "main");
        let workdir = scratch.child("work");
        assert!(!super::has_checkout(&workdir).await);

        clone_from(&remote, &repo_config("main"), &access(), &workdir)
            .await
            .expect("clone");
        assert!(super::has_checkout(&workdir).await);

        // And cloning over it is not a thing that could quietly work.
        clone_from(&remote, &repo_config("main"), &access(), &workdir)
            .await
            .expect_err("git refuses a destination that is not empty");
    }

    #[tokio::test]
    async fn a_clone_configures_the_identity_its_commits_are_authored_as() {
        let scratch = Scratch::new();
        let remote = origin(&scratch, "main");
        let workdir = scratch.child("work");

        clone_from(&remote, &repo_config("main"), &access(), &workdir)
            .await
            .expect("clone");

        for (key, expected) in [("user.name", COMMIT_NAME), ("user.email", COMMIT_EMAIL)] {
            let value = std::process::Command::new("git")
                .current_dir(&workdir)
                .args(["config", "--local", key])
                .output()
                .expect("read a config value");
            assert_eq!(String::from_utf8_lossy(&value.stdout).trim(), expected);
        }
    }

    #[tokio::test]
    async fn the_github_token_reaches_no_file_the_clone_leaves_behind() {
        // The whole point of the credential helper: the token is handed to
        // one child process through its environment and is readable nowhere
        // afterwards — not in the remote URL, not in .git/config, not in a
        // credential store, not in the packed refs.
        let scratch = Scratch::new();
        let remote = origin(&scratch, "main");
        let workdir = scratch.child("work");

        clone_from(&remote, &repo_config("main"), &access(), &workdir)
            .await
            .expect("clone");

        for path in files(&workdir) {
            let bytes = std::fs::read(&path).expect("read a file the clone wrote");
            assert!(
                !String::from_utf8_lossy(&bytes).contains(TOKEN),
                "{} holds the GitHub token",
                path.display()
            );
        }

        let url = std::process::Command::new("git")
            .current_dir(&workdir)
            .args(["remote", "get-url", "origin"])
            .output()
            .expect("read the remote URL");
        assert!(!String::from_utf8_lossy(&url.stdout).contains(TOKEN));
    }

    #[tokio::test]
    async fn the_github_token_never_survives_a_debug_rendering() {
        let github = access();
        let debugged = format!("{github:?}");
        assert!(!debugged.contains(TOKEN));
        // Still enough to identify whose access it is in a log line.
        assert!(debugged.contains(COMMIT_NAME));
    }

    #[tokio::test]
    async fn a_branch_the_origin_does_not_have_fails_with_gits_own_words() {
        let scratch = Scratch::new();
        let remote = origin(&scratch, "main");
        let workdir = scratch.child("work");

        let error = clone_from(&remote, &repo_config("nope"), &access(), &workdir)
            .await
            .expect_err("a missing branch is a session failure, not an empty checkout");

        let reported = error.to_string();
        assert!(
            reported.contains("nope"),
            "the failure names the branch that is missing: {reported}"
        );
    }

    #[tokio::test]
    async fn a_resumed_machine_clones_and_then_replays_its_stored_patch() {
        // What a resume onto a new machine actually is: a fresh clone at the
        // session's branch, plus the snapshot an automatic archive took of
        // the work that was never committed.
        let scratch = Scratch::new();
        let remote = origin(&scratch, "main");

        let first = scratch.child("first");
        clone_from(&remote, &repo_config("main"), &access(), &first)
            .await
            .expect("the original machine's clone");
        std::fs::write(first.join("draft.txt"), "half a refactor\n").expect("write");
        let snapshot = GitWorkdir::new(first.clone())
            .snapshot()
            .await
            .expect("snapshot")
            .expect("a dirty tree produces a patch");

        let second = scratch.child("second");
        clone_from(&remote, &repo_config("main"), &access(), &second)
            .await
            .expect("the new machine's clone");
        assert!(
            !second.join("draft.txt").exists(),
            "a fresh clone starts from the branch, not from the last machine"
        );

        super::reset_to(&second, &snapshot.base_commit)
            .await
            .expect("the clone rewinds to the patch's base");
        GitWorkdir::new(second.clone())
            .apply(&snapshot.patch)
            .await
            .expect("the stored patch applies onto the fresh clone");
        assert_eq!(
            std::fs::read_to_string(second.join("draft.txt")).expect("read the replayed file"),
            "half a refactor\n"
        );
    }

    #[tokio::test]
    async fn a_commit_the_agent_made_is_work_the_snapshot_carries() {
        // The shape the bug took: an agent that committed its work left a
        // clean tree, and a snapshot diffed against HEAD called that clean —
        // the commit went away with the disk.
        let scratch = Scratch::new();
        let remote = origin(&scratch, "main");

        let first = scratch.child("first");
        clone_from(&remote, &repo_config("main"), &access(), &first)
            .await
            .expect("the original machine's clone");
        let landed = std::process::Command::new("git")
            .current_dir(&first)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("read the landing tip");
        let landed = String::from_utf8_lossy(&landed.stdout).trim().to_owned();

        std::fs::write(first.join("fix.rs"), "fn fixed() {}\n").expect("write");
        git(&first, &["add", "fix.rs"]);
        git(&first, &["commit", "-m", "the fix"]);

        let snapshot = GitWorkdir::new(first.clone())
            .snapshot()
            .await
            .expect("snapshot")
            .expect("a committed tree is not a clean one");
        assert_eq!(
            snapshot.base_commit, landed,
            "the patch is diffed against the clone's landing tip"
        );
        assert!(
            String::from_utf8_lossy(&snapshot.patch).contains("fix.rs"),
            "the committed file is in the patch"
        );

        // Replay is the resume path: clone again, rewind to the base the
        // stored object carries, apply.
        let second = scratch.child("second");
        clone_from(&remote, &repo_config("main"), &access(), &second)
            .await
            .expect("the new machine's clone");
        let stored = snapshot.encode();
        let (base, patch) = super::decode_snapshot(&stored);
        assert_eq!(base, Some(snapshot.base_commit.as_str()));
        super::reset_to(&second, base.expect("a stored snapshot names its base"))
            .await
            .expect("rewind");
        GitWorkdir::new(second.clone())
            .apply(patch)
            .await
            .expect("the stored patch applies onto the fresh clone");
        assert_eq!(
            std::fs::read_to_string(second.join("fix.rs")).expect("read the replayed file"),
            "fn fixed() {}\n"
        );
    }

    #[test]
    fn a_stored_patch_from_before_bases_decodes_as_one() {
        // Patches stored before snapshots carried a base are a bare `git
        // diff`: they replay without a rewind, exactly as they used to.
        let body = b"diff --git a/NOTES.md b/NOTES.md\nindex 1..2 100644\n";
        let (base, patch) = super::decode_snapshot(body);
        assert_eq!(base, None);
        assert_eq!(patch, body);
    }

    #[tokio::test]
    async fn a_clean_tree_snapshots_as_none() {
        let scratch = Scratch::new();
        init(&scratch.0);
        let workdir = GitWorkdir::new(scratch.0.clone());
        assert!(workdir.snapshot().await.expect("snapshot").is_none());
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
        let snapshot = workdir
            .snapshot()
            .await
            .expect("snapshot")
            .expect("dirty trees produce a patch");
        let text = String::from_utf8_lossy(&snapshot.patch);
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
