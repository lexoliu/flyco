//! Reading the session's checkout for the browser.
//!
//! The daemon is the only process that can see the disk a session works on,
//! so the `Files` and `Diff` tabs of docs/ux.md §9.4 are answered from here:
//! the control plane relays a
//! [`WorkdirRequest`](flyco_core::workdir::WorkdirRequest) down the daemon's
//! command stream and this module answers it.
//!
//! # Read-only, and only inside the checkout
//!
//! Nothing here writes to the working tree — that is the agent's, and a
//! browser tab is not a second editor of it. Every path a browser sends is
//! resolved against the checkout root and then *canonicalized*, so a
//! `../../etc/passwd`, an absolute path and a symlink pointing out of the
//! tree are all the same refusal. The repository's own `.git` directory is
//! refused with them: it is machinery rather than the user's work, and
//! nothing in the UI offers it.
//!
//! # The diff stages into an index that is not the checkout's
//!
//! A session's diff has to include files the agent created and has not
//! committed, and `git diff` alone never shows an untracked file. Staging
//! them in the checkout's own index would be this daemon writing into state
//! the agent owns — an `git add -A` landing between the agent's own `add`
//! and its `commit` is exactly the sort of surprise flyco must not be. So
//! the diff points `GIT_INDEX_FILE` at a scratch index in the temporary
//! directory, reads `HEAD` into it, stages everything *there*, and diffs
//! that against the base branch. The checkout's index is untouched, and the
//! scratch file is deleted whether or not the diff succeeded.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use flyco_core::workdir::{
    DIFF_PATCH_BYTES_MAX, DIRECTORY_ENTRIES_MAX, DirectoryEntry, DirectoryListing, EntryKind,
    FILE_BYTES_MAX, FileChange, FileContent, FileDiff, WorkdirDiff, WorkdirRefusal, WorkdirReply,
    WorkdirRequest,
};
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

use crate::git::run_git;

/// The `.git` directory, which no listing shows and no path may enter.
const GIT_DIR: &str = ".git";

/// One session's workspace, as the browser is allowed to see it.
///
/// The workspace root is the harness's working directory and the parent of
/// every checkout: a session's `[[repos]]` each land in `root/<dir>/`, and
/// the guidance files written beside them are listed and read like any
/// other directory. `Entries` and `File` therefore answer against the
/// whole workspace — a checkout's files are just its `dir/` subtree — while
/// `Diff` is per-checkout, because `git diff` is a repository operation and
/// the workspace root is not a repository.
#[derive(Debug, Clone)]
pub struct Workspace {
    /// The directory the session works in.
    root: PathBuf,
    /// The diff base of each checkout beneath the root, by `dir` —
    /// `origin/{branch}` as the clone's `[[repos]]` entry names it.
    repos: BTreeMap<String, String>,
    /// The base a root-level diff is taken against, when the workspace
    /// itself is the checkout — the developer-machine shape, where `repos`
    /// is empty and `root` is the repository. Always `None` on a
    /// provisioned machine, where a `Diff` naming no `repo` is refused.
    base: Option<String>,
}

impl Workspace {
    /// Reads `root`, diffing a root checkout against `base` when the
    /// session has one — the developer-machine shape.
    #[must_use]
    pub const fn new(root: PathBuf, base: Option<String>) -> Self {
        Self {
            root,
            repos: BTreeMap::new(),
            base,
        }
    }

    /// Reads `root` with `[[repos]]` checkouts beneath it — the
    /// provisioned shape.
    ///
    /// `branches` pairs each checkout's `dir` with the branch its clone
    /// started on; the diff base recorded for it is `origin/{branch}`.
    #[must_use]
    pub fn provisioned(
        root: PathBuf,
        branches: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        Self {
            root,
            repos: branches
                .into_iter()
                .map(|(dir, branch)| (dir, format!("origin/{branch}")))
                .collect(),
            base: None,
        }
    }

    /// Registers a checkout that landed mid-session.
    ///
    /// An `AddRepo` the control plane approved names its `dir` and the
    /// branch the clone is on; from then on a `Diff` may name it like any
    /// configured one.
    pub fn register(&mut self, dir: &str, branch: &str) {
        self.repos
            .insert(dir.to_owned(), format!("origin/{branch}"));
    }

