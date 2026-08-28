//! Router assembly and the handlers that are not part of the OAuth flow.

use core::str::FromStr;

use flyco_core::{
    ApiKeyId, ApiKeySummary, ApprovalId, ApprovalState, ApprovalView, BudgetConfig, BudgetView,
    ControlToDaemon, CreateApiKey, CreateSession, CreatedApiKey, CurrentUser, DaemonToken,
    DecideApproval, RepoSlug, SessionDetail, SessionId, SessionState, SessionSummary, UpdateMe,
    wire::ApprovalPayload,
};
use serde::{Deserialize, Serialize};
use skyzen::extract::Query;
use skyzen::middleware::ErrorHandlingMiddleware;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Router, Routes as _};
use skyzen::utils::{Bytes, Json, State};
use skyzen::{HttpError as _, Response};
use skyzen_services::{Db, Kv, Storage};

use crate::authenticator::FlycoAuthenticator;
use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::extract::Headers;
use crate::github::{GithubOauth, ZenwaveGithub};
use crate::middleware::{DaemonSession, RequireAuth, RequireDaemon};
use crate::problem::Outcome;
use crate::relay::{RelayTicket, TicketQuery};
use crate::respond::{WithStatus, created, no_content};
use crate::room::EventPage;
use crate::rooms::Rooms;
use crate::{
    api_keys, approvals, daemon_tokens, database, oauth, problem, relay, sessions, transcripts,
    users,
};

/// Health probe response.
#[derive(Debug, Serialize, skyzen::ToSchema)]
struct Health {
    /// Wire protocol version this control plane speaks to daemons.
    wire_protocol_version: u32,
}

/// Reports that the control plane is up, and which wire protocol version it
/// speaks to session daemons.
#[skyzen::openapi]
async fn healthz() -> Json<Health> {
    Json(Health {
        wire_protocol_version: flyco_core::WIRE_PROTOCOL_VERSION,
    })
}

/// Describes the account behind the presented credential.
#[skyzen::openapi]
async fn me(State(user): State<CurrentUser>) -> Json<CurrentUser> {
    Json(user)
}

/// Updates the caller's account settings.
#[skyzen::openapi]
async fn update_me(
    State(user): State<CurrentUser>,
    Json(update): Json<UpdateMe>,
    db: Db,
) -> Outcome<Json<CurrentUser>> {
    apply_update_me(&user, update, &db).await.into()
}

async fn apply_update_me(
    user: &CurrentUser,
    update: UpdateMe,
    db: &Db,
) -> Result<Json<CurrentUser>, ApiError> {
    if let Some(cap) = update.session_cap {
        users::set_session_cap(db, user.id, cap).await?;
    }

    users::find(db, user.id)
        .await?
        .map(Json)
        .ok_or(ApiError::CorruptRecord(
            "the row for an authenticated user disappeared mid-request",
        ))
}

/// Mints an API key, returning the plaintext key exactly once.
#[skyzen::openapi]
async fn create_api_key(
    State(user): State<CurrentUser>,
    Json(request): Json<CreateApiKey>,
    db: Db,
) -> Outcome<Json<CreatedApiKey>> {
    api_keys::create(&db, user.id, request.label)
        .await
        .inspect(|key| tracing::info!(label = %key.label, "minted an API key"))
        .map(Json)
        .into()
}

/// Lists the caller's API keys, without their secrets.
#[skyzen::openapi]
async fn list_api_keys(
    State(user): State<CurrentUser>,
    db: Db,
) -> Outcome<Json<Vec<ApiKeySummary>>> {
    api_keys::list(&db, user.id).await.map(Json).into()
}

