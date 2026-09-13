//! `flyco login`'s browser-approval handshake.
//!
//! A CLI on a headless or SSH'd box cannot open a browser itself and cannot
//! receive a localhost redirect, so the secret never travels with the
//! redirect: `POST /v1/cli-sessions` creates an attempt carrying an
//! unguessable `poll_token`, the CLI prints an `authorize_url`, and the
//! user approves it on the PWA's `/cli/authorize` page under their existing
//! web session. Approval mints a single-use `fk_` key that the poll
//! endpoint (`GET /v1/cli-sessions/{id}?s=<poll_token>`) returns once — to
//! the holder of the poll token, never through a URL, browser history, or
//! clipboard.
//!
//! The record lives in expiring KV: ten minutes is enough for a user to
//! open a link and click approve, and means an abandoned attempt cleans
//! itself up.

use flyco_core::{
    ApiKeyId, CliSession, CliSessionId, CliSessionKey, CreateCliSession, CurrentUser,
};
use serde::{Deserialize, Serialize};
use skyzen::Responder;
use skyzen::extract::Query;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes};
use skyzen::utils::{Json, State};
use skyzen_services::{Db, Kv};

use crate::clock::now_unix;
use crate::config::ApiConfig;
use crate::crypto::{prefixed_token, token_hash};
use crate::error::ApiError;
use crate::expiring;
use crate::extract::path_id;
use crate::problem::Outcome;
use crate::respond::{Accepted, Created, NoContent};

/// How long an unanswered attempt stays approvable, in seconds.
const TTL_SECONDS: u64 = 10 * 60;

/// The prefix on a poll token — recognizable as flyco-issued, and not the
/// `fk_` shape the authenticator would try to resolve as an API key.
const POLL_TOKEN_PREFIX: &str = "fc_";

/// What the approval page decided, stored so the poll can answer it.
#[derive(Debug, Serialize, Deserialize)]
enum Decision {
    /// The user approved: the key waits here for the poller to collect it.
    Approved {
        /// Revocation handle and the plaintext key, delivered read-once.
        key_id: ApiKeyId,
        key: String,
    },
    /// The user refused.
    Denied,
}

/// The KV record behind an attempt.
#[derive(Debug, Serialize, Deserialize)]
struct Attempt {
    /// SHA-256 of the poll token — the only form a credential is stored in.
    poll: String,
    /// What to call the minted key — `flyco-cli on <hostname>`.
    hostname: Option<String>,
    /// The user's answer, once they give one.
    decision: Option<Decision>,
}

fn key_of(id: CliSessionId) -> String {
    format!("cli-session:{id}")
}

/// `POST /v1/cli-sessions` — opens a sign-in attempt.
///
/// Public by necessity: the CLI calling it holds no credential yet — that
/// is what the attempt is for. What it returns is a capability anyone may
/// mint, which is safe because it authorizes nothing: the poll token only
/// collects a key the user chose to approve, and the attempt id only names
/// a page that asks the signed-in user to decide.
#[skyzen::openapi]
async fn create_cli_session(
    State(config): State<ApiConfig>,
    Json(request): Json<CreateCliSession>,
    kv: Kv,
) -> Outcome<Created<Json<CliSession>>> {
    open(&config, request, &kv).await.into()
}

async fn open(
    config: &ApiConfig,
    request: CreateCliSession,
    kv: &Kv,
) -> Result<Created<Json<CliSession>>, ApiError> {
    let id = CliSessionId::generate();
    let poll_token = prefixed_token(POLL_TOKEN_PREFIX)?;
    let attempt = Attempt {
        poll: token_hash(&poll_token),
        hostname: request.hostname,
        decision: None,
    };
    expiring::put(kv, &key_of(id), &attempt, TTL_SECONDS).await?;

    let authorize_url = format!(
        "{}/cli/authorize?id={id}",
        config.redirect_uri().origin().ascii_serialization()
    );
    Ok(Created(Json(CliSession {
        id,
        poll_token,
        authorize_url,
        expires_at_unix: now_unix() + TTL_SECONDS,
    })))
}

/// Query the poll presents: `?s=<poll_token>`.
#[derive(Debug, Deserialize, skyzen::ToSchema)]
struct Poll {
    /// The `poll_token` `POST /v1/cli-sessions` issued.
    s: Option<String>,
}

/// The two ways a poll can answer `200`-or-`202`.
enum PollAnswer {
    /// Still waiting on the user — `202 Accepted`, no body.
    Pending,
    /// The key, delivered once — `200 OK` with the [`CliSessionKey`] body.
    Granted(CliSessionKey),
}

impl Responder for PollAnswer {
    type Error = skyzen::utils::json::JsonEncodingError;

