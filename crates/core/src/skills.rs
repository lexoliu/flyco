//! Skills: the zipped bundles flyco installs into every session machine.
//!
//! Claude Code and Codex read their global skills from different
//! directories, so a skill is stored against the [`SkillScope`] it belongs
//! in and installed into that harness's directory. The agent cannot write
//! either directory — it uploads through the `skill_upload` MCP tool, which
//! lands here — and every live session picks up the change.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::id::SkillId;

/// Which harness's global skills directory a bundle belongs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "sql", derive(skyzen::Column))]
pub enum SkillScope {
    /// Claude Code's skills directory.
    Claude,
    /// Codex's skills directory.
    Codex,
}

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
    /// Which harness gets it.
    pub scope: SkillScope,
    /// Size of the stored zip in bytes.
    pub size_bytes: u64,
    /// When it was last uploaded, seconds since the Unix epoch.
    pub uploaded_at_unix: u64,
}

/// One row of `GET /v1/sessions/{id}/skills`: the list a session's daemon
/// installs into the harness's global skills directory.
///
/// A slimmer projection than [`SkillView`] — the daemon needs the id to
/// ask for the bundle, the name to pick the directory it lands in, and the
/// scope to filter to its own harness. `uploaded_at_unix` is the registry's
/// bookkeeping, not the machine's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SkillMount {
    /// Identifier, which the daemon hands back to fetch the bundle.
    pub id: SkillId,
    /// Directory name the bundle is installed under.
    pub name: String,
    /// Which harness gets it.
    pub scope: SkillScope,
    /// Size of the stored zip in bytes.
    pub size_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::SkillScope;

    #[test]
    fn scopes_use_the_tokens_the_schema_stores() {
        for (scope, token) in [(SkillScope::Claude, "claude"), (SkillScope::Codex, "codex")] {
            assert_eq!(
                serde_json::to_value(scope).expect("serialize"),
                serde_json::Value::String(token.to_owned())
            );
        }
    }
}
