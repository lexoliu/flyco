//! Repository identifiers.
//!
//! Flyco only ever works with GitHub repositories, so `owner/name` is
//! parsed once at the edge and carried as a type from then on: nothing
//! downstream has to re-check what a "repo" string contains.

use core::fmt;
use core::str::FromStr;
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Longest owner or name segment GitHub accepts.
const MAX_SEGMENT: usize = 100;

/// Most repositories one session may carry.
///
/// A bound rather than `Vec`'s own limit, because every repository costs a
/// clone at boot and a watcher for the session's life: a request listing a
/// hundred is a client bug, and refusing it at the edge is cheaper than
/// discovering it on the machine.
pub const MAX_SESSION_REPOS: usize = 16;

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

/// Longest branch name flyco accepts.
///
/// Git itself is bounded only by the filesystem's path limit; a name this
/// long is a mistake rather than a branch, and a bound stated here is one
/// the database column and the clone command both inherit.
const MAX_BRANCH: usize = 255;

/// A git branch name, checked against `git check-ref-format`'s rules.
///
/// A session's branch is user input that ends up as an argument to `git
/// clone --branch`, and a name git would refuse is a machine that boots and
/// then fails its checkout minutes later. Parsing it at the edge moves that
/// refusal to where the user typed it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(transparent)]
#[schema(value_type = String, description = "Git branch name")]
pub struct BranchName(String);

/// Why a string is not a branch name.
///
/// One variant per rule `git check-ref-format` enforces that flyco can check
/// without a repository, because "invalid branch" alone tells the user
/// nothing about which character to delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BranchNameError {
    /// The name is empty, or longer than [`MAX_BRANCH`].
    #[error("a branch name must be between 1 and {MAX_BRANCH} characters")]
    Length,
    /// The name contains a byte git refuses in a ref: a control character,
    /// a space, or one of ``~^:?*[\``.
    #[error("`{0}` is not allowed in a branch name")]
    Character(char),
    /// The name contains `..`, `@{`, or ends in `.lock`.
    #[error("a branch name cannot contain `..` or `@{{`, or end in `.lock`")]
    Sequence,
    /// A slash-separated component is empty, or begins with `.`, or the
    /// whole name begins or ends with `/`, `.` or `-`.
    ///
    /// The leading `-` is flyco's own rule rather than git's: a name that
    /// begins with a dash is one an argument parser reads as an option.
    #[error(
        "a branch name cannot begin or end with `/`, `.` or `-`, and no part of it may begin with `.`"
    )]
    Component,
}

impl BranchName {
    /// The name as git spells it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for BranchName {
    type Err = BranchNameError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() || value.len() > MAX_BRANCH {
            return Err(BranchNameError::Length);
        }
        if let Some(bad) = value
            .chars()
            .find(|c| c.is_control() || matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
        {
            return Err(BranchNameError::Character(bad));
        }
        // `.lock` is checked per component, which is how git states it: the
        // suffix collides with the lock file git writes beside a ref, and a
        // ref is a path.
        if value.contains("..")
            || value.contains("@{")
            || value
                .split('/')
                .any(|part| part.strip_suffix(".lock").is_some())
        {
            return Err(BranchNameError::Sequence);
        }
        if value.starts_with('-')
            || value.starts_with('/')
            || value.ends_with('.')
            || value.ends_with('/')
            || value
                .split('/')
                .any(|part| part.is_empty() || part.starts_with('.'))
        {
            return Err(BranchNameError::Component);
        }
        Ok(Self(value.to_owned()))
    }
}

impl fmt::Display for BranchName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Who attached a repository to a session.
///
/// Recorded because the two paths carry different weight: a repository the
/// user picked was chosen before the machine booted, while one the agent
/// asked for reached the session through an approval the user granted —
/// and a UI listing the checkouts owes the reader that distinction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "sql", derive(skyzen::Column))]
pub enum RepoAddedBy {
    /// The user selected it, at creation or mid-session.
    User,
    /// The agent asked for it and the user approved.
    Agent,
}