/// Revokes one of the caller's API keys.
#[skyzen::openapi]
async fn revoke_api_key(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Outcome<Response> {
    revoke(&user, &params, &db).await.into()
}

async fn revoke(user: &CurrentUser, params: &Params, db: &Db) -> Result<Response, ApiError> {
    api_keys::revoke(db, user.id, path_id::<ApiKeyId>(params, "id")?).await?;
    Ok(no_content())
}

/// Reads a `{id}` path parameter as a typed identifier.
fn path_id<T: FromStr>(params: &Params, name: &'static str) -> Result<T, ApiError> {
    let raw = params
        .get(name)
        .map_err(|_| ApiError::CorruptRecord("the router did not bind a path parameter"))?;
    raw.parse()
        .map_err(|_| ApiError::MalformedId(raw.to_owned()))
}

/// Starts a session: reserves the budget and puts the session in the
/// provisioning queue. No machine exists yet.
#[skyzen::openapi]
async fn create_session(
    State(user): State<CurrentUser>,
    Json(request): Json<CreateSession>,
    db: Db,
) -> Outcome<WithStatus<Json<SessionDetail>>> {
    start_session(&user, request, &db).await.into()
}

async fn start_session(
    user: &CurrentUser,
    request: CreateSession,
    db: &Db,
) -> Result<WithStatus<Json<SessionDetail>>, ApiError> {
    let repo = request
        .repo
        .parse::<RepoSlug>()
        .map_err(|_| ApiError::InvalidRepo(request.repo.clone()))?;
    let budget = BudgetConfig::new(request.budget_limit).map_err(|_| ApiError::InvalidBudget)?;

    let session = sessions::create(
        db,
        user.id,
        user.session_cap,
        request.harness,
        &repo,
        budget,
    )
    .await?;

    tracing::info!(repo = %repo, harness = ?request.harness, spot = request.spot, "opened a session");
    Ok(created(Json(session)))
}

/// Lists the caller's sessions, newest first.
#[skyzen::openapi]
async fn list_sessions(
    State(user): State<CurrentUser>,
    db: Db,
) -> Outcome<Json<Vec<SessionSummary>>> {
    sessions::list(&db, user.id).await.map(Json).into()
}

/// Describes one of the caller's sessions, including its budget.
#[skyzen::openapi]
async fn get_session(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Outcome<Json<SessionDetail>> {
    read_session(&user, &params, &db).await.into()
}

async fn read_session(
    user: &CurrentUser,
    params: &Params,
    db: &Db,
) -> Result<Json<SessionDetail>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    sessions::find(db, user.id, id).await.map(Json)
}

/// Archives a session, releasing its execution environment for good.
#[skyzen::openapi]
async fn archive_session(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Outcome<Json<SessionDetail>> {
    end_session(&user, &params, &db).await.into()
}

async fn end_session(
    user: &CurrentUser,
    params: &Params,
    db: &Db,
) -> Result<Json<SessionDetail>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    sessions::transition(db, user.id, id, SessionState::Archived)
        .await
        .map(Json)
}

/// Reports a session's budget, recomputed from its spend ledger.
#[skyzen::openapi]
async fn get_session_budget(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Outcome<Json<BudgetView>> {
    read_budget(&user, &params, &db).await.into()
}

async fn read_budget(
    user: &CurrentUser,
    params: &Params,
    db: &Db,
) -> Result<Json<BudgetView>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    Ok(Json(sessions::find(db, user.id, id).await?.budget))
}

/// Narrows a listing of approvals.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
struct ApprovalFilter {
    /// Only approvals raised by this session.
    session: Option<SessionId>,
    /// Only approvals in this state — `pending` is the useful one.
    state: Option<ApprovalState>,
}

/// Lists approvals raised against the caller's sessions, newest first.
#[skyzen::openapi]
async fn list_approvals(
    State(user): State<CurrentUser>,
    Query(filter): Query<ApprovalFilter>,
    db: Db,
) -> Outcome<Json<Vec<ApprovalView>>> {
    approvals::list(&db, user.id, filter.session, filter.state)
        .await
        .map(Json)
        .into()
}

/// Records the caller's decision on a pending approval.
#[skyzen::openapi]
async fn decide_approval(
    State(user): State<CurrentUser>,
    params: Params,
    Json(request): Json<DecideApproval>,
    rooms: Rooms,
    db: Db,
) -> Outcome<Json<ApprovalView>> {
    settle_approval(&user, &params, request, &rooms, &db)
        .await
        .into()
}

async fn settle_approval(
    user: &CurrentUser,
    params: &Params,
    request: DecideApproval,
    rooms: &Rooms,
    db: &Db,
) -> Result<Json<ApprovalView>, ApiError> {
    let id = path_id::<ApprovalId>(params, "id")?;

    // D1 first, and only then the room. The decision is durable before it is
    // announced, so a daemon can never act on an approval the database would
    // still call pending — and the conditional `UPDATE` in `approvals` is
    // what makes "decided exactly once" true, so a second caller never gets
    // this far to announce a second decision.
    let decided = approvals::decide(db, user.id, id, request.decision).await?;

    let announced = rooms
        .command(
            decided.session,
            &ControlToDaemon::ApprovalDecision {
                id,
                decision: request.decision,
            },
        )
        .await;
    if let Err(error) = announced {
        // The decision stands whatever the relay did with it: the daemon
        // re-reads pending approvals when it reconnects, so a room that is
        // asleep or evicted costs a round trip, not the answer.
        tracing::warn!(%error, session = %decided.session, "a decided approval did not reach its room");
    }

    tracing::info!(decision = ?request.decision, "decided an approval");
    Ok(Json(decided))
}

// ── Pairing a session with its daemon ──

/// Mints the daemon token that pairs a session with its `flycod`.
///
/// Returned once and stored only as a hash; minting again replaces the
/// previous token, so re-pairing revokes the daemon that held it.
#[skyzen::openapi]
async fn create_daemon_token(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Outcome<Json<DaemonToken>> {
    pair_daemon(&user, &params, &db).await.into()
}

async fn pair_daemon(
    user: &CurrentUser,
    params: &Params,
    db: &Db,
) -> Result<Json<DaemonToken>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    let token = daemon_tokens::issue(db, user.id, id).await?;
    tracing::info!(session = %id, "minted a daemon token");
    Ok(Json(token))
}

// ── The live relay ──

/// Mints a single-use ticket a browser exchanges for a relay socket.
#[skyzen::openapi]
async fn create_relay_ticket(
    State(user): State<CurrentUser>,
    params: Params,
    kv: Kv,
    db: Db,
) -> Outcome<Json<RelayTicket>> {
    mint_ticket(&user, &params, &kv, &db).await.into()
}

async fn mint_ticket(
    user: &CurrentUser,
    params: &Params,
    kv: &Kv,
    db: &Db,
) -> Result<Json<RelayTicket>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    relay::issue_ticket(kv, db, user, id).await.map(Json)
}

