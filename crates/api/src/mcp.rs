//! The MCP registry: the servers flyco hands to every session.
//!
//! Agents may not configure MCP for themselves. The harness's MCP
//! configuration is root-owned on the machine and the allowlist is enforced
//! there, so this registry is the only way a server reaches a session, and it
//! belongs to the user.

use flyco_core::{
    CurrentUser, McpServerConfig, McpServerId, McpServerView, UpsertMcpServer, UserId,
};
use serde::Deserialize;
use skyzen::Response;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::utils::{Json, State};
use skyzen_services::Db;

use crate::clock::now_unix;
use crate::error::ApiError;
use crate::extract::path_id;
use crate::problem::Outcome;
use crate::respond::{Created, no_content};
use crate::sql::{from_column, to_column};

/// The columns every read on this path projects.
#[derive(Debug, Deserialize)]
struct McpRow {
    id: String,
    name: String,
    config: String,
    enabled: i64,
    updated_at_unix: i64,
}

impl TryFrom<McpRow> for McpServerView {
    type Error = ApiError;

    fn try_from(row: McpRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row
                .id
                .parse()
                .map_err(|_| ApiError::CorruptRecord("mcp_servers.id is not a UUID"))?,
            name: row.name,
            config: serde_json::from_str::<McpServerConfig>(&row.config)
                .map_err(|_| ApiError::CorruptRecord("mcp_servers.config is not a config"))?,
            enabled: row.enabled != 0,
            updated_at_unix: from_column(row.updated_at_unix, "mcp_servers.updated_at_unix")?,
        })
    }
}

/// Every column the projection needs, so the readers cannot drift apart.
const MCP_COLUMNS: &str = "id, name, config, enabled, updated_at_unix";

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

/// Lists the caller's registered MCP servers.
#[skyzen::openapi]
async fn list_mcp_servers(
    State(user): State<CurrentUser>,
    db: Db,
) -> Outcome<Json<Vec<McpServerView>>> {
    list(&db, user.id).await.map(Json).into()
}

async fn list(db: &Db, user: UserId) -> Result<Vec<McpServerView>, ApiError> {
    let sql = format!("SELECT {MCP_COLUMNS} FROM mcp_servers WHERE user_id = ? ORDER BY name");
    let rows: Vec<McpRow> = db.query(&sql).bind(user.to_string()).fetch_all().await?;
    rows.into_iter().map(TryInto::try_into).collect()
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
async fn register(
    db: &Db,
    user: UserId,
    request: UpsertMcpServer,
) -> Result<McpServerView, ApiError> {
    let name = checked_name(&request.name)?;
    let config = serde_json::to_string(&request.config)
        .map_err(|_| ApiError::CorruptRecord("the server config could not be encoded"))?;
    let sql = format!(
        "INSERT INTO mcp_servers (id, user_id, name, config, enabled, updated_at_unix) \
         VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (user_id, name) DO NOTHING \
         RETURNING {MCP_COLUMNS}"
    );

    let row: Option<McpRow> = db
        .query(&sql)
        .bind(McpServerId::generate().to_string())
        .bind(user.to_string())
        .bind(name.clone())
        .bind(config)
        .bind(to_column(u64::from(request.enabled)))
        .bind(to_column(now_unix()))
        .fetch_optional()
        .await?;

    row.ok_or(ApiError::McpServerNameTaken { name })?.try_into()
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
    let sql = format!("SELECT {MCP_COLUMNS} FROM mcp_servers WHERE id = ? AND user_id = ?");
    let row: Option<McpRow> = db
        .query(&sql)
        .bind(id.to_string())
        .bind(user.to_string())
        .fetch_optional()
        .await?;

    row.ok_or(ApiError::McpServerNotFound)?.try_into()
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

    let sql = format!(
        "UPDATE mcp_servers SET name = ?, config = ?, enabled = ?, updated_at_unix = ? \
         WHERE id = ? AND user_id = ? RETURNING {MCP_COLUMNS}"
    );
    let row: Option<McpRow> = db
        .query(&sql)
        .bind(name)
        .bind(config)
        .bind(to_column(u64::from(request.enabled)))
        .bind(to_column(now_unix()))
        .bind(id.to_string())
        .bind(user.to_string())
        .fetch_optional()
        .await?;

    row.ok_or(ApiError::McpServerNotFound)?.try_into()
}

/// Removes one of the caller's MCP servers.
#[skyzen::openapi]
async fn delete_mcp_server(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Outcome<Response> {
    remove(&db, user.id, &params).await.into()
}

async fn remove(db: &Db, user: UserId, params: &Params) -> Result<Response, ApiError> {
    let id: McpServerId = path_id(params, "id")?;
    let removed = db
        .query("DELETE FROM mcp_servers WHERE id = ? AND user_id = ?")
        .bind(id.to_string())
        .bind(user.to_string())
        .execute()
        .await?;

    if removed.rows_written == 0 {
        return Err(ApiError::McpServerNotFound);
    }

    Ok(no_content())
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