    /// Answers one question about the workspace.
    ///
    /// Infallible by construction: every way of failing is a
    /// [`WorkdirRefusal`], because the control plane turns each one into its
    /// own RFC 9457 problem and a browser has to be told which.
    pub async fn inspect(&self, request: WorkdirRequest) -> WorkdirReply {
        match request {
            WorkdirRequest::Entries { path } => match self.entries(&path).await {
                Ok(listing) => WorkdirReply::Entries { listing },
                Err(refusal) => WorkdirReply::refused(refusal),
            },
            WorkdirRequest::File { path } => match self.file(&path).await {
                Ok(content) => WorkdirReply::File { content },
                Err(refusal) => WorkdirReply::refused(refusal),
            },
            WorkdirRequest::Diff { repo } => match self.diff(repo.as_deref()).await {
                Ok(diff) => WorkdirReply::Diff { diff },
                Err(refusal) => WorkdirReply::refused(refusal),
            },
        }
    }

    /// Lists one directory, marking what git ignores.
    async fn entries(&self, path: &str) -> Result<DirectoryListing, WorkdirRefusal> {
        let directory = self.resolve(path).await?;
        let metadata =
            tokio::fs::metadata(&directory)
                .await
                .map_err(|_| WorkdirRefusal::NotFound {
                    path: path.to_owned(),
                })?;
        if !metadata.is_dir() {
            return Err(WorkdirRefusal::NotADirectory {
                path: path.to_owned(),
            });
        }

        let prefix = normalized(path);
        let mut reader = tokio::fs::read_dir(&directory)
            .await
            .map_err(|error| unreadable(&error))?;
        let mut entries = Vec::new();
        while let Some(entry) = reader
            .next_entry()
            .await
            .map_err(|error| unreadable(&error))?
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            // `.git` at any depth is one checkout's machinery — a
            // provisioned workspace holds one per `dir`, and none of them
            // is the user's work.
            if name == GIT_DIR {
                continue;
            }
            // `metadata` rather than the directory entry's own file type, so
            // a symlink is described by what it points at; one that points
            // nowhere is left out rather than offered as a file that cannot
            // be opened.
            let Ok(metadata) = entry.metadata().await else {
                continue;
            };
            let kind = if metadata.is_dir() {
                EntryKind::Directory
            } else {
                EntryKind::File
            };
            entries.push(DirectoryEntry {
                path: if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}/{name}")
                },
                name,
                kind,
                size_bytes: metadata.is_file().then_some(metadata.len()),
                ignored: false,
            });
        }

        // Directories first, then files, each in name order: the shape every
        // file tree has, so the eye can skip to the folders.
        entries.sort_by(|left, right| {
            rank(left.kind)
                .cmp(&rank(right.kind))
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
                .then_with(|| left.name.cmp(&right.name))
        });
        let truncated = entries.len() > DIRECTORY_ENTRIES_MAX;
        entries.truncate(DIRECTORY_ENTRIES_MAX);

        // Ignore rules come from a repository, and which repository is the
        // listed directory's own business: `flyco/` answers out of that
        // checkout's `.gitignore` while the workspace root — not a
        // repository at all on a provisioned machine — marks nothing. The
        // probe runs where the listing is, so a directory inside a checkout
        // is answered by that checkout.
        let ignored = if inside_work_tree(&directory).await? {
            self.ignored(&directory, &entries).await?
        } else {
            std::collections::BTreeSet::new()
        };
        for entry in &mut entries {
            entry.ignored = ignored.contains(&entry.name);
        }

        Ok(DirectoryListing {
            path: prefix,
            entries,
            truncated,
        })
    }

    /// Which of `entries` git's ignore rules exclude.
    ///
    /// Runs inside the listed `directory`, so the rules answered are that
    /// checkout's own: the workspace root of a provisioned session is not a
    /// repository, and a path handed to the wrong clone would borrow the
    /// wrong `.gitignore`. Entry *names* go in and come back — the listing
    /// they describe is the directory's, so a name is the relative path
    /// `check-ignore` expects.
    ///
    /// One `check-ignore` for the whole listing rather than one per row:
    /// the answer is the same and a directory of five hundred files is not
    /// five hundred processes. It exits 1 when nothing matched, which is an
    /// answer rather than a failure.
    async fn ignored(
        &self,
        directory: &Path,
        entries: &[DirectoryEntry],
    ) -> Result<std::collections::BTreeSet<String>, WorkdirRefusal> {
        if entries.is_empty() {
            return Ok(std::collections::BTreeSet::new());
        }

        let mut child = Command::new("git")
            .current_dir(directory)
            .args(["check-ignore", "-z", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|error| unreadable(&error))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| WorkdirRefusal::Unreadable {
                detail: "git check-ignore did not take a path list".to_owned(),
            })?;
        let mut paths = Vec::new();
        for entry in entries {
            paths.extend_from_slice(entry.name.as_bytes());
            paths.push(0);
        }
        stdin
            .write_all(&paths)
            .await
            .map_err(|error| unreadable(&error))?;
        drop(stdin);

        let output = child
            .wait_with_output()
            .await
            .map_err(|error| unreadable(&error))?;
        // 0: something is ignored. 1: nothing is. Anything else is git
        // refusing, and a listing that silently marked nothing would be
        // claiming a `target/` is checked in.
        match output.status.code() {
            Some(0 | 1) => {}
            _ => {
                return Err(WorkdirRefusal::Unreadable {
                    detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                });
            }
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(ToOwned::to_owned)
            .collect())
    }

    /// Reads one text file, within [`FILE_BYTES_MAX`].
    async fn file(&self, path: &str) -> Result<FileContent, WorkdirRefusal> {
        let file = self.resolve(path).await?;
        let metadata = tokio::fs::metadata(&file)
            .await
            .map_err(|_| WorkdirRefusal::NotFound {
                path: path.to_owned(),
            })?;
        if !metadata.is_file() {
            return Err(WorkdirRefusal::NotAFile {
                path: path.to_owned(),
            });
        }
        let bytes = metadata.len();
        if bytes > FILE_BYTES_MAX {
            return Err(WorkdirRefusal::TooLarge {
                path: path.to_owned(),
                bytes,
            });
        }

        let content = tokio::fs::read(&file)
            .await
            .map_err(|error| unreadable(&error))?;
        // UTF-8 *is* the definition of text here: anything else has no
        // rendering a code view could give it, and half a PNG in a <pre> is
        // worse than being told it is a binary file.
        let text = String::from_utf8(content).map_err(|_| WorkdirRefusal::NotText {
            path: path.to_owned(),
        })?;
        Ok(FileContent {
            path: normalized(path),
            text,
            bytes,
        })
    }

    /// Diffs one checkout's working tree against the branch it began on.
    ///
    /// `repo` names the checkout as `SessionRepo::dir` does; `None` asks
    /// for the workspace root itself, which only the developer-machine
    /// shape has — a provisioned session's root is a directory of
    /// checkouts, and diffing it would compare guidance files against no
    /// branch at all.
    async fn diff(&self, repo: Option<&str>) -> Result<WorkdirDiff, WorkdirRefusal> {
        let (root, base) = match repo {
            Some(dir) => {
                let base = self
                    .repos
                    .get(dir)
                    .ok_or_else(|| WorkdirRefusal::UnknownCheckout {
                        repo: dir.to_owned(),
                    })?;
                (self.root.join(dir), base.clone())
            }
            None => (
                self.root.clone(),
                self.base.clone().ok_or(WorkdirRefusal::NoBaseBranch)?,
            ),
        };
        let resolved = run_git(
            &root,
            &[
                OsStr::new("rev-parse"),
                OsStr::new("--verify"),
                OsStr::new("--quiet"),
                OsStr::new(&format!("{base}^{{commit}}")),
            ],
            &[],
            &[1],
        )
        .await
        .map_err(|error| unreadable(&error))?;
        if !resolved.status.success() {
            return Err(WorkdirRefusal::NoBaseBranch);
        }

        let index = std::env::temp_dir().join(format!("flyco-diff-{}.index", uuid::Uuid::new_v4()));
        let staged = self.stage_into(&root, &index, &base).await;
        // The scratch index is deleted whichever way the diff went: it is a
        // few kilobytes per request on a machine that runs for days.
        if let Err(error) = tokio::fs::remove_file(&index).await {
            tracing::debug!(%error, index = %index.display(), "a scratch diff index was not removed");
        }
        let (numstat, patch) = staged?;

        let files = files_of(&numstat, &patch)?;
        let added_lines = files.iter().map(|file| file.added_lines).sum();
        let removed_lines = files.iter().map(|file| file.removed_lines).sum();
        let truncated = files
            .iter()
            .any(|file| file.patch.is_none() && !file.binary);
        Ok(WorkdirDiff {
            base,
            files,
            added_lines,
            removed_lines,
            truncated,
        })
    }

    /// Stages the whole working tree of `root` into `index` and diffs it
    /// against `base`, answering with git's numstat and its patch.
    async fn stage_into(
        &self,
        root: &Path,
        index: &Path,
        base: &str,
    ) -> Result<(String, String), WorkdirRefusal> {
        let env = [("GIT_INDEX_FILE", index.as_os_str())];
        for args in [
            [OsStr::new("read-tree"), OsStr::new("HEAD")].as_slice(),
            [OsStr::new("add"), OsStr::new("-A")].as_slice(),
        ] {
            run_git(root, args, &env, &[])
                .await
                .map_err(|error| unreadable(&error))?;
        }

        let numstat = run_git(
            root,
            &[
                OsStr::new("diff"),
                OsStr::new("--cached"),
                OsStr::new("--numstat"),
                OsStr::new("-z"),
                OsStr::new(base),
                OsStr::new("--"),
            ],
            &env,
            &[],
        )
        .await
        .map_err(|error| unreadable(&error))?;
        let patch = run_git(
            root,
            &[
                OsStr::new("diff"),
                OsStr::new("--cached"),
                OsStr::new(base),
                OsStr::new("--"),
            ],
            &env,
            &[],
        )
        .await
        .map_err(|error| unreadable(&error))?;

        Ok((
            String::from_utf8_lossy(&numstat.stdout).into_owned(),
            String::from_utf8_lossy(&patch.stdout).into_owned(),
        ))
    }

    /// Resolves a browser-supplied path inside the workspace.
    ///
    /// Two checks, and both are load-bearing. The textual one refuses `..`,
    /// an absolute path and `.git` — at any depth, since each checkout
    /// carries its own — before anything touches the disk; the canonical
    /// one refuses a symlink that leaves the tree, which no amount of
    /// string inspection can see.
    async fn resolve(&self, path: &str) -> Result<PathBuf, WorkdirRefusal> {
        let outside = || WorkdirRefusal::OutsideCheckout {
            path: path.to_owned(),
        };
        if path.starts_with('/') || path.contains('\0') {
            return Err(outside());
        }

        let mut resolved = self.root.clone();
        for segment in path.split('/').filter(|part| !part.is_empty()) {
            if segment == "." || segment == ".." || segment == GIT_DIR {
                return Err(outside());
            }
            resolved.push(segment);
        }

        let root = tokio::fs::canonicalize(&self.root)
            .await
            .map_err(|error| unreadable(&error))?;
        let canonical =
            tokio::fs::canonicalize(&resolved)
                .await
                .map_err(|_| WorkdirRefusal::NotFound {
                    path: path.to_owned(),
                })?;
        if !canonical.starts_with(&root) {
            return Err(outside());
        }
        Ok(canonical)
    }
}