/// Joins a session's room as its daemon.
///
/// Authenticated by the session's `fd_` token rather than by a user
/// credential, so it sits outside [`RequireAuth`].
async fn open_daemon_relay(
    params: Params,
    headers: Headers,
    rooms: Rooms,
    db: Db,
) -> Outcome<Response> {
    join_as_daemon(&params, &headers, &rooms, &db).await.into()
}

async fn join_as_daemon(
    params: &Params,
    headers: &Headers,
    rooms: &Rooms,
    db: &Db,
) -> Result<Response, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    relay::open_daemon(rooms, db, id, headers.bearer()).await
}

/// Joins a session's room as a browser, redeeming a relay ticket.
async fn open_client_relay(
    params: Params,
    query: Query<TicketQuery>,
    rooms: Rooms,
    kv: Kv,
) -> Outcome<Response> {
    join_as_client(&params, &query, &rooms, &kv).await.into()
}

async fn join_as_client(
    params: &Params,
    query: &Query<TicketQuery>,
    rooms: &Rooms,
    kv: &Kv,
) -> Result<Response, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    relay::open_client(rooms, kv, id, relay::presented_ticket(query)).await
}

/// Where in a session's event stream to resume reading.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
struct EventCursor {
    /// Return events strictly after this position.
    after: Option<u64>,
}

