//! Tree memory: the memory system flyco serves to agents over MCP, in place
//! of the harnesses' file-based ones.
//!
//! A node is scoped to a repository or shared across all of them, and recall
//! is a walk down the tree — list the children of a parent, read one node —
//! rather than a grep over a directory of Markdown. The agent reaches the
//! same store through flycod's `memory_*` MCP tools; these routes are what
//! the user's own UI reads and writes.
//!
//! Every statement below carries `user_id` in its `WHERE` clause, including
//! the ones that already have a primary key to go on: a node belonging to
//! somebody else is indistinguishable from one that does not exist, and the
//! scope is the query rather than a check beside it that a later edit could
//! forget.

use flyco_core::{
    CreateMemoryNode, CurrentUser, MemoryNode, MemoryNodeId, RepoSlug, UpdateMemoryNode, UserId,
};
use serde::{Deserialize, Serialize};
use skyzen::extract::Query;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::utils::{Json, State};
use skyzen_services::Db;

use crate::clock::now_unix;
use crate::error::ApiError;
use crate::extract::path_id;
use crate::problem::Outcome;
use crate::respond::{Created, NoContent};

/// Which level of the tree to list.
#[derive(Debug, Default, Deserialize, Serialize, skyzen::ToSchema)]
pub struct MemoryFilter {
    /// Only nodes about this repository, `owner/name`. Omitted lists the
    /// memory that applies wherever the caller's agents run.
    pub repo: Option<String>,
    /// List this node's children. Omitted lists the roots.
    pub parent: Option<MemoryNodeId>,
}

/// The columns every read on this path projects.
#[derive(Debug, skyzen::FromRow)]
struct MemoryRow {
    id: MemoryNodeId,
    parent_id: Option<MemoryNodeId>,
    repo: Option<RepoSlug>,
    title: String,
    content: String,
    updated_at_unix: u64,
}

impl From<MemoryRow> for MemoryNode {
    fn from(row: MemoryRow) -> Self {
        Self {
            id: row.id,
            parent: row.parent_id,
            repo: row.repo,
            title: row.title,
            content: row.content,
            updated_at_unix: row.updated_at_unix,
        }
    }
}

/// Lists one level of the caller's memory tree.
#[skyzen::openapi]
async fn list_memory(
    State(user): State<CurrentUser>,
    Query(filter): Query<MemoryFilter>,
    db: Db,
) -> Outcome<Json<Vec<MemoryNode>>> {
    list(&db, user.id, &filter).await.map(Json).into()
}

/// Remembers something new.
#[skyzen::openapi]
async fn create_memory_node(
    State(user): State<CurrentUser>,
    Json(request): Json<CreateMemoryNode>,
    db: Db,
) -> Outcome<Created<Json<MemoryNode>>> {
    create(&db, user.id, request)
        .await
        .map(|node| Created(Json(node)))
        .into()
}