/// Whether `path` sits inside a git working tree.
///
/// The probe that tells a provisioned workspace's root (not a repository)
/// from a directory inside one of its checkouts: `rev-parse` answers from
/// wherever it is pointed, so the same question serves both. A refusal —
/// exit 128, git missing — is an [`WorkdirRefusal::Unreadable`] rather
/// than a quiet "no", because a listing that marks nothing ignored inside
/// a repository is claiming a `target/` is checked in.
async fn inside_work_tree(path: &Path) -> Result<bool, WorkdirRefusal> {
    let output = run_git(
        path,
        &[OsStr::new("rev-parse"), OsStr::new("--is-inside-work-tree")],
        &[],
        &[128],
    )
    .await
    .map_err(|error| unreadable(&error))?;
    Ok(output.status.success() && output.stdout.starts_with(b"true"))
}

/// Where a kind of entry sorts: directories above files.
const fn rank(kind: EntryKind) -> u8 {
    match kind {
        EntryKind::Directory => 0,
        EntryKind::File => 1,
    }
}

/// A path as the listing reports it: no leading, trailing or doubled
/// separators, so the string a browser sends back addresses the same file.
fn normalized(path: &str) -> String {
    path.split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

fn unreadable(error: &impl core::fmt::Display) -> WorkdirRefusal {
    WorkdirRefusal::Unreadable {
        detail: error.to_string(),
    }
}

/// One row of `git diff --numstat -z`.
#[derive(Debug, PartialEq, Eq)]
struct Numstat {
    added: u32,
    removed: u32,
    binary: bool,
    path: String,
    previous_path: Option<String>,
}

/// Reads `git diff --numstat -z` output.
///
/// The `-z` form is the only one that is unambiguous: a path with a space,
/// a quote or a newline in it is written verbatim between NUL bytes rather
/// than C-quoted, so nothing here has to unescape anything. A record is
/// `added\tremoved\tpath`; a rename writes `added\tremoved\t` and puts the
/// old and the new path in the two fields after it.
fn numstats(output: &str) -> Result<Vec<Numstat>, WorkdirRefusal> {
    let mut fields = output.split('\0').filter(|field| !field.is_empty());
    let mut rows = Vec::new();
    while let Some(field) = fields.next() {
        let mut parts = field.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(WorkdirRefusal::Unreadable {
                detail: format!("git numstat wrote a record this daemon cannot read: `{field}`"),
            });
        };
        // A binary file has no line counts, and git says so with a dash
        // rather than a zero — the difference between "nothing changed" and
        // "lines are not the unit here".
        let binary = added == "-" || removed == "-";
        let count = |value: &str| -> u32 { value.parse().unwrap_or(0) };
        let (path, previous_path) = if path.is_empty() {
            let (Some(previous), Some(current)) = (fields.next(), fields.next()) else {
                return Err(WorkdirRefusal::Unreadable {
                    detail: "git numstat wrote a rename with no paths after it".to_owned(),
                });
            };
            (current.to_owned(), Some(previous.to_owned()))
        } else {
            (path.to_owned(), None)
        };
        rows.push(Numstat {
            added: count(added),
            removed: count(removed),
            binary,
            path,
            previous_path,
        });
    }
    Ok(rows)
}