/// Reads a session's recorded event tail.
///
/// The catch-up path a browser takes before — and alongside — its live
/// socket: replay from the last position it saw, then follow the relay.
#[skyzen::openapi]
async fn get_session_events(
    State(user): State<CurrentUser>,
    params: Params,
    Query(cursor): Query<EventCursor>,
    rooms: Rooms,
    db: Db,
) -> Outcome<Json<EventPage>> {
    read_events(&user, &params, cursor.after.unwrap_or(0), &rooms, &db)
        .await
        .into()
}

async fn read_events(
    user: &CurrentUser,
    params: &Params,
    after: u64,
    rooms: &Rooms,
    db: &Db,
) -> Result<Json<EventPage>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    // A room cannot reach D1, so ownership is settled here, before the
    // Worker will address the room at all.
    if !sessions::is_owned_by(db, user.id, id).await? {
        return Err(ApiError::SessionNotFound);
    }
    rooms.events(id, after).await.map(Json)
}

// ── Daemon-scoped routes ──

/// Raises an approval against the daemon's own session.
///
/// The daemon records the durable approval here *before* it announces the
/// request on the relay, so the id a browser sees is always one the API can
/// decide.
async fn raise_approval(
    State(session): State<DaemonSession>,
    Json(payload): Json<ApprovalPayload>,
    db: Db,
) -> Outcome<WithStatus<Json<ApprovalView>>> {
    record_approval(session.0, &payload, &db).await.into()
}

async fn record_approval(
    session: SessionId,
    payload: &ApprovalPayload,
    db: &Db,
) -> Result<WithStatus<Json<ApprovalView>>, ApiError> {
    let id = approvals::raise(db, session, payload).await?;
    let view = approvals::find_for_session(db, session, id).await?;
    tracing::info!(%session, "a daemon raised an approval");
    Ok(created(Json(view)))
}

/// Stores one batch of a session's transcript.
async fn put_transcript_batch(
    State(session): State<DaemonSession>,
    params: Params,
    body: Bytes,
    storage: Storage,
) -> Outcome<Response> {
    store_batch(session.0, &params, body, &storage).await.into()
}

async fn store_batch(
    session: SessionId,
    params: &Params,
    body: Bytes,
    storage: &Storage,
) -> Result<Response, ApiError> {
    let stream = path_segment(params, "stream")?;
    let raw = path_segment(params, "seq")?;
    let seq = raw.parse::<u64>().map_err(|_| ApiError::MalformedId(raw))?;

    transcripts::put_batch(storage, session, &stream, seq, body.to_vec()).await?;
    Ok(no_content())
}

/// Reads a session's transcript stream back, for a resume onto a new host.
async fn get_transcript(
    State(session): State<DaemonSession>,
    params: Params,
    storage: Storage,
) -> Outcome<Response> {
    read_transcript(session.0, &params, &storage).await.into()
}

async fn read_transcript(
    session: SessionId,
    params: &Params,
    storage: &Storage,
) -> Result<Response, ApiError> {
    let stream = path_segment(params, "stream")?;
    let read = transcripts::read_stream(storage, session, &stream).await?;

    let mut response = Response::new(skyzen::Body::from(read.body));
    response.headers_mut().insert(
        skyzen::header::CONTENT_TYPE,
        skyzen::header::HeaderValue::from_static(transcripts::CONTENT_TYPE),
    );
    response.headers_mut().insert(
        transcripts::BATCH_COUNT_HEADER,
        skyzen::header::HeaderValue::from_str(&read.batches.to_string()).map_err(|_| {
            ApiError::CorruptRecord("a transcript batch count did not fit in a header")
        })?,
    );
    Ok(response)
}

/// Reads a `{name}` path parameter as an owned string.
fn path_segment(params: &Params, name: &'static str) -> Result<String, ApiError> {
    params
        .get(name)
        .map(ToOwned::to_owned)
        .map_err(|_| ApiError::CorruptRecord("the router did not bind a path parameter"))
}

/// Routes that anyone may call.
fn public_routes<G: GithubOauth>() -> Vec<RouteNode> {
    Route::new((
        "/v1/healthz".at(healthz),
        "/v1/auth/github".route((
            "/start".post(oauth::start),
            "/callback".at(oauth::callback::<G>),
        )),
    ))
    .into_route_nodes()
}

