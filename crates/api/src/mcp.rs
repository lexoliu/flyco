//! The MCP registry: the servers flyco hands to every session.
//!
//! Agents may not configure MCP for themselves. The harness's MCP
//! configuration is root-owned on the machine and the allowlist is enforced
//! there, so this registry is the only way a server reaches a session, and it
//! belongs to the user.

use flyco_core::{
    CurrentUser, McpServerConfig, McpServerId, McpServerMount, McpServerView, UpsertMcpServer,
    UserId,
};
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::sql;
use skyzen::utils::{Json, State};
use skyzen_services::Db;

use crate::clock::now_unix;
use crate::error::ApiError;
use crate::extract::path_id;
use crate::problem::Outcome;
use crate::respond::{Created, NoContent};

/// The columns every read on this path projects.
#[derive(Debug, skyzen::FromRow)]
struct McpRow {
    id: McpServerId,
    name: String,
    /// The transport definition, kept as a JSON document in a text column.
    #[row(json)]
    config: McpServerConfig,
    enabled: bool,
    updated_at_unix: u64,
}

/// The columns a provisioned mount projects.
///
/// Deliberately not [`McpRow`]: what a machine is handed is the name and
/// the transport, and an identifier or a timestamp on a session VM would be
/// registry bookkeeping the daemon has no use for.
#[derive(Debug, skyzen::FromRow)]
struct McpMountRow {
    name: String,
    #[row(json)]
    config: McpServerConfig,
}

impl From<McpMountRow> for McpServerMount {
    fn from(row: McpMountRow) -> Self {
        Self {
            name: row.name,
            config: row.config,
        }
    }
}

impl From<McpRow> for McpServerView {
    fn from(row: McpRow) -> Self {
        Self {
            id: row.id,
            name: row.name,
            config: row.config,
            enabled: row.enabled,
            updated_at_unix: row.updated_at_unix,
        }
    }
}

/// Rejects a name no harness could announce the server under.
fn checked_name(name: &str) -> Result<String, ApiError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(ApiError::InvalidMcpServer("a name is required"));
    }
    // The name becomes part of the tool identifier the harness exposes
    // (`mcp__<server>__<tool>`), so anything outside this set would produce
    // tools the model cannot address.
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ApiError::InvalidMcpServer(
            "a name may hold only letters, digits, dashes and underscores",
        ));
    }
    Ok(trimmed.to_owned())
}

