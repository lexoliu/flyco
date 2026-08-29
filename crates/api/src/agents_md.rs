//! The shared `AGENTS.md`, and the user's own edit path to it.
//!
//! An agent may only *request* a change — the file is root-owned on every
//! machine and the `agentsmd_change_request` MCP tool turns a request into an
//! ordinary approval — so these two routes are the only ones that write, and
//! they are the user's.
//!
//! Exactly one document per user, which is why `agents_md.user_id` is the
//! primary key: the caller's identity is the whole `WHERE` clause, and there
//! is no id a caller could name to reach somebody else's.
//!
//! Installing the document as managed policy on a machine is provisioning's
//! job, and provisioning is M4. Storing it is not a half-measure: the
//! document is what a machine is built from, and no session holds a machine
//! in this build.

use flyco_core::{AgentsDocument, CurrentUser, UpdateAgentsDocument, UserId};
use skyzen::routing::{CreateRouteNode, Route, RouteNode, Routes as _};
use skyzen::utils::{Json, State};
use skyzen_services::Db;

use crate::clock::now_unix;
use crate::error::ApiError;
use crate::problem::Outcome;
use crate::sql::{from_column, to_column};

/// The columns the document is stored in.
#[derive(Debug, skyzen::FromRow)]
struct DocumentRow {
    content: String,
    updated_at_unix: i64,
}

impl TryFrom<DocumentRow> for AgentsDocument {
    type Error = ApiError;

    fn try_from(row: DocumentRow) -> Result<Self, Self::Error> {
        Ok(Self {
            content: row.content,
            updated_at_unix: from_column(row.updated_at_unix, "agents_md.updated_at_unix")?,
        })
    }
}

/// Reads the caller's shared `AGENTS.md`.
///
/// A user who has never written one has an empty document rather than a
/// missing resource: the file exists on every machine either way.
#[skyzen::openapi]
async fn get_agents_md(State(user): State<CurrentUser>, db: Db) -> Outcome<Json<AgentsDocument>> {
    read(&db, user.id).await.map(Json).into()
}

/// Replaces the caller's shared `AGENTS.md`.
#[skyzen::openapi]
async fn put_agents_md(
    State(user): State<CurrentUser>,
    Json(update): Json<UpdateAgentsDocument>,
    db: Db,
) -> Outcome<Json<AgentsDocument>> {
    write(&db, user.id, update.content).await.map(Json).into()
}

/// The caller's document, or an empty one if they have never written it.
async fn read(db: &Db, user: UserId) -> Result<AgentsDocument, ApiError> {
    let row: Option<DocumentRow> = db
        .query("SELECT content, updated_at_unix FROM agents_md WHERE user_id = ?")
        .bind(user.to_string())
        .fetch_optional()
        .await?;

    row.map_or_else(
        || {
            Ok(AgentsDocument {
                content: String::new(),
                updated_at_unix: 0,
            })
        },
        TryInto::try_into,
    )
}

/// Replaces the document, stamping the time the control plane recorded it.
async fn write(db: &Db, user: UserId, content: String) -> Result<AgentsDocument, ApiError> {
    let row: DocumentRow = db
        .query(
            "INSERT INTO agents_md (user_id, content, updated_at_unix) VALUES (?, ?, ?) \
             ON CONFLICT (user_id) DO UPDATE SET \
             content = excluded.content, updated_at_unix = excluded.updated_at_unix \
             RETURNING content, updated_at_unix",
        )
        .bind(user.to_string())
        .bind(content)
        .bind(to_column(now_unix()))
        .fetch_one()
        .await?;

    tracing::info!(bytes = row.content.len(), "replaced the shared AGENTS.md");
    row.try_into()
}

/// The user-scoped routes of the shared `AGENTS.md`.
pub fn routes() -> Vec<RouteNode> {
    Route::new(("/v1/agents-md".at(get_agents_md).put(put_agents_md),)).into_route_nodes()
}