/// The two relay upgrades.
///
/// Neither carries a user credential — one presents a session's daemon
/// token, the other a single-use ticket — so they authenticate themselves
/// rather than sitting behind [`RequireAuth`]. They are also the only
/// routes whose success is not a document, so they are left out of the
/// `OpenAPI` export: a `101` with a socket attached is not something the
/// spec's response model can describe.
fn relay_routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/sessions/{id}/relay/daemon".at(open_daemon_relay),
        "/v1/sessions/{id}/relay/client".at(open_client_relay),
    ))
    .into_route_nodes()
}

/// Routes a session's own daemon calls, authenticated by its `fd_` token.
fn daemon_routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/sessions/{id}/approvals".post(raise_approval),
        "/v1/sessions/{id}/transcript/{stream}".at(get_transcript),
        "/v1/sessions/{id}/transcript/{stream}/batches/{seq}".put(put_transcript_batch),
    ))
    .middleware(RequireDaemon::new())
    .into_route_nodes()
}

/// Routes that require a bearer credential.
fn authenticated_routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/me".at(me).patch(update_me),
        "/v1/api-keys".post(create_api_key).get(list_api_keys),
        "/v1/api-keys/{id}".delete(revoke_api_key),
        "/v1/sessions".post(create_session).get(list_sessions),
        "/v1/sessions/{id}".at(get_session),
        "/v1/sessions/{id}/archive".post(archive_session),
        "/v1/sessions/{id}/budget".at(get_session_budget),
        "/v1/sessions/{id}/daemon-token".post(create_daemon_token),
        "/v1/sessions/{id}/relay-ticket".post(create_relay_ticket),
        "/v1/sessions/{id}/events".at(get_session_events),
        "/v1/approvals".at(list_approvals),
        "/v1/approvals/{id}/decision".post(decide_approval),
    ))
    .middleware(RequireAuth::new(FlycoAuthenticator::new()))
    .into_route_nodes()
}

/// The complete route tree, before state or middleware is attached.
///
/// Separated from [`router`] so the `OpenAPI` export can describe the API
/// without opening a database or reading any configuration.
fn routes<G: GithubOauth>() -> Route {
    let mut nodes = public_routes::<G>();
    nodes.extend(relay_routes());
    nodes.extend(daemon_routes());
    nodes.extend(authenticated_routes());
    Route::new(nodes)
}

/// Gives the route tree somewhere to find session rooms.
///
/// On the Worker a room is resolved from the `SESSION_ROOMS` binding the
/// runtime already put in request extensions, so there is nothing to
/// attach. Natively the simulator's namespace is process-local state and
/// has to be carried: one namespace per router, so two routers in one test
/// binary do not share rooms.
#[cfg(not(target_arch = "wasm32"))]
fn with_rooms(route: Route) -> Route {
    route.with(State(crate::rooms::NativeRooms::new()))
}

/// See the native counterpart above.
#[cfg(target_arch = "wasm32")]
const fn with_rooms(route: Route) -> Route {
    route
}

/// The `OpenAPI` document describing the control plane.
///
/// Only debug native builds collect handler metadata (skyzen gathers it
/// through a `linkme` slice that is compiled out otherwise), so the export
/// binary must be built in debug.
#[must_use]
pub fn openapi_document() -> skyzen::OpenApi {
    routes::<ZenwaveGithub>().openapi()
}

