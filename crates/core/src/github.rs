//! GitHub DTOs the control plane serves.
//!
//! Flyco's only identity provider is GitHub and its only source of code is a
//! GitHub repository, so the session-creation picker is a list of the
//! caller's repositories reduced to what the picker actually renders.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::repo::RepoSlug;

/// One row of `GET /v1/github/repos`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RepoSummary {
    /// `owner/name`, which is what `POST /v1/sessions` takes.
    pub slug: RepoSlug,
    /// Whether the repository is private.
    pub private: bool,
    /// Branch a session starts from unless the user names another.
    pub default_branch: String,
    /// GitHub's description, when the repository has one.
    pub description: Option<String>,
    /// Last push, seconds since the Unix epoch, so the picker can order by
    /// what the user is actually working on.
    pub pushed_at_unix: Option<u64>,
}
