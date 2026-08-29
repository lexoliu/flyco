//! Skills: the zipped bundles installed into every session's global skills
//! directory.
//!
//! The bundle is the request body, not a JSON field: a skill is a zip, and
//! base64 inside a document would double its size to no end. The name and
//! the harness it belongs to therefore ride the query string, which is what
//! keeps the body exactly the bytes that get stored.
//!
//! Agents cannot write either harness's skills directory — it is read-only
//! and a hook refuses the write — so an agent that wants to publish a skill
//! calls the `skill_upload` MCP tool, which lands on this same route.

use flyco_core::{CurrentUser, SkillScope, SkillView};
use serde::Deserialize;
use skyzen::extract::Query;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::utils::{Bytes, Json, State};
use skyzen_services::{Db, Storage};

use crate::problem::Outcome;
use crate::respond::{Created, NoContent};

/// What an uploaded zip is stored as.
#[derive(Debug, Deserialize, skyzen::ToSchema)]
pub struct UploadSkill {
    /// Directory name the bundle is installed under.
    pub name: String,
    /// Which harness's skills directory it belongs in. Claude Code and Codex
    /// read different ones, so this is not derivable from the bundle.
    pub scope: SkillScope,
}

/// Lists the caller's skills.
#[skyzen::openapi]
async fn list_skills(State(_user): State<CurrentUser>, _db: Db) -> Outcome<Json<Vec<SkillView>>> {
    todo!("M6: list skills for the caller, newest first")
}

/// Uploads a skill bundle, replacing any bundle of the same name and scope.
#[skyzen::openapi]
async fn upload_skill(
    State(_user): State<CurrentUser>,
    Query(_upload): Query<UploadSkill>,
    _storage: Storage,
    _db: Db,
    _body: Bytes,
) -> Outcome<Created<Json<SkillView>>> {
    todo!("M6: validate the zip, store it in R2, upsert the row, propagate to live sessions")
}

/// Describes one of the caller's skills.
#[skyzen::openapi]
async fn get_skill(
    State(_user): State<CurrentUser>,
    _params: Params,
    _db: Db,
) -> Outcome<Json<SkillView>> {
    todo!("M6: read one skills row scoped to the caller")
}

/// Removes one of the caller's skills.
#[skyzen::openapi]
async fn delete_skill(
    State(_user): State<CurrentUser>,
    _params: Params,
    _storage: Storage,
    _db: Db,
) -> Outcome<NoContent> {
    todo!("M6: delete the row and its R2 object, then withdraw it from live sessions")
}

/// The user-scoped skill routes.
pub fn routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/skills".at(list_skills).post(upload_skill),
        "/v1/skills/{id}".at(get_skill).delete(delete_skill),
    ))
    .into_route_nodes()
}