/// Splits a whole `git diff` into one block per file.
///
/// Blocks are matched to [`numstats`] rows by position, which is exact:
/// both come from one `git diff` invocation over one index, and git writes
/// the files in the same order for every output format.
fn blocks(patch: &str) -> Vec<String> {
    let mut blocks: Vec<String> = Vec::new();
    for line in patch.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            blocks.push(String::new());
        }
        if let Some(block) = blocks.last_mut() {
            block.push_str(line);
        }
    }
    blocks
}

/// Pairs git's counts with git's patch text, within the size budget.
fn files_of(numstat: &str, patch: &str) -> Result<Vec<FileDiff>, WorkdirRefusal> {
    let rows = numstats(numstat)?;
    let blocks = blocks(patch);
    if rows.len() != blocks.len() {
        return Err(WorkdirRefusal::Unreadable {
            detail: format!(
                "git described {} changed files and produced {} patches",
                rows.len(),
                blocks.len()
            ),
        });
    }

    let mut spent = 0_usize;
    let mut files = Vec::with_capacity(rows.len());
    for (row, block) in rows.into_iter().zip(blocks) {
        let change = change_of(&block, row.previous_path.is_some());
        // A binary file's patch is base85 or a one-line "differ", neither of
        // which a diff view can render — the counts and the badge are the
        // whole of what there is to say about it.
        let patch = if row.binary || spent.saturating_add(block.len()) > DIFF_PATCH_BYTES_MAX {
            None
        } else {
            spent = spent.saturating_add(block.len());
            Some(block)
        };
        files.push(FileDiff {
            path: row.path,
            previous_path: row.previous_path,
            change,
            added_lines: row.added,
            removed_lines: row.removed,
            binary: row.binary,
            patch,
        });
    }
    Ok(files)
}