/// Builds the control-plane router around an explicit configuration, GitHub
/// client, and database.
///
/// All three are injected rather than discovered, so tests can drive the
/// OAuth callback without reaching `github.com` or a real D1. The KV store
/// arrives separately: it is a declared portable service, so
/// `#[skyzen::main]` wraps the router with it.
#[must_use]
pub fn router<G: GithubOauth>(config: ApiConfig, github: G, db: Db) -> Router {
    with_rooms(routes::<G>())
        .with(State(config))
        .with(State(github))
        .with(db)
        // Outermost, so extractor and routing failures answer in the same
        // shape flyco's own errors do.
        .with(ErrorHandlingMiddleware::new(
            |error: skyzen::BoxHttpError| async move {
                let status = error.status();
                let title = status.canonical_reason().unwrap_or("Error");
                let detail = if status.is_server_error() {
                    tracing::error!(%error, "request failed");
                    "The control plane failed to handle this request.".to_owned()
                } else {
                    error.to_string()
                };
                problem::response(
                    &flyco_core::Problem::about_blank(status.as_u16(), title, detail),
                    None,
                )
            },
        ))
        .build()
}

/// Builds the router the deployed control plane runs.
///
/// # Panics
///
/// Panics if any required configuration binding is missing or malformed, or
/// if the database cannot be opened — a misconfigured control plane must
/// fail at startup, not at the first sign-in attempt.
pub async fn router_from_environment() -> Router {
    let config = ApiConfig::from_environment()
        .unwrap_or_else(|error| panic!("flyco control plane is misconfigured: {error}"));
    router(config, ZenwaveGithub::new(), database::open().await)
}

#[cfg(test)]
mod tests {
    use flyco_core::{ApiKeySummary, CreateApiKey, CreatedApiKey, CurrentUser, Problem};
    use skyzen_services::{Db, Kv};
    use skyzen_test::TestContext;

    use crate::testing::{GITHUB_LOGIN, migrated_router, seed_user, test_router};
    use crate::{api_keys, session};

    #[skyzen::test]
    async fn healthz_reports_protocol_version(ctx: TestContext, db: Db) {
        let client = ctx.client(test_router(db));
        let response = client.get("/v1/healthz").send().await;
        response.assert_status(200);
        let body: serde_json::Value = response.json();
        assert_eq!(
            body["wire_protocol_version"],
            u64::from(flyco_core::WIRE_PROTOCOL_VERSION)
        );
    }

    #[skyzen::test]
    async fn an_anonymous_request_is_challenged(ctx: TestContext, _kv: Kv, db: Db) {
        let response = ctx
            .client(migrated_router(&db).await)
            .get("/v1/me")
            .send()
            .await;

        response.assert_status(401);
        response.assert_header("content-type", "application/problem+json");
        response.assert_header("www-authenticate", "Bearer");

        let problem: Problem = response.json();
        assert_eq!(problem.status, 401);
        assert_eq!(
            problem.kind,
            "https://flyco.dev/problems/missing-credential"
        );
    }

    #[skyzen::test]
    async fn a_rejected_credential_is_named_as_such(ctx: TestContext, _kv: Kv, db: Db) {
        let client = ctx.client(migrated_router(&db).await);

        for token in [
            "fk_not-a-real-key",
            "fs_not-a-real-session",
            "neither-prefix",
        ] {
            let response = client.get("/v1/me").bearer(token).send().await;
            response.assert_status(401);
            response.assert_header("www-authenticate", "Bearer error=\"invalid_token\"");
            assert_eq!(
                response.json::<Problem>().kind,
                "https://flyco.dev/problems/invalid-credential"
            );
        }
    }

    #[skyzen::test]
    async fn a_non_bearer_scheme_is_treated_as_no_credential(ctx: TestContext, _kv: Kv, db: Db) {
        let response = ctx
            .client(migrated_router(&db).await)
            .get("/v1/me")
            .header("Authorization", "Basic Zm9vOmJhcg==")
            .send()
            .await;

        response.assert_status(401);
        response.assert_header("www-authenticate", "Bearer");
    }

    #[skyzen::test]
    async fn me_answers_for_a_session_token(ctx: TestContext, kv: Kv, db: Db) {
        let router = migrated_router(&db).await;
        let user = seed_user(&db).await;
        let token = session::issue(&kv, user.id).await.expect("issue a session");
        assert!(token.starts_with(session::TOKEN_PREFIX));

        let response = ctx.client(router).get("/v1/me").bearer(&token).send().await;

        response.assert_status(200);
        assert_eq!(response.json::<CurrentUser>(), user);
    }

