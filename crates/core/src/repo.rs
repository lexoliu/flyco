//! Repository identifiers.
//!
//! Flyco only ever works with GitHub repositories, so `owner/name` is
//! parsed once at the edge and carried as a type from then on: nothing
//! downstream has to re-check what a "repo" string contains.

use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Serialize};

/// Longest owner or name segment GitHub accepts.
const MAX_SEGMENT: usize = 100;

/// A GitHub repository in `owner/name` form.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(transparent)]
#[schema(value_type = String, description = "GitHub repository in `owner/name` form")]
pub struct RepoSlug(String);

/// Why a string is not a repository slug.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RepoSlugError {
    /// The value is not exactly one owner and one name separated by `/`.
    #[error("expected exactly one `/` separating owner and name")]
    Shape,
    /// An owner or name segment is empty, over-long, or contains a
    /// character GitHub does not allow.
    #[error("`{0}` is not a valid owner or repository name")]
    Segment(&'static str),
}

fn valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= MAX_SEGMENT
        && segment != "."
        && segment != ".."
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

impl RepoSlug {
    /// The owner segment.
    #[must_use]
    pub fn owner(&self) -> &str {
        self.split().0
    }

    /// The repository-name segment.
    #[must_use]
    pub fn name(&self) -> &str {
        self.split().1
    }

    /// The whole `owner/name` string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn split(&self) -> (&str, &str) {
        self.0
            .split_once('/')
            .expect("a RepoSlug always holds exactly one separator")
    }
}

impl FromStr for RepoSlug {
    type Err = RepoSlugError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (owner, name) = value.split_once('/').ok_or(RepoSlugError::Shape)?;
        if name.contains('/') {
            return Err(RepoSlugError::Shape);
        }
        if !valid_segment(owner) {
            return Err(RepoSlugError::Segment("owner"));
        }
        if !valid_segment(name) {
            return Err(RepoSlugError::Segment("name"));
        }
        Ok(Self(value.to_owned()))
    }
}

impl fmt::Display for RepoSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The working tree of a session's checkout, as `GET
/// /v1/sessions/{id}/repo-status` reports it.
///
/// Dirtiness is load-bearing rather than informational: an agent may not
/// stop while the tree is dirty, and archiving a dirty session warns the
/// user before the disk is released. The UI needs the same fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RepoStatus {
    /// Whether the working tree has changes that are not committed.
    pub dirty: bool,
    /// `git status --short`, as the daemon last read it. Empty when clean.
    pub summary: String,
}

#[cfg(test)]
mod tests {
    use super::{RepoSlug, RepoSlugError};

    #[test]
    fn a_well_formed_slug_splits_into_owner_and_name() {
        let slug: RepoSlug = "lexoliu/flyco".parse().expect("valid");
        assert_eq!(slug.owner(), "lexoliu");
        assert_eq!(slug.name(), "flyco");
        assert_eq!(slug.to_string(), "lexoliu/flyco");
    }

    #[test]
    fn dots_dashes_and_underscores_are_allowed() {
        assert!("some-org/dot.net_thing".parse::<RepoSlug>().is_ok());
    }

    #[test]
    fn anything_that_is_not_owner_slash_name_is_rejected() {
        for candidate in ["flyco", "a/b/c", "", "/"] {
            assert!(
                candidate.parse::<RepoSlug>().is_err(),
                "`{candidate}` must be rejected"
            );
        }
        assert_eq!(
            "flyco".parse::<RepoSlug>(),
            Err(RepoSlugError::Shape),
            "a missing separator is a shape error"
        );
    }

    #[test]
    fn illegal_characters_are_rejected() {
        for candidate in ["owner name/repo", "owner/re po", "owner/re:po", "own er/.."] {
            assert!(
                candidate.parse::<RepoSlug>().is_err(),
                "`{candidate}` must be rejected"
            );
        }
    }

    #[test]
    fn a_slug_round_trips_through_serde() {
        let slug: RepoSlug = "lexoliu/flyco".parse().expect("valid");
        let json = serde_json::to_string(&slug).expect("serialize");
        assert_eq!(json, "\"lexoliu/flyco\"");
        assert_eq!(
            serde_json::from_str::<RepoSlug>(&json).expect("deserialize"),
            slug
        );
    }
}
