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

use crate::problem::Outcome;

/// Narrows the repository picker.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
pub struct RepoQuery {
    /// Substring to match against `owner/name`. Omitted lists the caller's
    /// repositories by most recent push.
    pub q: Option<String>,
}

/// Lists the caller's GitHub repositories, for the session-creation picker.
#[skyzen::openapi]
async fn list_repos(
    State(_user): State<CurrentUser>,
    Query(_query): Query<RepoQuery>,
    _db: Db,
) -> Outcome<Json<Vec<RepoSummary>>> {
    todo!("M6: unseal the caller's GitHub token and search their repositories through zenwave")
}

/// The user-scoped GitHub routes.
pub fn routes() -> Vec<RouteNode> {
    Route::new(("/v1/github/repos".at(list_repos),)).into_route_nodes()
}
