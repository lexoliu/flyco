//! Plugin marketplaces: the GitHub repositories the skill catalog reads.
//!
//! A marketplace is a repository with `.claude-plugin/marketplace.json` in
//! it. flyco hosts no registry of its own — [`BUILT_IN_MARKETPLACE`] is
//! offered to everybody and the user adds whichever others they trust — so
//! this table is a list of repositories and nothing more; what is *in* one
//! lives in [`crate::skill_catalog`]'s cached document, because reading a
//! marketplace is a walk of its tree and a read of every `SKILL.md` in it.
//!
//! The built-in marketplace is deliberately not a row. A row per user
//! saying "and also the one everybody has" is the same fact written once
//! per account, and it would be removable, which it is not.

use core::str::FromStr as _;

use flyco_core::{
    AddMarketplace, BUILT_IN_MARKETPLACE, CurrentUser, MarketplaceId, MarketplaceView, RepoSlug,
    UserId,
};
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::sql;
use skyzen::utils::{Json, State};
use skyzen_services::{Db, Kv, Queue};

use crate::clock::now_unix;
use crate::error::ApiError;
use crate::extract::path_id;
use crate::problem::Outcome;
use crate::respond::{Created, NoContent};
use crate::skill_catalog;

/// The longest ref flyco will pin a marketplace to.
///
/// Git's own limit is far higher; this is the length past which a "branch"
/// is something else pasted into the field.
const MAX_REF_LEN: usize = 100;

/// One marketplace as the catalog reads it: where it is and at what ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marketplace {
    /// The repository.
    pub repo: RepoSlug,
    /// The ref the user pinned, or `None` for the default branch.
    pub git_ref: Option<String>,
}

impl Marketplace {
    /// The built-in marketplace, which every user has.
    ///
    /// # Panics
    ///
    /// Panics if [`BUILT_IN_MARKETPLACE`] stops being a `owner/name` slug,
    /// which is a compile-time literal and so a bug rather than input.
    #[must_use]
    pub fn built_in() -> Self {
        Self {
            repo: RepoSlug::from_str(BUILT_IN_MARKETPLACE)
                .expect("the built-in marketplace is a literal slug"),
            git_ref: None,
        }
    }
}

/// The columns every read on this path projects.
#[derive(Debug, skyzen::FromRow)]
struct MarketplaceRow {
    id: MarketplaceId,
    repo: String,
    git_ref: Option<String>,
    added_at_unix: u64,
}

impl From<MarketplaceRow> for MarketplaceView {
    fn from(row: MarketplaceRow) -> Self {
        Self {
            id: Some(row.id),
            repo: row.repo,
            git_ref: row.git_ref,
            built_in: false,
            added_at_unix: Some(row.added_at_unix),
        }
    }
}

/// The view of the marketplace nobody added.
fn built_in_view() -> MarketplaceView {
    MarketplaceView {
        id: None,
        repo: BUILT_IN_MARKETPLACE.to_owned(),
        git_ref: None,
        built_in: true,
        added_at_unix: None,
    }
}

/// Rejects a repository or ref flyco cannot read a marketplace from.
fn checked(request: &AddMarketplace) -> Result<(RepoSlug, Option<String>), ApiError> {
    let repo = RepoSlug::from_str(request.repo.trim())
        .map_err(|_| ApiError::InvalidMarketplace("a marketplace is a GitHub `owner/name`"))?;
    if repo.to_string() == BUILT_IN_MARKETPLACE {
        return Err(ApiError::InvalidMarketplace(
            "this marketplace is built in and is always available",
        ));
    }
    let git_ref = request
        .git_ref
        .as_deref()
        .map(str::trim)
        .filter(|git_ref| !git_ref.is_empty())
        .map(ToOwned::to_owned);
    if let Some(git_ref) = &git_ref
        && (git_ref.len() > MAX_REF_LEN
            || git_ref
                .chars()
                .any(|c| c.is_whitespace() || matches!(c, '?' | '#' | '~' | '^' | ':' | '\\')))
    {
        return Err(ApiError::InvalidMarketplace(
            "a ref is a branch or tag name",
        ));
    }
    Ok((repo, git_ref))
}

/// Lists the caller's marketplaces, the built-in one first.
#[skyzen::openapi]
async fn list_marketplaces(
    State(user): State<CurrentUser>,
    db: Db,
) -> Outcome<Json<Vec<MarketplaceView>>> {
    list(&db, user.id).await.map(Json).into()
}

async fn list(db: &Db, user: UserId) -> Result<Vec<MarketplaceView>, ApiError> {
    let mut listed = vec![built_in_view()];
    listed.extend(rows(db, user).await?.into_iter().map(Into::into));
    Ok(listed)
}

/// The caller's own marketplace rows, oldest first.
async fn rows(db: &Db, user: UserId) -> Result<Vec<MarketplaceRow>, ApiError> {
    Ok(sql!(
        db,
        "SELECT id, repo, git_ref, added_at_unix FROM marketplaces \
         WHERE user_id = {user} ORDER BY added_at_unix, id"
    )
    .fetch_all()
    .await?)
}

