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

use flyco_core::{AuthorizeUrl, CurrentUser, HarnessAccountView, LlmUsageView};
use serde::Deserialize;
use skyzen::Response;
use skyzen::extract::Query;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::utils::{Json, State};
use skyzen_services::{Db, Kv};

use crate::problem::Outcome;

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
    State(_user): State<CurrentUser>,
    _db: Db,
) -> Outcome<Json<Vec<HarnessAccountView>>> {
    todo!("M4: list harness_accounts for the caller, without unsealing any token")
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
) -> Outcome<Response> {
    todo!(
        "M4: consume the state, exchange the code with the vendor, seal the token, 303 to the SPA"
    )
}

/// Unlinks the caller's account for one harness.
#[skyzen::openapi]
async fn unlink_harness_account(
    State(_user): State<CurrentUser>,
    _params: Params,
    _db: Db,
) -> Outcome<Response> {
    todo!("M4: delete the harness account and stop provisioning it onto new machines")
}

/// Reports what flyco has observed of each harness account's usage.
///
/// Reactive by necessity: neither Anthropic nor `OpenAI` publishes a
/// remaining-quota API, so this reports the cost telemetry the harness
/// emitted and the rate limits it actually hit. A panel built on it says
/// what has happened, never what is left.
#[skyzen::openapi]
async fn llm_usage(State(_user): State<CurrentUser>, _db: Db) -> Outcome<Json<Vec<LlmUsageView>>> {
    todo!("M6: aggregate observed rate-limit events and OTLP cost telemetry per account")
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