/// A repository a session checks out, as `SessionSummary::repos` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SessionRepo {
    /// The repository, `owner/name`.
    pub slug: RepoSlug,
    /// The branch the checkout is on.
    ///
    /// `None` only for a repository recorded before its branch could be
    /// resolved — the provisioning queue asks GitHub for the default at
    /// provision and writes it back, so a session that has been on a
    /// machine always names one.
    pub branch: Option<BranchName>,
    /// The directory under the session's workdir this checkout lives in.
    ///
    /// How the browser names one checkout of several: workdir paths are
    /// workspace-relative, so `dir` is both the tree's top level and the
    /// identity a `Diff` request or a dirty status carries.
    pub dir: String,
    /// Who put it on the session.
    pub added_by: RepoAddedBy,
}

/// One repository a request asks a session to work in.
///
/// The `repos` entry of [`CreateSession`](crate::session::CreateSession)
/// and the body of `POST /v1/sessions/{id}/repos`. Strings rather than
/// typed values for the same reason [`CreateSession`]'s other fields are:
/// this is untrusted input, and the control plane's parse of it is the
/// refusal that tells the caller which character git would not accept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RepoSelection {
    /// Repository to check out, `owner/name`.
    pub repo: String,
    /// Branch to check out.
    ///
    /// Omitted, the control plane asks GitHub for the repository's default
    /// branch and records *that*, so a session always names the branch each
    /// checkout works on rather than leaving every later reader to guess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// The directory a repository checks out into, under the session's workdir.
///
/// The repository's own name while that is free, `owner--name` when another
/// checkout already holds it, `owner--name-2` and counting past that —
/// every candidate stays inside the characters [`RepoSlug`] already
/// admits, so a name chosen here can never leave the workspace or collide
/// with `.git`. `taken` is the set the answer is chosen against *and* the
/// set it is recorded into: the call that returns a directory has claimed
/// it.
pub fn checkout_dir(slug: &RepoSlug, taken: &mut BTreeSet<String>) -> String {
    let mut candidate = slug.name().to_owned();
    if taken.contains(&candidate) {
        candidate = format!("{}--{}", slug.owner(), slug.name());
        for suffix in 2.. {
            if !taken.contains(&candidate) {
                break;
            }
            candidate = format!("{}--{}-{suffix}", slug.owner(), slug.name());
        }
    }
    taken.insert(candidate.clone());
    candidate
}

/// The working trees of a session's checkouts, as `GET
/// /v1/sessions/{id}/repo-status` reports them.
///
/// Dirtiness is load-bearing rather than informational: an agent may not
/// stop while any tree is dirty, and archiving a dirty session warns the
/// user before the disk is released. The UI needs the same fact per
/// checkout, because a session can work across several repositories at
/// once and `dirty` without *which* would be half the answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RepoStatus {
    /// One entry per checkout the daemon has reported on.
    ///
    /// A session that has reported nothing answers with an empty list —
    /// not a failure: no daemon has attached yet, or no watcher has
    /// finished a first pass.
    pub checkouts: Vec<CheckoutStatus>,
}