    #[skyzen::test]
    async fn me_answers_for_an_api_key(ctx: TestContext, _kv: Kv, db: Db) {
        let router = migrated_router(&db).await;
        let user = seed_user(&db).await;
        let key = api_keys::create(&db, user.id, "ci".to_owned())
            .await
            .expect("mint a key");

        let response = ctx
            .client(router)
            .get("/v1/me")
            .bearer(&key.token)
            .send()
            .await;

        response.assert_status(200);
        assert_eq!(response.json::<CurrentUser>().login, GITHUB_LOGIN);

        let listed = api_keys::list(&db, user.id).await.expect("list");
        assert!(
            listed[0].last_used_unix.is_some(),
            "authenticating with a key stamps it as used"
        );
    }

    #[skyzen::test]
    async fn api_keys_round_trip(ctx: TestContext, kv: Kv, db: Db) {
        let router = migrated_router(&db).await;
        let user = seed_user(&db).await;
        let token = session::issue(&kv, user.id).await.expect("issue a session");
        let client = ctx.client(router);

        let created = client
            .post("/v1/api-keys")
            .bearer(&token)
            .json(&CreateApiKey {
                label: "laptop".to_owned(),
            })
            .send()
            .await;
        created.assert_status(200);
        let created: CreatedApiKey = created.json();
        assert!(created.token.starts_with(api_keys::TOKEN_PREFIX));
        assert_eq!(created.label, "laptop");

        // D1 keeps the hash and nothing else, so the plaintext key cannot be
        // recovered from the table.
        let stored: std::collections::BTreeMap<String, String> = db
            .query("SELECT token_hash FROM api_keys WHERE id = ?")
            .bind(created.id.to_string())
            .fetch_one()
            .await
            .expect("read the stored key");
        let stored = stored.get("token_hash").expect("token_hash column");
        assert_eq!(stored, &crate::crypto::token_hash(&created.token));
        assert!(!stored.contains(&created.token));

        let listed = client.get("/v1/api-keys").bearer(&token).send().await;
        listed.assert_status(200);
        let listed: Vec<ApiKeySummary> = listed.json();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, created.id);
        assert_eq!(listed[0].last_used_unix, None);

        let path = format!("/v1/api-keys/{}", created.id);
        client
            .delete(&path)
            .bearer(&token)
            .send()
            .await
            .assert_status(204);

        let gone = client.delete(&path).bearer(&token).send().await;
        gone.assert_status(404);
        assert_eq!(
            gone.json::<Problem>().kind,
            "https://flyco.dev/problems/api-key-not-found"
        );

        let listed = client.get("/v1/api-keys").bearer(&token).send().await;
        assert_eq!(
            listed.json::<Vec<ApiKeySummary>>(),
            [] as [ApiKeySummary; 0]
        );
    }

    #[skyzen::test]
    async fn a_malformed_key_id_is_a_bad_request(ctx: TestContext, kv: Kv, db: Db) {
        let router = migrated_router(&db).await;
        let user = seed_user(&db).await;
        let token = session::issue(&kv, user.id).await.expect("issue a session");

        let response = ctx
            .client(router)
            .delete("/v1/api-keys/not-a-uuid")
            .bearer(&token)
            .send()
            .await;

        response.assert_status(400);
        assert_eq!(
            response.json::<Problem>().kind,
            "https://flyco.dev/problems/malformed-id"
        );
    }

    #[skyzen::test]
    async fn an_extractor_failure_still_answers_with_a_problem(ctx: TestContext, kv: Kv, db: Db) {
        let router = migrated_router(&db).await;
        let user = seed_user(&db).await;
        let token = session::issue(&kv, user.id).await.expect("issue a session");

        // No `Content-Type: application/json`, so the body extractor refuses.
        let response = ctx
            .client(router)
            .post("/v1/api-keys")
            .bearer(&token)
            .body("{}")
            .send()
            .await;

        response.assert_status(400);
        response.assert_header("content-type", "application/problem+json");
        assert_eq!(response.json::<Problem>().kind, "about:blank");
    }
}
