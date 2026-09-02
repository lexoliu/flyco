//! The repository and branch pickers behind session creation.
//!
//! `POST /v1/sessions` takes an `owner/name` slug and a branch, and the only
//! way a user should have to produce either is by choosing from what GitHub
//! says they have. Both listings are read from GitHub with the caller's
//! stored token rather than mirrored into D1: a mirror would be stale the
//! moment a repository is created, renamed, or unshared — or a branch pushed
//! — and a picker is exactly where that matters.

use flyco_core::{BranchPage, BranchSummary, CurrentUser, RepoSlug, RepoSummary};
use serde::Deserialize;
use skyzen::extract::Query;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::utils::{Json, State};
use skyzen_services::Db;

use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::extract::path_segment;
use crate::github::{GithubClient, GithubOauth};
use crate::problem::Outcome;
use crate::users;

/// Narrows the repository picker.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
pub struct RepoQuery {
    /// Substring to match against `owner/name`. Omitted lists the caller's
    /// repositories by most recent push.
    pub q: Option<String>,
}

/// Asks for one page of a repository's branches.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
pub struct BranchQuery {
    /// Cursor from a previous page's `next_cursor`. Omitted starts at the
    /// first page, whose first row is the repository's default branch.
    pub cursor: Option<String>,
}

/// The first page, when a request names no cursor.
const FIRST_PAGE: u32 = 1;

/// Lists the caller's GitHub repositories, for the session-creation picker.
///
/// The client is the concrete [`GithubClient`] rather than a type
/// parameter: an annotated handler cannot be generic, and a generic one
/// would carry the substituted type into its operation id, which is not a
/// name a generated client can be written against.
#[skyzen::openapi]
async fn list_repos(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(github): State<GithubClient>,
    Query(query): Query<RepoQuery>,
    db: Db,
) -> Outcome<Json<Vec<RepoSummary>>> {
    read(&github, &config, &db, &user, &query)
        .await
        .map(Json)
        .into()
}

/// Reads the picker's listing with the caller's own stored token.
///
/// The filter is applied here rather than by GitHub: the listing is one page
/// of the caller's own repositories, and searching server-side would need
/// GitHub's search API, whose relevance ordering is wrong for a picker that
/// wants "what I was last working on".
async fn read(
    github: &GithubClient,
    config: &ApiConfig,
    db: &Db,
    user: &CurrentUser,
    query: &RepoQuery,
) -> Result<Vec<RepoSummary>, ApiError> {
    let token = users::github_token(db, config, user.id).await?;

    let mut repos = github.list_repos(&token).await?;
    if let Some(needle) = query.q.as_ref().map(|q| q.trim().to_lowercase())
        && !needle.is_empty()
    {
        repos.retain(|repo| repo.slug.to_string().to_lowercase().contains(&needle));
    }
    Ok(repos)
}

/// Lists one repository's branches, default branch first.
#[skyzen::openapi]
async fn list_branches(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(github): State<GithubClient>,
    Query(query): Query<BranchQuery>,
    params: Params,
    db: Db,
) -> Outcome<Json<BranchPage>> {
    branches(&github, &config, &db, &user, &params, &query)
        .await
        .map(Json)
        .into()
}

/// Reads one page of branches with the caller's own stored token.
///
/// The default branch is *hoisted*, not merely marked: GitHub orders
/// branches alphabetically, so a repository whose default is `main` and
/// whose first alphabetical branch is `add-tests` would open the picker on a
/// branch nobody asked for. It is put at the head of the first page and
/// removed from every page, so it appears exactly once and always first.
async fn branches(
    github: &GithubClient,
    config: &ApiConfig,
    db: &Db,
    user: &CurrentUser,
    params: &Params,
    query: &BranchQuery,
) -> Result<BranchPage, ApiError> {
    let slug = slug_from(params)?;
    let page = page_from(query)?;
    let token = users::github_token(db, config, user.id).await?;

    // Asked for on every page rather than only the first: it is what marks
    // the default row, and a client paging forward must not have to remember
    // an answer the first page happened to carry.
    let default_branch = github.get_repo(&token, &slug).await?.default_branch;
    let listing = github.list_branches(&token, &slug, page).await?;

    let head = (page == FIRST_PAGE).then(|| BranchSummary {
        name: default_branch.clone(),
        is_default: true,
    });
    let branches = head
        .into_iter()
        .chain(
            listing
                .names
                .into_iter()
                .filter(|name| name != &default_branch)
                .map(|name| BranchSummary {
                    name,
                    is_default: false,
                }),
        )
        .collect();

    Ok(BranchPage {
        branches,
        next_cursor: listing.has_more.then(|| page.saturating_add(1).to_string()),
    })
}

