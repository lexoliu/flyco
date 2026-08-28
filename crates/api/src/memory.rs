//! Tree memory: the memory system flyco serves to agents over MCP, in place
//! of the harnesses' file-based ones.
//!
//! A node is scoped to a repository or shared across all of them, and recall
//! is a walk down the tree — list the children of a parent, read one node —
//! rather than a grep over a directory of Markdown. The agent reaches the
//! same store through flycod's `memory_*` MCP tools; these routes are what
//! the user's own UI reads and writes.

use flyco_core::{CreateMemoryNode, CurrentUser, MemoryNode, MemoryNodeId, UpdateMemoryNode};
use serde::Deserialize;
use skyzen::Response;
use skyzen::extract::Query;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::utils::{Json, State};
use skyzen_services::Db;

use crate::problem::Outcome;
use crate::respond::Created;

/// Which level of the tree to list.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
pub struct MemoryFilter {
    /// Only nodes about this repository, `owner/name`. Omitted lists the
    /// memory that applies wherever the caller's agents run.
    pub repo: Option<String>,
    /// List this node's children. Omitted lists the roots.
    pub parent: Option<MemoryNodeId>,
}

/// Lists one level of the caller's memory tree.
#[skyzen::openapi]
async fn list_memory(
    State(_user): State<CurrentUser>,
    Query(_filter): Query<MemoryFilter>,
    _db: Db,
) -> Outcome<Json<Vec<MemoryNode>>> {
    todo!("M3c: list memory_nodes at one level, scoped to the caller and the filter")
}

/// Remembers something new.
#[skyzen::openapi]
async fn create_memory_node(
    State(_user): State<CurrentUser>,
    Json(_request): Json<CreateMemoryNode>,
    _db: Db,
) -> Outcome<Created<Json<MemoryNode>>> {
    todo!("M3c: insert the node, refusing a parent that is not the caller's")
}

/// Reads one node of the caller's memory tree.
#[skyzen::openapi]
async fn get_memory_node(
    State(_user): State<CurrentUser>,
    _params: Params,
    _db: Db,
) -> Outcome<Json<MemoryNode>> {
    todo!("M3c: read one memory_nodes row scoped to the caller")
}

/// Edits one node of the caller's memory tree.
#[skyzen::openapi]
async fn update_memory_node(
    State(_user): State<CurrentUser>,
    _params: Params,
    Json(_request): Json<UpdateMemoryNode>,
    _db: Db,
) -> Outcome<Json<MemoryNode>> {
    todo!("M3c: apply the present fields and restamp updated_at_unix")
}

/// Forgets one node, and everything under it.
#[skyzen::openapi]
async fn delete_memory_node(
    State(_user): State<CurrentUser>,
    _params: Params,
    _db: Db,
) -> Outcome<Response> {
    todo!("M3c: delete the subtree, so no node is left pointing at a parent that is gone")
}

/// The user-scoped tree-memory routes.
pub fn routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/memory".at(list_memory).post(create_memory_node),
        "/v1/memory/{id}"
            .at(get_memory_node)
            .patch(update_memory_node)
            .delete(delete_memory_node),
    ))
    .into_route_nodes()
}