/// The servers a session machine is provisioned with.
///
/// Only the enabled ones, and only the two fields a harness configuration
/// needs. This is the *whole* set a session gets: the machine's harness
/// config is root-owned and its MCP allowlist is enforced there, so an
/// agent cannot reach a server this query did not return, and one the user
/// disabled is not one it can turn back on.
pub(crate) async fn mounts(db: &Db, user: UserId) -> Result<Vec<McpServerMount>, ApiError> {
    let rows: Vec<McpMountRow> = sql!(
        db,
        "SELECT name, config FROM mcp_servers \
         WHERE user_id = {user} AND enabled = 1 ORDER BY name"
    )
    .fetch_all()
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Lists the caller's registered MCP servers.
#[skyzen::openapi]
async fn list_mcp_servers(
    State(user): State<CurrentUser>,
    db: Db,
) -> Outcome<Json<Vec<McpServerView>>> {
    list(&db, user.id).await.map(Json).into()
}

async fn list(db: &Db, user: UserId) -> Result<Vec<McpServerView>, ApiError> {
    let rows: Vec<McpRow> = sql!(
        db,
        "SELECT id, name, config, enabled, updated_at_unix \
         FROM mcp_servers WHERE user_id = {user} ORDER BY name"
    )
    .fetch_all()
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Registers an MCP server.
#[skyzen::openapi]
async fn register_mcp_server(
    State(user): State<CurrentUser>,
    Json(request): Json<UpsertMcpServer>,
    db: Db,
) -> Outcome<Created<Json<McpServerView>>> {
    register(&db, user.id, request)
        .await
        .map(|view| Created(Json(view)))
        .into()
}

/// Registers a server, refusing a name the caller already uses.
///
/// The name is what the harness announces the server under, so two servers
/// answering to one name is a collision on the machine rather than a
/// cosmetic duplicate. The unique index is the arbiter: a conflicting insert
/// writes no row, which is what turns into the 409.
pub(crate) async fn register(
    db: &Db,
    user: UserId,
    request: UpsertMcpServer,
) -> Result<McpServerView, ApiError> {
    let name = checked_name(&request.name)?;
    let config = serde_json::to_string(&request.config)
        .map_err(|_| ApiError::CorruptRecord("the server config could not be encoded"))?;
    let row: Option<McpRow> = sql!(
        db,
        "INSERT INTO mcp_servers (id, user_id, name, config, enabled, updated_at_unix) \
         VALUES ({McpServerId::generate()}, {user}, {name.clone()}, {config}, \
                 {request.enabled}, {now_unix()}) \
         ON CONFLICT (user_id, name) DO NOTHING \
         RETURNING id, name, config, enabled, updated_at_unix"
    )
    .fetch_optional()
    .await?;

    Ok(row.ok_or(ApiError::McpServerNameTaken { name })?.into())
}

/// Describes one of the caller's MCP servers.
#[skyzen::openapi]
async fn get_mcp_server(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Outcome<Json<McpServerView>> {
    read(&db, user.id, &params).await.map(Json).into()
}

async fn read(db: &Db, user: UserId, params: &Params) -> Result<McpServerView, ApiError> {
    let id: McpServerId = path_id(params, "id")?;
    let row: Option<McpRow> = sql!(
        db,
        "SELECT id, name, config, enabled, updated_at_unix \
         FROM mcp_servers WHERE id = {id} AND user_id = {user}"
    )
    .fetch_optional()
    .await?;

    Ok(row.ok_or(ApiError::McpServerNotFound)?.into())
}

/// Replaces one of the caller's MCP servers.
///
/// The body is the whole document rather than a diff: a half-applied
/// transport change is a state nobody should be able to describe.
#[skyzen::openapi]
async fn update_mcp_server(
    State(user): State<CurrentUser>,
    params: Params,
    Json(request): Json<UpsertMcpServer>,
    db: Db,
) -> Outcome<Json<McpServerView>> {
    update(&db, user.id, &params, request)
        .await
        .map(Json)
        .into()
}

/// Replaces a server definition.
///
/// A live session picks the change up when its machine next renders managed
/// MCP configuration; nothing is pushed into a running harness, because a
/// server list is read when the process starts and a mid-turn swap would
/// leave the model holding tools that no longer exist.
async fn update(
    db: &Db,
    user: UserId,
    params: &Params,
    request: UpsertMcpServer,
) -> Result<McpServerView, ApiError> {
    let id: McpServerId = path_id(params, "id")?;
    let name = checked_name(&request.name)?;
    let config = serde_json::to_string(&request.config)
        .map_err(|_| ApiError::CorruptRecord("the server config could not be encoded"))?;

    let row: Option<McpRow> = sql!(
        db,
        "UPDATE mcp_servers SET name = {name}, config = {config}, \
         enabled = {request.enabled}, updated_at_unix = {now_unix()} \
         WHERE id = {id} AND user_id = {user} \
         RETURNING id, name, config, enabled, updated_at_unix"
    )
    .fetch_optional()
    .await?;

    Ok(row.ok_or(ApiError::McpServerNotFound)?.into())
}

/// Removes one of the caller's MCP servers.
#[skyzen::openapi]
async fn delete_mcp_server(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Outcome<NoContent> {
    remove(&db, user.id, &params).await.into()
}

async fn remove(db: &Db, user: UserId, params: &Params) -> Result<NoContent, ApiError> {
    let id: McpServerId = path_id(params, "id")?;
    let removed = sql!(
        db,
        "DELETE FROM mcp_servers WHERE id = {id} AND user_id = {user}"
    )
    .execute()
    .await?;

    if removed.rows_written == 0 {
        return Err(ApiError::McpServerNotFound);
    }

    Ok(NoContent)
}

/// The user-scoped MCP registry routes.
pub fn routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/mcp-servers"
            .at(list_mcp_servers)
            .post(register_mcp_server),
        "/v1/mcp-servers/{id}"
            .at(get_mcp_server)
            .patch(update_mcp_server)
            .delete(delete_mcp_server),
    ))
    .into_route_nodes()
}
