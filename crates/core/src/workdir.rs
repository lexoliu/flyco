//! Reading a session's checkout: the `Files` and `Diff` tabs of
//! docs/ux.md §9.4.
//!
//! Three questions a browser asks about a running session's disk — what is
//! in this directory, what does this file say, and what has the agent
//! changed — and one shape for each answer. The daemon is the only process
//! that can see the checkout, so every one of them is a
//! [`WorkdirRequest`](crate::wire::ControlToDaemon::InspectWorkdir) relayed
//! to it and a [`WorkdirReply`] relayed back; the control plane proxies,
//! authorizes and never stores.
//!
//! # Everything here is bounded
//!
//! A relay frame caps at 1 MiB on Cloudflare, and a browser tab is not a
//! file manager. So a listing carries at most
//! [`DIRECTORY_ENTRIES_MAX`] entries, a file is served only up to
//! [`FILE_BYTES_MAX`] and only if it is text, and a diff stops attaching
//! patch text at [`DIFF_PATCH_BYTES_MAX`] while still reporting every
//! changed file's line counts. Passing a limit is a *typed refusal* rather
//! than a truncation nobody is told about: the UI says which limit was hit
//! and offers the terminal instead.

use serde::{Deserialize, Serialize};

/// Largest file the content route will serve, in bytes.
///
/// A source file the user wants to read is kilobytes; anything past this is
/// a build artifact, a lockfile dump or a data set, none of which a
/// read-only pane in a drawer is the right way to look at.
pub const FILE_BYTES_MAX: u64 = 128 * 1024;

/// How much patch text one diff may carry, in bytes.
///
/// Counted across the whole diff rather than per file: what has to fit is
/// the relay frame, and one enormous file fills it just as well as three
/// hundred small ones.
pub const DIFF_PATCH_BYTES_MAX: usize = 256 * 1024;

/// Most entries one directory listing returns.
pub const DIRECTORY_ENTRIES_MAX: usize = 1_000;

/// What a directory entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// A regular file, or a symlink to one.
    File,
    /// A directory the tree can expand.
    Directory,
}

/// One row of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DirectoryEntry {
    /// The entry's own name, without any directory part.
    pub name: String,
    /// Where it is, relative to the checkout root, `/`-separated.
    ///
    /// What a `path=` query passes back to expand a directory or open a
    /// file, so the browser never assembles a path itself.
    pub path: String,
    /// Whether it is a file or a directory.
    pub kind: EntryKind,
    /// Size in bytes, for files.
    pub size_bytes: Option<u64>,
    /// Whether git ignores it.
    ///
    /// Marked rather than hidden: a `target/` or a `.env` is exactly what a
    /// user goes looking for when something is wrong, and a tree that
    /// silently omitted them would be lying about the disk.
    pub ignored: bool,
}

/// One directory of a session's checkout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DirectoryListing {
    /// The directory listed, relative to the checkout root. Empty is the
    /// root itself.
    pub path: String,
    /// Directories first, then files, each in name order.
    pub entries: Vec<DirectoryEntry>,
    /// Whether entries past [`DIRECTORY_ENTRIES_MAX`] were left out.
    pub truncated: bool,
}

/// One text file of a session's checkout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct FileContent {
    /// The file read, relative to the checkout root.
    pub path: String,
    /// Its whole content. Never a prefix: a file too big to serve is
    /// [refused](WorkdirRefusal::TooLarge) rather than cut in half.
    pub text: String,
    /// Its size on disk, in bytes.
    pub bytes: u64,
}

/// What happened to one file between the base branch and the working tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileChange {
    /// The file did not exist on the base branch.
    Added,
    /// The file exists on both sides with different content.
    Modified,
    /// The file existed on the base branch and does not now.
    Deleted,
    /// The file moved, with or without an edit.
    Renamed,
}

/// One file's share of a session's diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct FileDiff {
    /// The file's path now, relative to the checkout root.
    pub path: String,
    /// Where it was before, when [`change`](Self::change) is
    /// [`FileChange::Renamed`].
    pub previous_path: Option<String>,
    /// What happened to it.
    pub change: FileChange,
    /// Lines this file gained.
    pub added_lines: u32,
    /// Lines this file lost.
    pub removed_lines: u32,
    /// Whether git could not diff it as text.
    pub binary: bool,
    /// The file's own unified diff, hunk headers included.
    ///
    /// `None` when there is no text to show: a binary file, or a diff that
    /// had already spent [`DIFF_PATCH_BYTES_MAX`] on the files before it —
    /// which the containing [`WorkdirDiff::truncated`] announces.
    pub patch: Option<String>,
}

