//! Claude and Codex accounts, and the LLM usage panel.
//!
//! Linking runs against the **vendor's own** authorization page — the flow
//! the official CLIs wrap — so flyco never handles a vendor password and
//! stores only the resulting token, sealed. That makes the callback public:
//! the browser arrives back from Anthropic or `OpenAI` carrying no flyco
//! credential, so the `state` minted at the start is what identifies the
//! user, exactly as in [`crate::oauth`].
//!
//! Flyco holds at most one account per harness, which is why the harness
//! itself names the account in a path rather than an identifier.

use flyco_core::{
    AuthorizeUrl, CurrentUser, HarnessAccountId, HarnessAccountView, HarnessKind, LlmUsageView,
    UserId,
};
use serde::Deserialize;
use skyzen::extract::Query;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::sql;
use skyzen::utils::{Json, State};
use skyzen_services::sql::ColumnEnum as _;
use skyzen_services::{Db, Kv};

use crate::error::ApiError;
use crate::extract::path_segment;
use crate::observations;
use crate::problem::Outcome;
use crate::respond::{NoContent, SeeOther};

/// The columns every read on this path projects.
///
/// `token_enc` is deliberately absent: the sealed credential is provisioned
/// onto machines and is never selected by a route that answers a browser.
#[derive(Debug, skyzen::FromRow)]
struct HarnessAccountRow {
    id: HarnessAccountId,
    harness: HarnessKind,
    label: String,
    linked_at_unix: u64,
    expires_at_unix: Option<u64>,
}

impl From<HarnessAccountRow> for HarnessAccountView {
    fn from(row: HarnessAccountRow) -> Self {
        Self {
            id: row.id,
            harness: row.harness,
            label: row.label,
            linked_at_unix: row.linked_at_unix,
            expires_at_unix: row.expires_at_unix,
        }
    }
}

/// Reads the `{harness}` path segment as the harness it names.
///
/// The segment and the `harness` column speak the same tokens, because both
/// are [`ColumnEnum::from_token`] — a path a caller can type cannot name a
/// harness the table could not hold.
fn harness_of(params: &Params) -> Result<HarnessKind, ApiError> {
    let segment = path_segment(params, "harness")?;
    HarnessKind::from_token(&segment).ok_or(ApiError::MalformedId(segment))
}

/// Query string the vendor appends when it redirects back.
#[derive(Debug, Deserialize, skyzen::ToSchema)]
pub struct LinkCallback {
    /// The single-use authorization code.
    pub code: String,
    /// The `state` this control plane minted when the link began.
    pub state: String,
}

/// Lists the caller's linked harness accounts.
#[skyzen::openapi]
async fn list_harness_accounts(
    State(user): State<CurrentUser>,
    db: Db,
) -> Outcome<Json<Vec<HarnessAccountView>>> {
    list(&db, user.id).await.map(Json).into()
}

async fn list(db: &Db, user: UserId) -> Result<Vec<HarnessAccountView>, ApiError> {
    let rows: Vec<HarnessAccountRow> = sql!(
        db,
        "SELECT id, harness, label, linked_at_unix, expires_at_unix \
         FROM harness_accounts WHERE user_id = {user} ORDER BY harness"
    )
    .fetch_all()
    .await?;

    Ok(rows.into_iter().map(Into::into).collect())
}

/// Begins linking a harness account, returning the vendor's authorize URL.
#[skyzen::openapi]
async fn start_harness_link(
    State(_user): State<CurrentUser>,
    _params: Params,
    _kv: Kv,
) -> Outcome<Json<AuthorizeUrl>> {
    todo!("M4: mint a single-use state in KV and build the vendor's own authorize URL")
}

/// Completes a harness link and returns the browser to the SPA.
///
/// Public, because the browser arrives from the vendor with no flyco
/// credential; the single-use `state` minted by the start call is what says
/// whose account this is.
#[skyzen::openapi]
async fn complete_harness_link(
    _params: Params,
    Query(_callback): Query<LinkCallback>,
    _kv: Kv,
    _db: Db,
) -> Outcome<SeeOther> {
    todo!(
        "M4: consume the state, exchange the code with the vendor, seal the token, 303 to the SPA"
    )
}

/// Unlinks the caller's account for one harness.
#[skyzen::openapi]
async fn unlink_harness_account(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Outcome<NoContent> {
    unlink(&db, user.id, &params).await.into()
}

/// Unlinking stops the credential reaching machines built from here on.
///
/// It does not reach into machines already running: their environment was
/// fixed when the process started, and a session mid-turn keeps the
/// credential it was given until it is archived.
async fn unlink(db: &Db, user: UserId, params: &Params) -> Result<NoContent, ApiError> {
    let harness = harness_of(params)?;

    let removed = sql!(
        db,
        "DELETE FROM harness_accounts WHERE user_id = {user} AND harness = {harness}"
    )
    .execute()
    .await?;

    if removed.rows_written == 0 {
        return Err(ApiError::HarnessAccountNotFound);
    }

    tracing::info!(?harness, "unlinked a harness account");
    Ok(NoContent)
}

/// Reports what flyco has observed of each harness account's usage.
///
/// Reactive by necessity: neither Anthropic nor `OpenAI` publishes a
/// remaining-quota API, so this reports the cost telemetry the harness
/// emitted and the rate limits it actually hit. A panel built on it says
/// what has happened, never what is left.
///
/// The rows come out of [`crate::observations`], which is filled in by the
/// sessions' own daemons as they run — the only place either number exists.
/// An account with nothing observed about it still appears, reporting
/// nothing, because "nothing has happened" is an answer and a missing row
/// would read as an account that is not linked.
#[skyzen::openapi]
async fn llm_usage(State(user): State<CurrentUser>, db: Db) -> Outcome<Json<Vec<LlmUsageView>>> {
    observations::usage(&db, user.id).await.map(Json).into()
}

/// The user-scoped harness-account routes.
pub fn routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/harness-accounts".at(list_harness_accounts),
        "/v1/harness-accounts/{harness}".delete(unlink_harness_account),
        "/v1/harness-accounts/{harness}/link/start".post(start_harness_link),
        "/v1/usage/llm".at(llm_usage),
    ))
    .into_route_nodes()
}

/// The link callback, which carries no flyco credential.
pub fn public_routes() -> Vec<RouteNode> {
    Route::new(("/v1/harness-accounts/{harness}/link/callback".at(complete_harness_link),))
        .into_route_nodes()
}