/// Every marketplace a user's catalog reads, the built-in one included.
///
/// # Errors
///
/// Returns [`ApiError`] if the database refuses the read.
pub async fn all(db: &Db, user: UserId) -> Result<Vec<Marketplace>, ApiError> {
    let mut all = vec![Marketplace::built_in()];
    for row in rows(db, user).await? {
        // A row whose slug no longer parses is one the database should not
        // hold; skipping it keeps a catalog readable rather than failing
        // every read on one bad row.
        if let Ok(repo) = RepoSlug::from_str(&row.repo) {
            all.push(Marketplace {
                repo,
                git_ref: row.git_ref,
            });
        }
    }
    Ok(all)
}

/// Adds a marketplace and asks for it to be read.
#[skyzen::openapi]
async fn add_marketplace(
    State(user): State<CurrentUser>,
    Json(request): Json<AddMarketplace>,
    db: Db,
    kv: Kv,
    queue: Queue,
) -> Outcome<Created<Json<MarketplaceView>>> {
    add(&db, &kv, &queue, user.id, request)
        .await
        .map(|view| Created(Json(view)))
        .into()
}

/// Inserts the row, then asks the queue to read the repository.
///
/// The read is asked for here rather than left to the first catalog request
/// so that a marketplace added now is listed by the time the user looks:
/// reading one is a tree walk plus a file per skill, which is not work a
/// request does.
async fn add(
    db: &Db,
    kv: &Kv,
    queue: &Queue,
    user: UserId,
    request: AddMarketplace,
) -> Result<MarketplaceView, ApiError> {
    let (repo, git_ref) = checked(&request)?;
    let slug = repo.to_string();
    let row: Option<MarketplaceRow> = sql!(
        db,
        "INSERT INTO marketplaces (id, user_id, repo, git_ref, added_at_unix) \
         VALUES ({MarketplaceId::generate()}, {user}, {slug.clone()}, {git_ref.clone()}, {now_unix()}) \
         ON CONFLICT (user_id, repo) DO NOTHING \
         RETURNING id, repo, git_ref, added_at_unix"
    )
    .fetch_optional()
    .await?;
    let row = row.ok_or(ApiError::MarketplaceAlreadyAdded { repo: slug })?;

    let marketplace = Marketplace { repo, git_ref };
    if skill_catalog::ask_for_refresh(kv, queue, user, &marketplace).await? {
        tracing::info!(repo = %marketplace.repo, "asked for a first read of a marketplace");
    }
    Ok(row.into())
}

/// Removes one of the caller's marketplaces.
#[skyzen::openapi]
async fn delete_marketplace(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Outcome<NoContent> {
    remove(&db, user.id, &params).await.into()
}

/// Deletes the row.
///
/// The cached document is left alone: it belongs to the repository rather
/// than to the user who happened to add it, it expires by itself, and a
/// marketplace added again should not have to be read a second time.
async fn remove(db: &Db, user: UserId, params: &Params) -> Result<NoContent, ApiError> {
    let id: MarketplaceId = path_id(params, "id")?;
    let removed: Option<MarketplaceRow> = sql!(
        db,
        "DELETE FROM marketplaces WHERE id = {id} AND user_id = {user} \
         RETURNING id, repo, git_ref, added_at_unix"
    )
    .fetch_optional()
    .await?;
    let removed = removed.ok_or(ApiError::MarketplaceNotFound)?;

    tracing::info!(marketplace = %removed.id, repo = %removed.repo, "removed a marketplace");
    Ok(NoContent)
}

/// The user-scoped marketplace routes.
pub fn routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/marketplaces"
            .at(list_marketplaces)
            .post(add_marketplace),
        "/v1/marketplaces/{id}".delete(delete_marketplace),
    ))
    .into_route_nodes()
}

#[cfg(test)]
mod tests {
    use flyco_core::AddMarketplace;

    use super::checked;

    fn request(repo: &str, git_ref: Option<&str>) -> AddMarketplace {
        AddMarketplace {
            repo: repo.to_owned(),
            git_ref: git_ref.map(ToOwned::to_owned),
        }
    }

    #[test]
    fn a_marketplace_is_a_slug_and_an_optional_ref() {
        let (repo, git_ref) = checked(&request(" owner/name ", Some(" v1.2 "))).expect("valid");
        assert_eq!(repo.to_string(), "owner/name");
        assert_eq!(git_ref.as_deref(), Some("v1.2"));

        let (_, blank) = checked(&request("owner/name", Some("  "))).expect("valid");
        assert_eq!(blank, None);
    }

    #[test]
    fn the_built_in_marketplace_cannot_be_added_again() {
        assert!(checked(&request("anthropics/skills", None)).is_err());
    }

    #[test]
    fn a_ref_that_is_not_a_ref_is_refused() {
        assert!(checked(&request("owner/name", Some("main?x=1"))).is_err());
        assert!(checked(&request("owner/name", Some("a branch"))).is_err());
        assert!(checked(&request("not-a-slug", None)).is_err());
    }
}