/// Everything a session has changed, against the branch it started from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct WorkdirDiff {
    /// The git ref the working tree was compared against.
    pub base: String,
    /// One entry per changed file, in git's own order.
    pub files: Vec<FileDiff>,
    /// Lines gained across every file, including files whose patch was
    /// left out.
    pub added_lines: u32,
    /// Lines lost across every file.
    pub removed_lines: u32,
    /// Whether some patches were left out for size.
    pub truncated: bool,
}

/// What a browser is asking the session's disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "ask", rename_all = "snake_case")]
pub enum WorkdirRequest {
    /// List one directory. An empty path is the workspace root — the
    /// directory holding every checkout.
    Entries {
        /// The directory, relative to the workspace root: `flyco/src` asks
        /// for `src` of the checkout at `flyco/`.
        path: String,
    },
    /// Read one text file.
    File {
        /// The file, relative to the workspace root on the same terms.
        path: String,
    },
    /// Diff one checkout's working tree against the branch it started on.
    Diff {
        /// Which checkout, as [`SessionRepo::dir`](crate::repo::SessionRepo::dir)
        /// names it.
        ///
        /// `None` asks for the workspace root itself — a session on a
        /// developer's machine, where the workdir *is* the checkout. A
        /// provisioned session has no root checkout, so `None` is refused
        /// there rather than answered with a diff of the workspace
        /// directory, which is not a repository at all.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
    },
}

/// Why the daemon would not answer a [`WorkdirRequest`].
///
/// Typed rather than a message, because the control plane turns each of
/// these into its own RFC 9457 problem and the UI says something different
/// for every one: a binary file offers the terminal, a file that is too
/// large says how large, and a path outside the checkout is a bug in the
/// caller rather than a state of the disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "refusal", rename_all = "snake_case")]
pub enum WorkdirRefusal {
    /// Nothing is at that path.
    NotFound {
        /// The path asked for.
        path: String,
    },
    /// The path leaves the checkout, names its `.git` directory, or is not
    /// a relative path at all.
    OutsideCheckout {
        /// The path asked for.
        path: String,
    },
    /// A listing was asked for something that is not a directory.
    NotADirectory {
        /// The path asked for.
        path: String,
    },
    /// Content was asked for something that is not a regular file.
    NotAFile {
        /// The path asked for.
        path: String,
    },
    /// The file is not UTF-8 text, so there is nothing to render.
    NotText {
        /// The path asked for.
        path: String,
    },
    /// The file is larger than [`FILE_BYTES_MAX`].
    TooLarge {
        /// The path asked for.
        path: String,
        /// What it actually measures, in bytes.
        bytes: u64,
    },
    /// The session has no base branch to diff against.
    ///
    /// A daemon started against a directory rather than a clone — the
    /// developer-machine shape — has no branch the session began at, and a
    /// diff against nothing is not something to invent.
    NoBaseBranch,
    /// The named checkout does not exist.
    ///
    /// A `Diff` that names a directory no repository is checked out into —
    /// a stale picker row, a repository added after the page loaded — is
    /// refused rather than answered with the workspace's own status, which
    /// is not a checkout's diff at all.
    UnknownCheckout {
        /// The `repo` the request named.
        repo: String,
    },
    /// git could not be run, or refused.
    Unreadable {
        /// What git said, for the log and for the problem detail.
        detail: String,
    },
}

/// What the daemon answers a [`WorkdirRequest`] with.
///
/// One flat enum rather than a `Result`-shaped pair of them: the reply is a
/// wire frame, and a single `outcome` tag is what keeps a refusal from
/// having to be nested inside a success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum WorkdirReply {
    /// A directory listing.
    Entries {
        /// The listing.
        listing: DirectoryListing,
    },
    /// A file's content.
    File {
        /// The content.
        content: FileContent,
    },
    /// The working tree's diff.
    Diff {
        /// The diff.
        diff: WorkdirDiff,
    },
    /// The daemon would not answer, and this is why.
    Refused {
        /// The refusal.
        refusal: WorkdirRefusal,
    },
}

impl WorkdirReply {
    /// Wraps a refusal, which is how every failure path builds one.
    #[must_use]
    pub const fn refused(refusal: WorkdirRefusal) -> Self {
        Self::Refused { refusal }
    }
}
