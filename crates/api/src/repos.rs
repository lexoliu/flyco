//! The repository picker behind session creation.
//!
//! `POST /v1/sessions` takes an `owner/name` slug, and the only way a user
//! should have to produce one is by choosing from their own repositories.
//! The listing is read from GitHub with the caller's stored token rather
//! than mirrored into D1: a mirror would be stale the moment a repository is
//! created, renamed, or unshared, and the picker is exactly where that
//! matters.

use flyco_core::{CurrentUser, RepoSummary};
use serde::Deserialize;
use skyzen::extract::Query;
use skyzen::routing::{CreateRouteNode, Route, RouteNode, Routes as _};
use skyzen::utils::{Json, State};
use skyzen_services::Db;

use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::github::{GithubOauth, GithubToken};
use crate::problem::Outcome;
use crate::users;

/// Narrows the repository picker.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
pub struct RepoQuery {
    /// Substring to match against `owner/name`. Omitted lists the caller's
    /// repositories by most recent push.
    pub q: Option<String>,
}

/// Lists the caller's GitHub repositories, for the session-creation picker.
///
/// Generic over the GitHub client so tests can drive it without reaching
/// `api.github.com`, which is also why it carries no `#[skyzen::openapi]`:
/// the macro emits module-level items naming every argument type, and a type
/// parameter does not exist at module scope. The route still appears in the
/// document; only its schemas are missing.
async fn list_repos<G: GithubOauth>(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(github): State<G>,
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
async fn read<G: GithubOauth>(
    github: &G,
    config: &ApiConfig,
    db: &Db,
    user: &CurrentUser,
    query: &RepoQuery,
) -> Result<Vec<RepoSummary>, ApiError> {
    let sealed = users::sealed_github_token(db, user.id)
        .await?
        .ok_or(ApiError::CorruptRecord(
            "the user has no stored GitHub token",
        ))?;
    let token = GithubToken {
        access_token: config.token_cipher().open(&sealed)?,
    };

    let mut repos = github.list_repos(&token).await?;
    if let Some(needle) = query.q.as_ref().map(|q| q.trim().to_lowercase())
        && !needle.is_empty()
    {
        repos.retain(|repo| repo.slug.to_string().to_lowercase().contains(&needle));
    }
    Ok(repos)
}

/// The user-scoped GitHub routes.
pub fn routes<G: GithubOauth>() -> Vec<RouteNode> {
    Route::new(("/v1/github/repos".at(list_repos::<G>),)).into_route_nodes()
}
