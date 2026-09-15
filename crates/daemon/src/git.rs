//! The session checkout: putting it on the machine, and watching it.
//!
//! [`clone_into`] is what makes a session VM more than an empty directory —
//! the agent's whole job is the repository, so the checkout has to exist
//! before the harness is started in it.
//!
//! After that, dirtiness is load-bearing: an agent may not stop while the
//! tree is dirty (unless the compute budget is exhausted), a manual archive
//! of a dirty tree requires confirmation and discards the work, and an
//! automatic archive snapshots the uncommitted changes before the disk is
//! released.
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

use crate::config::RepoConfig;

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
pub async fn has_checkout(workdir: &Path) -> bool {
    tokio::fs::metadata(workdir.join(".git")).await.is_ok()
}

/// Clones a session's repository into `workdir`.
///
/// The branch is checked out by name rather than fetched and switched to:
/// a session names one branch for its whole life, and a clone that pulled
/// every branch would spend a session VM's first minute on history nothing
/// is going to read.
///
/// The commit identity is written into the checkout's own `.git/config`
/// rather than a global one, so it applies to this repository and says who
/// the session is on behalf of.
///
/// # Errors
///
/// Returns [`GitError`] if git could not be started or refused — an
/// unreachable remote, a token the repository does not admit, a branch that
/// does not exist, or a workdir that already holds something. Every one of
/// those is a session failure rather than something to work around, and the
/// error carries git's own words so the user is told which.
pub async fn clone_into(repo: &RepoConfig, workdir: &Path) -> Result<(), GitError> {
    let remote = repo.remote_url();
    tracing::info!(
        slug = %repo.slug,
        branch = %repo.branch,
        workdir = %workdir.display(),
        "cloning the session's repository"
    );
    clone_from(&remote, repo, workdir).await
}

/// Clones `remote` — the URL split out from [`clone_into`] so a test can
/// point it at a bare repository on disk instead of at `github.com`.
///
/// # Errors
///
/// Returns [`GitError`] exactly as [`clone_into`] does.
pub async fn clone_from(remote: &str, repo: &RepoConfig, workdir: &Path) -> Result<(), GitError> {
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
        repo,
        &[
            OsStr::new("clone"),
            OsStr::new("--recurse-submodules"),
            OsStr::new(&branch),
            OsStr::new("--"),
            OsStr::new(remote),
            workdir.as_os_str(),
        ],
        Path::new("."),
    )
    .await?;

    git(
        workdir,
        &["config", "user.name", repo.identity.name.as_str()],
    )
    .await?;
    git(
        workdir,
        &["config", "user.email", repo.identity.email.as_str()],
    )
    .await?;
    Ok(())
}

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
    repo: &RepoConfig,
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
        .env(TOKEN_VAR, &repo.token)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await
        .map_err(GitError::Spawn)?;
    finished(command, output)
}

/// Rewinds `workdir`'s checked-out branch to `commit`.
///
/// A handoff's patch was diffed against the local merge-base, not against
/// the tip the clone landed on, so the branch is wound back before the
/// patch applies. On a fresh clone the tree is clean, and `reset --hard`
/// only moves the ref.
///
/// # Errors
///
/// Returns [`GitError`] if `commit` is not in the clone — a base a
/// force-push removed between send and provision is a session failure,
/// not something to patch around.
pub async fn reset_to(workdir: &Path, commit: &str) -> Result<(), GitError> {
    git(workdir, &["reset", "--hard", commit]).await.map(|_| ())
}

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
    use super::{GitWorkdir, WorkingTree, clone_from};
    use crate::config::{GitIdentity, RepoConfig};
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
        let mut daemon = std::process::Command::new("git")
            .args([
                "daemon",
                "--export-all",
                "--reuseaddr",
                "--listen=127.0.0.1",
                &format!("--port={port}"),
                &format!("--base-path={}", scratch.0.display()),
            ])
            .arg(&scratch.0)
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn git daemon");
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
        let outcome = clone_from(
            bare.to_str().expect("a UTF-8 path"),
            &repo_config("main"),
            &workdir,
        )
        .await;
        let _ = daemon.kill();
        // `kill` only sends the signal; `wait` is what reaps the child —
        // without it the daemon stays a zombie until the test exits.
        let _ = daemon.wait();
        outcome.expect("the recursive clone succeeds");

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

        clone_from(&remote, &repo_config("dev"), &workdir)
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

        clone_from(&remote, &repo_config("main"), &workdir)
            .await
            .expect("clone");
        assert!(super::has_checkout(&workdir).await);

        // And cloning over it is not a thing that could quietly work.
        clone_from(&remote, &repo_config("main"), &workdir)
            .await
            .expect_err("git refuses a destination that is not empty");
    }

    #[tokio::test]
    async fn a_clone_configures_the_identity_its_commits_are_authored_as() {
        let scratch = Scratch::new();
        let remote = origin(&scratch, "main");
        let workdir = scratch.child("work");

        clone_from(&remote, &repo_config("main"), &workdir)
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

        clone_from(&remote, &repo_config("main"), &workdir)
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
        let config = repo_config("main");
        let debugged = format!("{config:?}");
        assert!(!debugged.contains(TOKEN));
        // Still enough to identify the checkout in a log line.
        assert!(debugged.contains("lexoliu/flyco"));
        assert!(debugged.contains("main"));
    }

    #[tokio::test]
    async fn a_branch_the_origin_does_not_have_fails_with_gits_own_words() {
        let scratch = Scratch::new();
        let remote = origin(&scratch, "main");
        let workdir = scratch.child("work");

        let error = clone_from(&remote, &repo_config("nope"), &workdir)
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
        clone_from(&remote, &repo_config("main"), &first)
            .await
            .expect("the original machine's clone");
        std::fs::write(first.join("draft.txt"), "half a refactor\n").expect("write");
        let patch = GitWorkdir::new(first.clone())
            .snapshot()
            .await
            .expect("snapshot")
            .expect("a dirty tree produces a patch");

        let second = scratch.child("second");
        clone_from(&remote, &repo_config("main"), &second)
            .await
            .expect("the new machine's clone");
        assert!(
            !second.join("draft.txt").exists(),
            "a fresh clone starts from the branch, not from the last machine"
        );

        GitWorkdir::new(second.clone())
            .apply(&patch)
            .await
            .expect("the stored patch applies onto the fresh clone");
        assert_eq!(
            std::fs::read_to_string(second.join("draft.txt")).expect("read the replayed file"),
            "half a refactor\n"
        );
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