/// What a file's own patch header says happened to it.
fn change_of(block: &str, renamed: bool) -> FileChange {
    if renamed {
        return FileChange::Renamed;
    }
    for line in block.lines() {
        if line.starts_with("new file mode ") {
            return FileChange::Added;
        }
        if line.starts_with("deleted file mode ") {
            return FileChange::Deleted;
        }
        if line.starts_with("@@") {
            break;
        }
    }
    FileChange::Modified
}

#[cfg(test)]
mod tests {
    use flyco_core::workdir::{
        EntryKind, FileChange, WorkdirRefusal, WorkdirReply, WorkdirRequest,
    };

    use super::Workspace;

    /// A scratch checkout with one commit on `main`, and an `origin/main`
    /// to diff against — the shape a session VM has after `git clone`.
    struct Scratch {
        path: std::path::PathBuf,
    }

    impl Scratch {
        /// A standalone checkout — the developer-machine shape, where the
        /// workspace root is the repository.
        fn new() -> Self {
            Self::at(std::env::temp_dir().join(format!("flyco-workdir-{}", uuid::Uuid::new_v4())))
        }

        /// A checkout at `path` — for the provisioned shape, where the
        /// workspace holds each repository in a named directory.
        fn at(path: std::path::PathBuf) -> Self {
            std::fs::create_dir_all(&path).expect("scratch checkout");
            let scratch = Self { path };
            scratch.git(&["init", "--initial-branch=main"]);
            scratch.git(&["config", "user.email", "me@lexo.cool"]);
            scratch.git(&["config", "user.name", "Lexo Liu"]);
            scratch.write("README.md", "# flyco\n");
            scratch.write("src/lib.rs", "fn main() {}\n");
            scratch.write(".gitignore", "target/\n");
            scratch.git(&["add", "-A"]);
            scratch.git(&["commit", "-m", "init"]);
            // The remote-tracking ref a clone leaves behind, made here
            // without a remote: what the session's diff is taken against.
            scratch.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
            scratch
        }