    fn respond_to(
        self,
        request: &skyzen::Request,
        response: &mut skyzen::Response,
    ) -> Result<(), Self::Error> {
        match self {
            Self::Pending => Accepted
                .respond_to(request, response)
                .map_err(|never| match never {}),
            Self::Granted(key) => Json(key).respond_to(request, response),
        }
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<Vec<skyzen::openapi::ResponseSchema>> {
        let mut schemas = Json::<CliSessionKey>::openapi().unwrap_or_default();
        schemas.extend(Accepted::openapi().unwrap_or_default());
        Some(schemas)
    }

    #[cfg(feature = "openapi")]
    fn register_openapi_schemas(
        defs: &mut std::collections::BTreeMap<String, skyzen::openapi::SchemaRef>,
    ) {
        Json::<CliSessionKey>::register_openapi_schemas(defs);
    }
}

/// `GET /v1/cli-sessions/{id}?s=<poll_token>` — the CLI's poll.
///
/// `202` while the user has not decided; `200` with the key once they
/// approve — the read that delivers it consumes the record, so a repeated
/// poll is `410` rather than a second copy. Public because the poller is by
/// definition unauthenticated; the poll token is the credential.
#[skyzen::openapi]
async fn poll_cli_session(
    params: Params,
    Query(query): Query<Poll>,
    kv: Kv,
) -> Outcome<PollAnswer> {
    poll(&params, query, &kv).await.into()
}

async fn poll(params: &Params, query: Poll, kv: &Kv) -> Result<PollAnswer, ApiError> {
    let id = path_id::<CliSessionId>(params, "id")?;
    let Some(attempt) = expiring::get::<Attempt>(kv, &key_of(id)).await? else {
        return Err(ApiError::CliSessionGone);
    };
    let Some(presented) = query.s else {
        return Err(ApiError::CliSessionPollDenied);
    };
    if token_hash(&presented) != attempt.poll {
        return Err(ApiError::CliSessionPollDenied);
    }

    match attempt.decision {
        Some(Decision::Approved { .. }) => {
            // Read-once: collect the record before answering, so no second
            // poll can read the key back. A racing poll that collected
            // first finds the record gone — `410`, same as an expired
            // attempt.
            match expiring::take::<Attempt>(kv, &key_of(id)).await? {
                Some(Attempt {
                    decision: Some(Decision::Approved { key_id, key }),
                    ..
                }) => Ok(PollAnswer::Granted(CliSessionKey { key_id, key })),
                _ => Err(ApiError::CliSessionGone),
            }
        }
        Some(Decision::Denied) => Err(ApiError::CliSessionDenied),
        None => Ok(PollAnswer::Pending),
    }
}

/// `POST /v1/cli-sessions/{id}/approve` — the PWA's "approve" button.
///
/// Authenticated like every other user route; the minted key belongs to
/// whoever is signed in — a key for somebody else's account would hand the
/// CLI the wrong identity, so there is nothing to name but the attempt id.
#[skyzen::openapi]
async fn approve_cli_session(
    State(user): State<CurrentUser>,
    params: Params,
    kv: Kv,
    db: Db,
) -> Outcome<NoContent> {
    decide(&user, &params, true, &kv, &db).await.into()
}

/// `POST /v1/cli-sessions/{id}/deny` — the PWA's "deny" button.
///
/// Recorded rather than just closed: the CLI's poll is answered `403`
/// instead of running the attempt's ten minutes out.
#[skyzen::openapi]
async fn deny_cli_session(
    State(user): State<CurrentUser>,
    params: Params,
    kv: Kv,
    db: Db,
) -> Outcome<NoContent> {
    decide(&user, &params, false, &kv, &db).await.into()
}

async fn decide(
    user: &CurrentUser,
    params: &Params,
    approved: bool,
    kv: &Kv,
    db: &Db,
) -> Result<NoContent, ApiError> {
    let id = path_id::<CliSessionId>(params, "id")?;
    let Some(mut attempt) = expiring::get::<Attempt>(kv, &key_of(id)).await? else {
        return Err(ApiError::CliSessionGone);
    };
    if let Some(decision) = &attempt.decision {
        return Err(ApiError::CliSessionAlreadyDecided {
            state: match decision {
                Decision::Approved { .. } => "approved",
                Decision::Denied => "denied",
            },
        });
    }

    attempt.decision = if approved {
        let hostname = attempt.hostname.as_deref().unwrap_or("unknown host");
        let key = crate::api_keys::create(db, user.id, format!("flyco-cli on {hostname}")).await?;
        Some(Decision::Approved {
            key_id: key.id,
            key: key.token,
        })
    } else {
        Some(Decision::Denied)
    };
    // Re-written rather than patched, at a fresh TTL: approval at minute
    // nine still leaves the poller a full window to collect the key.
    expiring::put(kv, &key_of(id), &attempt, TTL_SECONDS).await?;
    Ok(NoContent)
}

/// The routes that must sit outside [`RequireAuth`](crate::middleware::RequireAuth).
pub fn public_routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/cli-sessions".post(create_cli_session),
        "/v1/cli-sessions/{id}".at(poll_cli_session),
    ))
    .into_route_nodes()
}

/// The decision routes, under the same auth as the rest of the account API.
pub fn routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/cli-sessions/{id}/approve".post(approve_cli_session),
        "/v1/cli-sessions/{id}/deny".post(deny_cli_session),
    ))
    .into_route_nodes()
}