/// Reads one node of the caller's memory tree.
#[skyzen::openapi]
async fn get_memory_node(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Outcome<Json<MemoryNode>> {
    read(&db, user.id, &params).await.map(Json).into()
}

/// Edits one node of the caller's memory tree.
#[skyzen::openapi]
async fn update_memory_node(
    State(user): State<CurrentUser>,
    params: Params,
    Json(request): Json<UpdateMemoryNode>,
    db: Db,
) -> Outcome<Json<MemoryNode>> {
    update(&db, user.id, &params, request)
        .await
        .map(Json)
        .into()
}

/// Forgets one node, and everything under it.
#[skyzen::openapi]
async fn delete_memory_node(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Outcome<NoContent> {
    forget(&db, user.id, &params).await.into()
}

/// Lists the nodes at one level of the tree.
///
/// Both halves of the filter are exact rather than "contains": an omitted
/// `parent` means the roots, and an omitted `repo` means the memory that is
/// not about any one repository. A listing that folded the two together
/// would answer a question — "everything, flat" — that the tree exists to
/// avoid asking.
async fn list(db: &Db, user: UserId, filter: &MemoryFilter) -> Result<Vec<MemoryNode>, ApiError> {
    let repo = filter
        .repo
        .as_deref()
        .map(str::parse::<RepoSlug>)
        .transpose()
        .map_err(|_| ApiError::InvalidRepo(filter.repo.clone().unwrap_or_default()))?;

    let rows: Vec<MemoryRow> = db
        .query(
            "SELECT id, parent_id, repo, title, content, updated_at_unix \
             FROM memory_nodes WHERE user_id = ? \
             AND ((? IS NULL AND repo IS NULL) OR repo = ?) \
             AND ((? IS NULL AND parent_id IS NULL) OR parent_id = ?) \
             ORDER BY title, id",
        )
        .bind(user)
        .bind(repo.clone())
        .bind(repo)
        .bind(filter.parent)
        .bind(filter.parent)
        .fetch_all()
        .await?;

    Ok(rows.into_iter().map(Into::into).collect())
}

/// Inserts a node, refusing a parent the caller does not own.
///
/// The parent is checked first because D1 has no foreign-key enforcement to
/// lean on and a node hanging off a stranger's parent would be reachable
/// from their tree.
async fn create(db: &Db, user: UserId, request: CreateMemoryNode) -> Result<MemoryNode, ApiError> {
    if let Some(parent) = request.parent {
        load(db, user, parent).await?;
    }

    let id = MemoryNodeId::generate();
    db.query(
        "INSERT INTO memory_nodes \
         (id, user_id, parent_id, repo, title, content, updated_at_unix) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(user)
    .bind(request.parent)
    .bind(request.repo)
    .bind(request.title)
    .bind(request.content)
    .bind(now_unix())
    .execute()
    .await?;

    tracing::info!(node = %id, "remembered a memory node");
    load(db, user, id).await
}

async fn read(db: &Db, user: UserId, params: &Params) -> Result<MemoryNode, ApiError> {
    load(db, user, path_id::<MemoryNodeId>(params, "id")?).await
}

/// Applies the fields a patch carries and restamps the node.
///
/// A patch that names nothing is still a write: it restamps
/// `updated_at_unix`, because "the user looked at this and left it as it is"
/// is a fact the recall order should reflect.
async fn update(
    db: &Db,
    user: UserId,
    params: &Params,
    request: UpdateMemoryNode,
) -> Result<MemoryNode, ApiError> {
    let id = path_id::<MemoryNodeId>(params, "id")?;
    let current = load(db, user, id).await?;

    db.query(
        "UPDATE memory_nodes SET title = ?, content = ?, updated_at_unix = ? \
         WHERE id = ? AND user_id = ?",
    )
    .bind(request.title.unwrap_or(current.title))
    .bind(request.content.unwrap_or(current.content))
    .bind(now_unix())
    .bind(id)
    .bind(user)
    .execute()
    .await?;

    load(db, user, id).await
}

/// Deletes a node and everything under it.
///
/// One recursive statement rather than a walk in the handler: D1 has no
/// transactions, so a walk that failed halfway would leave orphans pointing
/// at a parent that is gone — and `parent_id` is how the tree is read.
async fn forget(db: &Db, user: UserId, params: &Params) -> Result<NoContent, ApiError> {
    let id = path_id::<MemoryNodeId>(params, "id")?;
    load(db, user, id).await?;

    db.query(
        "WITH RECURSIVE subtree(id) AS ( \
             SELECT id FROM memory_nodes WHERE id = ? AND user_id = ? \
             UNION ALL \
             SELECT node.id FROM memory_nodes node \
             JOIN subtree ON node.parent_id = subtree.id \
         ) \
         DELETE FROM memory_nodes WHERE id IN (SELECT id FROM subtree)",
    )
    .bind(id)
    .bind(user)
    .execute()
    .await?;

    tracing::info!(node = %id, "forgot a memory subtree");
    Ok(NoContent)
}

/// Loads one of the caller's nodes.
async fn load(db: &Db, user: UserId, id: MemoryNodeId) -> Result<MemoryNode, ApiError> {
    let row: Option<MemoryRow> = db
        .query(
            "SELECT id, parent_id, repo, title, content, updated_at_unix \
             FROM memory_nodes WHERE id = ? AND user_id = ?",
        )
        .bind(id)
        .bind(user)
        .fetch_optional()
        .await?;

    Ok(row.ok_or(ApiError::MemoryNodeNotFound)?.into())
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