/// Builds the slug from the route's own `{owner}` and `{name}`.
///
/// Parsed rather than concatenated and trusted: the two segments reach the
/// GitHub API in a URL path, and a segment holding a `/` or a `..` would
/// address something else entirely.
fn slug_from(params: &Params) -> Result<RepoSlug, ApiError> {
    let owner = path_segment(params, "owner")?;
    let name = path_segment(params, "name")?;
    let slug = format!("{owner}/{name}");
    slug.parse().map_err(|_| ApiError::InvalidRepo(slug))
}

/// Reads the opaque cursor back as the page number it encodes.
fn page_from(query: &BranchQuery) -> Result<u32, ApiError> {
    query.cursor.as_deref().map_or(Ok(FIRST_PAGE), |cursor| {
        cursor
            .parse::<u32>()
            .ok()
            .filter(|page| *page >= FIRST_PAGE)
            .ok_or_else(|| ApiError::InvalidCursor(cursor.to_owned()))
    })
}

/// The user-scoped GitHub routes.
pub fn routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/github/repos".at(list_repos),
        "/v1/github/repos/{owner}/{name}/branches".at(list_branches),
    ))
    .into_route_nodes()
}

#[cfg(test)]
mod tests {
    use flyco_core::{BranchPage, RepoSummary};
    use skyzen_services::{Db, Kv};
    use skyzen_test::TestContext;

    use crate::session;
    use crate::testing::{TEST_BRANCHES, TEST_DEFAULT_BRANCH, migrated_router, seed_user};

    /// The repository whose branches the picker lists.
    const REPO: &str = "lexoliu/flyco";

    async fn signed_in(kv: &Kv, db: &Db) -> String {
        let user = seed_user(db).await;
        session::issue(kv, user.id).await.expect("issue a session")
    }

    #[skyzen::test]
    async fn the_picker_lists_the_callers_repositories(ctx: TestContext, kv: Kv, db: Db) {
        let client = ctx.client(migrated_router(&db).await);
        let token = signed_in(&kv, &db).await;

        let response = client.get("/v1/github/repos").bearer(&token).send().await;
        response.assert_status(200);
        let repos: Vec<RepoSummary> = response.json();
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0].slug.to_string(), REPO);
        assert_eq!(repos[0].default_branch.to_string(), TEST_DEFAULT_BRANCH);
    }

    #[skyzen::test]
    async fn branches_open_on_the_default_and_never_repeat_it(ctx: TestContext, kv: Kv, db: Db) {
        let client = ctx.client(migrated_router(&db).await);
        let token = signed_in(&kv, &db).await;

        let response = client
            .get(&format!("/v1/github/repos/{REPO}/branches"))
            .bearer(&token)
            .send()
            .await;
        response.assert_status(200);
        let page: BranchPage = response.json();

        // GitHub orders branches alphabetically, so `add-tests` comes back
        // first; the picker must still open on the branch a session would
        // otherwise take.
        assert_eq!(page.branches[0].name.to_string(), TEST_DEFAULT_BRANCH);
        assert!(page.branches[0].is_default);
        assert_eq!(
            page.branches
                .iter()
                .filter(|branch| branch.name.to_string() == TEST_DEFAULT_BRANCH)
                .count(),
            1,
            "the default branch is hoisted, not duplicated"
        );
        assert_eq!(page.branches.len(), TEST_BRANCHES.len());
        assert!(
            page.branches[1..].iter().all(|branch| !branch.is_default),
            "exactly one row is the default"
        );
        assert_eq!(
            page.next_cursor, None,
            "a repository with three branches is one page"
        );
    }

    #[skyzen::test]
    async fn a_cursor_this_api_did_not_issue_is_refused(ctx: TestContext, kv: Kv, db: Db) {
        let client = ctx.client(migrated_router(&db).await);
        let token = signed_in(&kv, &db).await;

        let response = client
            .get(&format!("/v1/github/repos/{REPO}/branches?cursor=nonsense"))
            .bearer(&token)
            .send()
            .await;

        response.assert_status(400);
        assert_eq!(
            response.json::<flyco_core::Problem>().kind,
            "https://flyco.dev/problems/invalid-cursor"
        );
    }

    #[skyzen::test]
    async fn the_pickers_answer_only_to_a_signed_in_caller(ctx: TestContext, db: Db) {
        let client = ctx.client(migrated_router(&db).await);

        client
            .get("/v1/github/repos")
            .send()
            .await
            .assert_status(401);
        client
            .get(&format!("/v1/github/repos/{REPO}/branches"))
            .send()
            .await
            .assert_status(401);
    }
}
