//! The MCP registry: the servers flyco hands to every session.
//!
//! Agents may not configure MCP for themselves. The harness's MCP
//! configuration is root-owned on the machine and the allowlist is enforced
//! there, so this registry is the only way a server reaches a session, and it
//! belongs to the user.

use flyco_core::{CurrentUser, McpServerView, UpsertMcpServer};
use skyzen::Response;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::utils::{Json, State};
use skyzen_services::Db;

use crate::problem::Outcome;
use crate::respond::Created;

/// Lists the caller's registered MCP servers.
#[skyzen::openapi]
async fn list_mcp_servers(
    State(_user): State<CurrentUser>,
    _db: Db,
) -> Outcome<Json<Vec<McpServerView>>> {
    todo!("M6: list mcp_servers for the caller")
}

/// Registers an MCP server.
#[skyzen::openapi]
async fn register_mcp_server(
    State(_user): State<CurrentUser>,
    Json(_request): Json<UpsertMcpServer>,
    _db: Db,
) -> Outcome<Created<Json<McpServerView>>> {
    todo!("M6: insert the server, refusing a name the caller already uses")
}

/// Describes one of the caller's MCP servers.
#[skyzen::openapi]
async fn get_mcp_server(
    State(_user): State<CurrentUser>,
    _params: Params,
    _db: Db,
) -> Outcome<Json<McpServerView>> {
    todo!("M6: read one mcp_servers row scoped to the caller")
}

/// Replaces one of the caller's MCP servers.
///
/// The body is the whole document rather than a diff: a half-applied
/// transport change is a state nobody should be able to describe.
#[skyzen::openapi]
async fn update_mcp_server(
    State(_user): State<CurrentUser>,
    _params: Params,
    Json(_request): Json<UpsertMcpServer>,
    _db: Db,
) -> Outcome<Json<McpServerView>> {
    todo!("M6: update the row and re-render managed MCP config on every live session")
}

/// Removes one of the caller's MCP servers.
#[skyzen::openapi]
async fn delete_mcp_server(
    State(_user): State<CurrentUser>,
    _params: Params,
    _db: Db,
) -> Outcome<Response> {
    todo!("M6: delete the row and withdraw the server from every live session")
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
