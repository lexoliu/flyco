//! Skills: the zipped bundles flyco installs into every session machine.
//!
//! A skill belongs to the user, not to a harness: Claude Code and Codex
//! read their global skills from different directories, and mounting the
//! same set into all of them is the daemon's job. A skill arrives by
//! upload on Settings → Tools or installed from a plugin marketplace; the
//! agent cannot write either directory, so publishing always goes through
//! the control plane, and a machine picks the change up the next time its
//! daemon starts.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::id::SkillId;

/// One row of `GET /v1/skills`, and the response of `GET /v1/skills/{id}`.
///
/// The bundle itself is not part of this: a skill zip is object-storage
/// content, not a JSON field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SkillView {
    /// Identifier.
    pub id: SkillId,
    /// Directory name the bundle is installed under.
    pub name: String,
    /// Size of the stored zip in bytes.
    pub size_bytes: u64,
    /// When it was last uploaded, seconds since the Unix epoch.
    pub uploaded_at_unix: u64,
}

/// One row of `GET /v1/sessions/{id}/skills`: the list a session's daemon
/// installs into every harness's global skills directory on the machine.
///
/// A slimmer projection than [`SkillView`] — the daemon needs the id to
/// ask for the bundle and the name to pick the directory it lands in.
/// `uploaded_at_unix` is the registry's bookkeeping, not the machine's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SkillMount {
    /// Identifier, which the daemon hands back to fetch the bundle.
    pub id: SkillId,
    /// Directory name the bundle is installed under.
    pub name: String,
    /// Size of the stored zip in bytes.
    pub size_bytes: u64,
}
