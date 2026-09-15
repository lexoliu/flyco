//! The `session_repos` table.
//!
//! One row per repository a session checks out, ordered by `position`:
//! `session_repos` replaces `sessions.repo`/`sessions.branch`, which held
//! exactly one (issue #280). The first row is the primary — the repository
//! the session header names — and every later row is one the user added
//! themselves or approved for the agent.
//!
//! `dir` is the checkout's identity inside the workspace: the directory
//! under the session's workdir it is cloned into, unique per session, and
//! the name a diff request, a dirty status and a stored workdir patch all
//! refer to the checkout by.

use flyco_core::{BranchName, RepoAddedBy, RepoSlug, SessionId, SessionRepo, checkout_dir};
use skyzen::sql;
use skyzen_services::Db;

use crate::clock::now_unix;
use crate::error::ApiError;

/// A row of `session_repos`, in the order the session carries them.
#[derive(Debug, skyzen::FromRow)]
struct SessionRepoRow {
    slug: RepoSlug,
    branch: Option<BranchName>,
    dir: String,
    added_by: RepoAddedBy,
}

impl From<SessionRepoRow> for SessionRepo {
    fn from(row: SessionRepoRow) -> Self {
        Self {
            slug: row.slug,
            branch: row.branch,
            dir: row.dir,
            added_by: row.added_by,
        }
    }
}

/// The repositories a session checks out, primary first.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails or a stored row is malformed.
pub async fn of_session(db: &Db, session: SessionId) -> Result<Vec<SessionRepo>, ApiError> {
    let rows: Vec<SessionRepoRow> = sql!(
        db,
        "SELECT slug, branch, dir, added_by FROM session_repos \
         WHERE session_id = {session} ORDER BY position"
    )
    .fetch_all()
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// The repository a session carries under `dir`, when it carries one.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails or the stored row is
/// malformed.
pub async fn by_dir(
    db: &Db,
    session: SessionId,
    dir: &str,
) -> Result<Option<SessionRepo>, ApiError> {
    let row: Option<SessionRepoRow> = sql!(
        db,
        "SELECT slug, branch, dir, added_by FROM session_repos \
         WHERE session_id = {session} AND dir = {dir}"
    )
    .fetch_optional()
    .await?;
    Ok(row.map(Into::into))
}

/// Puts a repository on the session and answers the row it now carries.
///
/// The directory is chosen here — the repository's own name, or
/// `owner--name` when a checkout already holds that — because the set it
/// is chosen against is the one this reads, and a caller choosing it
/// separately could only produce a collision the `UNIQUE` index would have
/// to catch anyway. The position is the next after the last, so a session's
/// order is the order its repositories were added in and the primary stays
/// first for its whole life.
///
/// # Errors
///
/// Returns [`ApiError::RepoAlreadyAttached`] when the session already
/// carries the repository, or [`ApiError`] if the database fails.
pub async fn attach(
    db: &Db,
    session: SessionId,
    slug: &RepoSlug,
    branch: &BranchName,
    added_by: RepoAddedBy,
) -> Result<SessionRepo, ApiError> {
    if by_slug(db, session, slug).await?.is_some() {
        return Err(ApiError::RepoAlreadyAttached { repo: slug.clone() });
    }
    let existing = of_session(db, session).await?;
    if existing.len() >= flyco_core::MAX_SESSION_REPOS {
        return Err(ApiError::SessionRepoCapReached {
            cap: flyco_core::MAX_SESSION_REPOS,
        });
    }
    let mut taken: std::collections::BTreeSet<String> =
        existing.into_iter().map(|repo| repo.dir).collect();
    let dir = checkout_dir(slug, &mut taken);
    let position = next_position(db, session).await?;
    let now = now_unix();
    sql!(
        db,
        "INSERT INTO session_repos \
         (session_id, position, slug, branch, dir, added_by, added_at_unix) \
         VALUES ({session}, {position}, {slug}, {branch}, {dir.clone()}, {added_by}, {now})"
    )
    .execute()
    .await?;
    Ok(SessionRepo {
        slug: slug.clone(),
        branch: Some(branch.clone()),
        dir,
        added_by,
    })
}

/// Whether the session already carries the repository.
async fn by_slug(
    db: &Db,
    session: SessionId,
    slug: &RepoSlug,
) -> Result<Option<SessionRepo>, ApiError> {
    let row: Option<SessionRepoRow> = sql!(
        db,
        "SELECT slug, branch, dir, added_by FROM session_repos \
         WHERE session_id = {session} AND slug = {slug}"
    )
    .fetch_optional()
    .await?;
    Ok(row.map(Into::into))
}

/// The position a new row takes: one past the session's last.
async fn next_position(db: &Db, session: SessionId) -> Result<u32, ApiError> {
    Ok(sql!(
        db,
        "SELECT COALESCE(MAX(position) + 1, 0) AS position FROM session_repos \
         WHERE session_id = {session}"
    )
    .fetch_scalar()
    .await?)
}

/// Records the branch a checkout works on.
///
/// Written by `POST /v1/sessions` through [`crate::sessions::create`] for
/// every repository a session opens with, and by the provisioning queue for
/// a row whose branch was not resolved yet — which resolves the
/// repository's default from GitHub the first time the session is put on a
/// machine. Recording it there rather than resolving it again on every
/// provision is what makes a checkout's branch stable: a repository whose
/// default moves must not move a session already built on the old one.
///
/// # Errors
///
/// Returns [`ApiError`] if the write fails.
pub async fn record_branch(
    db: &Db,
    session: SessionId,
    dir: &str,
    branch: &BranchName,
) -> Result<(), ApiError> {
    sql!(
        db,
        "UPDATE session_repos SET branch = {branch} \
         WHERE session_id = {session} AND dir = {dir}"
    )
    .execute()
    .await?;
    Ok(())
}