        fn git(&self, args: &[&str]) {
            let output = std::process::Command::new("git")
                .current_dir(&self.path)
                .args(args)
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        fn write(&self, path: &str, content: &str) {
            let file = self.path.join(path);
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent).expect("parent directory");
            }
            std::fs::write(file, content).expect("write");
        }

        fn checkout(&self) -> Workspace {
            Workspace::new(self.path.clone(), Some("origin/main".to_owned()))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// A provisioned-shaped workspace: a plain directory holding one
    /// `Scratch` checkout per `dir`, beside the guidance files a session
    /// VM's workdir root carries.
    struct MultiScratch {
        path: std::path::PathBuf,
        dirs: Vec<String>,
        /// Held so a checkout's `Drop` does not delete it mid-test; the
        /// field order removes them with the workspace.
        _checkouts: Vec<Scratch>,
    }

    impl MultiScratch {
        fn new(dirs: &[&str]) -> Self {
            let path =
                std::env::temp_dir().join(format!("flyco-workspace-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("scratch workspace");
            let checkouts = dirs.iter().map(|dir| Scratch::at(path.join(dir))).collect();
            Self {
                path,
                dirs: dirs.iter().map(|dir| (*dir).to_owned()).collect(),
                _checkouts: checkouts,
            }
        }

        /// Writes `content` at `dir/path`, inside that checkout.
        fn write(&self, dir: &str, path: &str, content: &str) {
            let file = self.path.join(dir).join(path);
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent).expect("parent directory");
            }
            std::fs::write(file, content).expect("write");
        }

        fn workspace(&self) -> Workspace {
            Workspace::provisioned(
                self.path.clone(),
                self.dirs.iter().map(|dir| (dir.clone(), "main".to_owned())),
            )
        }
    }

    impl Drop for MultiScratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn listing(reply: WorkdirReply) -> flyco_core::workdir::DirectoryListing {
        match reply {
            WorkdirReply::Entries { listing } => listing,
            other => panic!("expected a listing, got {other:?}"),
        }
    }

    fn refusal(reply: WorkdirReply) -> WorkdirRefusal {
        match reply {
            WorkdirReply::Refused { refusal } => refusal,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_root_lists_directories_first_and_hides_the_git_directory() {
        let scratch = Scratch::new();
        let listing = listing(
            scratch
                .checkout()
                .inspect(WorkdirRequest::Entries {
                    path: String::new(),
                })
                .await,
        );

        let names: Vec<&str> = listing
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["src", ".gitignore", "README.md"],
            "directories come first, and the repository's own `.git` is not the user's work"
        );
        assert_eq!(listing.path, "");
        assert!(!listing.truncated);
        let source = listing.entries.first().expect("src");
        assert_eq!(source.kind, EntryKind::Directory);
        assert_eq!(source.path, "src");
        assert_eq!(source.size_bytes, None);
    }

    #[tokio::test]
    async fn an_ignored_file_is_listed_and_marked() {
        let scratch = Scratch::new();
        scratch.write("target/debug.log", "noise\n");
        let listing = listing(
            scratch
                .checkout()
                .inspect(WorkdirRequest::Entries {
                    path: String::new(),
                })
                .await,
        );

        let target = listing
            .entries
            .iter()
            .find(|entry| entry.name == "target")
            .expect("the ignored directory is listed rather than hidden");
        assert_eq!(target.kind, EntryKind::Directory);
        assert!(target.ignored, "`target/` is in .gitignore");
        let readme = listing
            .entries
            .iter()
            .find(|entry| entry.name == "README.md")
            .expect("README.md");
        assert!(!readme.ignored);
        assert_eq!(readme.size_bytes, Some(8));
    }

    #[tokio::test]
    async fn a_path_that_leaves_the_checkout_is_refused_before_the_disk_is_touched() {
        let scratch = Scratch::new();
        let checkout = scratch.checkout();
        for path in ["../secrets", "/etc/passwd", ".git/config", "src/../../out"] {
            assert!(
                matches!(
                    refusal(
                        checkout
                            .inspect(WorkdirRequest::File {
                                path: path.to_owned()
                            })
                            .await
                    ),
                    WorkdirRefusal::OutsideCheckout { .. }
                ),
                "`{path}` must not be readable"
            );
        }
    }

    #[tokio::test]
    async fn a_text_file_is_served_and_a_binary_one_is_refused() {
        let scratch = Scratch::new();
        let checkout = scratch.checkout();
        let WorkdirReply::File { content } = checkout
            .inspect(WorkdirRequest::File {
                path: "src/lib.rs".to_owned(),
            })
            .await
        else {
            panic!("src/lib.rs is text");
        };
        assert_eq!(content.text, "fn main() {}\n");
        assert_eq!(content.path, "src/lib.rs");
        assert_eq!(content.bytes, 13);

        std::fs::write(scratch.path.join("logo.png"), [0x89, b'P', 0x00, 0xff])
            .expect("write a binary file");
        assert!(matches!(
            refusal(
                checkout
                    .inspect(WorkdirRequest::File {
                        path: "logo.png".to_owned()
                    })
                    .await
            ),
            WorkdirRefusal::NotText { .. }
        ));
    }

    #[tokio::test]
    async fn a_file_past_the_limit_is_refused_with_its_size() {
        let scratch = Scratch::new();
        let big =
            "x".repeat(usize::try_from(flyco_core::workdir::FILE_BYTES_MAX).expect("fits") + 1);
        scratch.write("big.txt", &big);
        let refused = refusal(
            scratch
                .checkout()
                .inspect(WorkdirRequest::File {
                    path: "big.txt".to_owned(),
                })
                .await,
        );
        let WorkdirRefusal::TooLarge { bytes, .. } = refused else {
            panic!("a file past the limit is refused as too large, not as {refused:?}");
        };
        assert_eq!(bytes, flyco_core::workdir::FILE_BYTES_MAX + 1);
    }

    #[tokio::test]
    async fn missing_paths_and_the_wrong_kind_of_path_are_told_apart() {
        let scratch = Scratch::new();
        let checkout = scratch.checkout();
        assert!(matches!(
            refusal(
                checkout
                    .inspect(WorkdirRequest::File {
                        path: "nope.rs".to_owned()
                    })
                    .await
            ),
            WorkdirRefusal::NotFound { .. }
        ));
        assert!(matches!(
            refusal(
                checkout
                    .inspect(WorkdirRequest::File {
                        path: "src".to_owned()
                    })
                    .await
            ),
            WorkdirRefusal::NotAFile { .. }
        ));
        assert!(matches!(
            refusal(
                checkout
                    .inspect(WorkdirRequest::Entries {
                        path: "README.md".to_owned()
                    })
                    .await
            ),
            WorkdirRefusal::NotADirectory { .. }
        ));
    }

    #[tokio::test]
    async fn the_diff_covers_committed_uncommitted_and_untracked_work() {
        let scratch = Scratch::new();
        // Committed on top of the base.
        scratch.write("src/lib.rs", "fn main() {\n    run();\n}\n");
        scratch.git(&["add", "-A"]);
        scratch.git(&["commit", "-m", "call run"]);
        // Uncommitted.
        scratch.write("README.md", "# flyco\n\nthe agent was here\n");
        // Untracked, which `git diff` alone never shows.
        scratch.write("notes.md", "scratch\n");
        // Ignored, which must stay out of the diff entirely.
        scratch.write("target/debug.log", "noise\n");

        let WorkdirReply::Diff { diff } = scratch
            .checkout()
            .inspect(WorkdirRequest::Diff { repo: None })
            .await
        else {
            panic!("the checkout has a base branch");
        };
        assert_eq!(diff.base, "origin/main");
        let paths: Vec<&str> = diff.files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(paths, ["README.md", "notes.md", "src/lib.rs"]);
        assert!(
            !paths.iter().any(|path| path.starts_with("target/")),
            "an ignored file is not part of the session's work"
        );

        let notes = &diff.files[1];
        assert_eq!(notes.change, FileChange::Added);
        assert_eq!(notes.added_lines, 1);
        assert_eq!(notes.removed_lines, 0);
        assert!(
            notes
                .patch
                .as_ref()
                .expect("an untracked file's patch")
                .contains("+scratch")
        );
        assert_eq!(diff.added_lines, 6);
        assert_eq!(diff.removed_lines, 1);
        assert!(!diff.truncated);

        // The scratch index is the one that was staged into; the checkout's
        // own index still holds nothing.
        let staged = std::process::Command::new("git")
            .current_dir(&scratch.path)
            .args(["diff", "--cached", "--name-only"])
            .output()
            .expect("run git");
        assert!(
            staged.stdout.is_empty(),
            "reading the diff must not stage anything in the agent's index"
        );
    }

    #[tokio::test]
    async fn a_rename_keeps_both_names() {
        let scratch = Scratch::new();
        scratch.git(&["mv", "README.md", "READ.md"]);
        scratch.git(&["commit", "-m", "rename"]);

        let WorkdirReply::Diff { diff } = scratch
            .checkout()
            .inspect(WorkdirRequest::Diff { repo: None })
            .await
        else {
            panic!("the checkout has a base branch");
        };
        let renamed = diff.files.first().expect("one changed file");
        assert_eq!(renamed.change, FileChange::Renamed);
        assert_eq!(renamed.path, "READ.md");
        assert_eq!(renamed.previous_path.as_deref(), Some("README.md"));
    }

    #[tokio::test]
    async fn a_session_with_no_base_branch_refuses_a_diff() {
        let scratch = Scratch::new();
        let checkout = Workspace::new(scratch.path.clone(), None);
        assert_eq!(
            refusal(checkout.inspect(WorkdirRequest::Diff { repo: None }).await),
            WorkdirRefusal::NoBaseBranch
        );

        let unknown = Workspace::new(scratch.path.clone(), Some("origin/nope".to_owned()));
        assert_eq!(
            refusal(unknown.inspect(WorkdirRequest::Diff { repo: None }).await),
            WorkdirRefusal::NoBaseBranch
        );
    }

    #[tokio::test]
    async fn a_workspace_lists_its_checkouts_and_routes_each_diff() {
        // The provisioned shape: `flyco/` and `other/` are checkouts of two
        // repositories, `AGENTS.md` is workspace machinery, and each
        // checkout's diff is its own.
        let multi = MultiScratch::new(&["flyco", "other"]);
        multi.write("flyco", "notes.md", "first repo's work\n");
        multi.write("other", "notes.md", "second repo's work\n");
        std::fs::write(multi.path.join("AGENTS.md"), "checkouts live in dirs\n")
            .expect("write workspace guidance");

        let workspace = multi.workspace();
        let listing = listing(
            workspace
                .inspect(WorkdirRequest::Entries {
                    path: String::new(),
                })
                .await,
        );
        let names: Vec<&str> = listing
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["flyco", "other", "AGENTS.md"],
            "the workspace root lists the checkout directories and its own files, no `.git`"
        );
        // None of them is inside a repository, so nothing is marked ignored
        // and asking git about them is not attempted.
        assert!(listing.entries.iter().all(|entry| !entry.ignored));

        for (dir, expected) in [("flyco", "first"), ("other", "second")] {
            let WorkdirReply::Diff { diff } = workspace
                .inspect(WorkdirRequest::Diff {
                    repo: Some(dir.to_owned()),
                })
                .await
            else {
                panic!("{dir} has a base branch");
            };
            let note = diff
                .files
                .iter()
                .find(|file| file.path == "notes.md")
                .expect("the untracked note is in the diff");
            assert!(
                note.patch
                    .as_ref()
                    .expect("a patch")
                    .contains(&format!("+{expected}")),
                "{dir}'s diff is {dir}'s own work"
            );
        }

        // A `repo` the workspace does not hold is refused rather than
        // answered with some other checkout's diff.
        assert_eq!(
            refusal(
                workspace
                    .inspect(WorkdirRequest::Diff {
                        repo: Some("ghost".to_owned())
                    })
                    .await
            ),
            WorkdirRefusal::UnknownCheckout {
                repo: "ghost".to_owned()
            }
        );
        // And a provisioned workspace names no root checkout at all.
        assert_eq!(
            refusal(workspace.inspect(WorkdirRequest::Diff { repo: None }).await),
            WorkdirRefusal::NoBaseBranch
        );
    }

    #[tokio::test]
    async fn a_checkouts_own_ignore_rules_and_git_directory_stay_inside_it() {
        let multi = MultiScratch::new(&["flyco"]);
        multi.write("flyco", "target/debug.log", "noise\n");

        let workspace = multi.workspace();
        // Inside the checkout, ignore rules are that checkout's.
        let inside = listing(
            workspace
                .inspect(WorkdirRequest::Entries {
                    path: "flyco".to_owned(),
                })
                .await,
        );
        let target = inside
            .entries
            .iter()
            .find(|entry| entry.name == "target")
            .expect("target is listed");
        assert!(target.ignored, "flyco's .gitignore applies inside flyco/");
        assert!(
            inside.entries.iter().all(|entry| entry.name != ".git"),
            "the checkout's machinery is not listed"
        );

        // And a path into a checkout's .git is refused like the root's was.
        assert!(matches!(
            refusal(
                workspace
                    .inspect(WorkdirRequest::File {
                        path: "flyco/.git/config".to_owned()
                    })
                    .await
            ),
            WorkdirRefusal::OutsideCheckout { .. }
        ));
    }
}
