//! The shared `AGENTS.md`, and the user's own edit path to it.
//!
//! An agent may only *request* a change — the file is root-owned on every
//! machine and the `agentsmd_change_request` MCP tool turns a request into an
//! ordinary approval — so these two routes are the only ones that write, and
//! they are the user's.

use flyco_core::{AgentsDocument, CurrentUser, UpdateAgentsDocument};
use skyzen::routing::{CreateRouteNode, Route, RouteNode, Routes as _};
use skyzen::utils::{Json, State};
use skyzen_services::Db;

use crate::problem::Outcome;

/// Reads the caller's shared `AGENTS.md`.
///
/// A user who has never written one has an empty document rather than a
/// missing resource: the file exists on every machine either way.
#[skyzen::openapi]
async fn get_agents_md(State(_user): State<CurrentUser>, _db: Db) -> Outcome<Json<AgentsDocument>> {
    todo!("M3c: read agents_md for the caller, answering an empty document when there is no row")
}

/// Replaces the caller's shared `AGENTS.md`.
#[skyzen::openapi]
async fn put_agents_md(
    State(_user): State<CurrentUser>,
    Json(_update): Json<UpdateAgentsDocument>,
    _db: Db,
) -> Outcome<Json<AgentsDocument>> {
    todo!("M3c: upsert agents_md and re-render managed policy on every live session")
}

/// The user-scoped routes of the shared `AGENTS.md`.
pub fn routes() -> Vec<RouteNode> {
    Route::new(("/v1/agents-md".at(get_agents_md).put(put_agents_md),)).into_route_nodes()
}
