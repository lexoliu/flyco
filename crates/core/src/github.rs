//! GitHub DTOs the control plane serves.
//!
//! Flyco's only identity provider is GitHub and its only source of code is a
//! GitHub repository, so the session-creation picker is a list of the
//! caller's repositories reduced to what the picker actually renders.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::repo::{BranchName, RepoSlug};

/// One row of `GET /v1/github/repos`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RepoSummary {
    /// `owner/name`, which is what `POST /v1/sessions` takes.
    pub slug: RepoSlug,
    /// Whether the repository is private.
    pub private: bool,
    /// Branch a session starts from unless the user names another.
    pub default_branch: BranchName,
    /// GitHub's description, when the repository has one.
    pub description: Option<String>,
    /// Last push, seconds since the Unix epoch, so the picker can order by
    /// what the user is actually working on.
    pub pushed_at_unix: Option<u64>,
}

/// One row of `GET /v1/github/repos/{owner}/{name}/branches`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct BranchSummary {
    /// The branch, as git spells it.
    pub name: BranchName,
    /// Whether this is the repository's default branch.
    ///
    /// Carried rather than worked out by the client: the first row of the
    /// first page *is* the default branch, and a client deriving that for
    /// itself would be repeating the question the control plane already
    /// asked GitHub.
    pub is_default: bool,
}

/// One page of `GET /v1/github/repos/{owner}/{name}/branches`.
///
/// The default branch is the first row of the first page and appears on no
/// other page, so a picker opens on the branch a session would otherwise
/// take without reading to the end of a repository with two hundred of them.
///
/// The cursor is opaque: it names a position in GitHub's own listing, and a
/// client that stores it must hand it back unread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct BranchPage {
    /// The branches, default first and the rest as GitHub orders them.
    pub branches: Vec<BranchSummary>,
    /// Cursor to pass as `cursor` for the next page, or `None` at the end.
    pub next_cursor: Option<String>,
}