impl RepoStatus {
    /// Whether any checkout holds uncommitted work.
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.checkouts.iter().any(|checkout| checkout.dirty)
    }

    /// `git status` across the dirty checkouts, one section per checkout.
    ///
    /// What a dirty-archive refusal carries: the user is asked to discard
    /// work, and a bare "something is dirty" does not tell them what or
    /// where. A checkout that names no directory is the workspace root —
    /// the single-repo shape — and needs no heading of its own.
    #[must_use]
    pub fn dirty_summary(&self) -> String {
        self.checkouts
            .iter()
            .filter(|checkout| checkout.dirty)
            .map(|checkout| {
                checkout.dir.as_ref().map_or_else(
                    || checkout.summary.clone(),
                    |dir| format!("{dir}:\n{}", checkout.summary),
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// One checkout's working-tree state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CheckoutStatus {
    /// Which checkout, as [`SessionRepo::dir`] names it.
    ///
    /// `None` is the workspace root itself — the shape of a session on a
    /// developer's machine, where the workdir *is* the checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// Whether the working tree has changes that are not committed.
    pub dirty: bool,
    /// `git status --short`, as the daemon last read it. Empty when clean.
    pub summary: String,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{RepoSlug, RepoSlugError, checkout_dir};

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
    fn ordinary_branch_names_parse() {
        for candidate in [
            "main",
            "dev",
            "feat/issue-73-repo-clone",
            "release-1.0",
            "v2.1",
        ] {
            assert!(
                candidate.parse::<super::BranchName>().is_ok(),
                "`{candidate}` is a branch git would accept"
            );
        }
        assert_eq!(
            "dev".parse::<super::BranchName>().expect("valid").as_str(),
            "dev"
        );
    }

    #[test]
    fn names_git_would_refuse_are_refused_here() {
        for candidate in [
            "",
            "a b",
            "feat~1",
            "feat^",
            "ns:branch",
            "what?",
            "star*",
            "brack[et",
            "back\\slash",
            "a..b",
            "a@{0}",
            "wip.lock",
            ".hidden",
            "trailing.",
            "/leading",
            "trailing/",
            "a//b",
            "a/.b",
        ] {
            assert!(
                candidate.parse::<super::BranchName>().is_err(),
                "`{candidate}` must be rejected"
            );
        }
    }

    #[test]
    fn a_name_that_looks_like_an_option_is_refused() {
        // `git clone --branch` would read this as a flag rather than a ref,
        // which is exactly the argument a user must not be able to smuggle in.
        assert_eq!(
            "--upload-pack=whatever".parse::<super::BranchName>(),
            Err(super::BranchNameError::Component)
        );
    }

    #[test]
    fn a_branch_round_trips_through_serde() {
        let branch: super::BranchName = "dev".parse().expect("valid");
        let json = serde_json::to_string(&branch).expect("serialize");
        assert_eq!(json, "\"dev\"");
        assert_eq!(
            serde_json::from_str::<super::BranchName>(&json).expect("deserialize"),
            branch
        );
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

    #[test]
    fn a_checkout_directory_is_the_repositories_own_name() {
        let mut taken = BTreeSet::new();
        let slug: RepoSlug = "lexoliu/flyco".parse().expect("valid");
        assert_eq!(checkout_dir(&slug, &mut taken), "flyco");
        assert!(taken.contains("flyco"));
    }

    #[test]
    fn two_repositories_with_one_name_do_not_share_a_directory() {
        let mut taken = BTreeSet::new();
        let first: RepoSlug = "alice/sdk".parse().expect("valid");
        let second: RepoSlug = "bob/sdk".parse().expect("valid");
        let third: RepoSlug = "carol/sdk".parse().expect("valid");
        assert_eq!(checkout_dir(&first, &mut taken), "sdk");
        assert_eq!(checkout_dir(&second, &mut taken), "bob--sdk");
        // A repository literally named `bob--sdk` cannot sneak into the
        // directory the fallback already claimed.
        let squat: RepoSlug = "zed/bob--sdk".parse().expect("valid");
        assert_eq!(checkout_dir(&squat, &mut taken), "zed--bob--sdk");
        assert_eq!(checkout_dir(&third, &mut taken), "carol--sdk");
    }

    #[test]
    fn a_checkout_directory_stays_inside_segment_characters() {
        let mut taken = BTreeSet::new();
        let slug: RepoSlug = "o-w.n_e/r.ep".parse().expect("valid");
        let dir = checkout_dir(&slug, &mut taken);
        assert_eq!(dir, "r.ep");
        assert!(!dir.contains('/'));
    }
}
