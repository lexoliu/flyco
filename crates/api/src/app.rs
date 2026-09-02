//! Router assembly and the handlers that are not part of the OAuth flow.

use flyco_core::{
    AgentMachineView, ApiKeyId, ApiKeySummary, ApprovalDecision, ApprovalId, ApprovalState,
    ApprovalView, BranchName, BudgetConfig, BudgetView, ClientEvent, ControlToDaemon, CreateApiKey,
    CreateSession, CreatedApiKey, CurrentUser, DaemonToken, DecideApproval, EnvDocument,
    HarnessFeature, HarnessObservation, MAX_SESSION_TITLE_CHARS, MachineCatalogEntry,
    MachineOrigin, MachineSpec, RepoSlug, RepoStatus, ReportProvisioningStage, ResizeMachine,
    SendMessage, SessionDetail, SessionId, SessionState, SessionSummary, TurnPage, UpdateEnv,
    UpdateMe, UpdateSession, UserId, wire::ApprovalPayload,
};
use serde::{Deserialize, Serialize};
use skyzen::extract::Query;
use skyzen::middleware::ErrorHandlingMiddleware;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Router, Routes as _};
use skyzen::static_files::EmbeddedStaticDir;
use skyzen::utils::{Bytes, Json, State};
use skyzen::{HttpError as _, Response};
use skyzen_services::{Db, Kv, Queue, Storage};

use crate::authenticator::FlycoAuthenticator;
use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::extract::{Headers, path_id, path_segment};
use crate::github::GithubClient;
use crate::middleware::{DaemonSession, RequireAuth, RequireDaemon};
use crate::problem::Outcome;
use crate::provisioning_queue::{self, ProvisioningJob};
use crate::relay::{RelayTicket, TicketQuery};
use crate::respond::{Accepted, Created, NoContent};
use crate::room::EventPage;
use crate::rooms::Rooms;
use crate::vendors::Vendors;
use crate::{
    agents_md, api_keys, approvals, claude_oauth, codex_oauth, daemon_tokens, env,
    harness_accounts, machines, mcp, memory, oauth, observations, problem, provider_accounts,
    provisioning, push, relay, releases, repos, responses, sessions, skills, transcripts, turns,
    users, webhooks, workdirs,
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

/// Serves one allowlisted execution-plane binary or checksum from R2.
async fn get_release_artifact(params: Params, storage: Storage) -> Outcome<Response> {
    read_release_artifact(&params, &storage).await.into()
}

async fn read_release_artifact(params: &Params, storage: &Storage) -> Result<Response, ApiError> {
    let name = path_segment(params, "artifact")?;
    let artifact = releases::get(storage, &name)
        .await?
        .ok_or(ApiError::ReleaseArtifactNotFound)?;
    let mut response = Response::new(skyzen::Body::from(artifact.body));
    response.headers_mut().insert(
        skyzen::header::CONTENT_TYPE,
        skyzen::header::HeaderValue::from_static(artifact.content_type),
    );
    response.headers_mut().insert(
        skyzen::header::CACHE_CONTROL,
        skyzen::header::HeaderValue::from_static("public, max-age=60"),
    );
    Ok(response)
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
) -> Outcome<NoContent> {
    revoke(&user, &params, &db).await.into()
}

async fn revoke(user: &CurrentUser, params: &Params, db: &Db) -> Result<NoContent, ApiError> {
    api_keys::revoke(db, user.id, path_id::<ApiKeyId>(params, "id")?).await?;
    Ok(NoContent)
}

/// Starts a session: reserves its budget and its machine, and queues the
/// machine to be built. No machine exists yet.
///
/// The choice of machine is validated against the named account's own
/// catalog *before* anything is written, so a machine the account cannot
/// deploy is refused here rather than accepted and then failed minutes later
/// by a queue consumer the caller is no longer watching.
#[skyzen::openapi]
async fn create_session(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(github): State<GithubClient>,
    Json(request): Json<CreateSession>,
    rooms: Rooms,
    queue: Queue,
    db: Db,
) -> Outcome<Created<Json<SessionDetail>>> {
    start_session(&user, request, &config, &github, &rooms, &queue, &db)
        .await
        .into()
}

async fn start_session(
    user: &CurrentUser,
    request: CreateSession,
    config: &ApiConfig,
    github: &GithubClient,
    rooms: &Rooms,
    queue: &Queue,
    db: &Db,
) -> Result<Created<Json<SessionDetail>>, ApiError> {
    let prompt = request.prompt.trim();
    if prompt.is_empty() {
        return Err(ApiError::EmptyMessage);
    }
    let prompt = prompt.to_owned();
    let repo = request
        .repo
        .parse::<RepoSlug>()
        .map_err(|_| ApiError::InvalidRepo(request.repo.clone()))?;
    let branch = resolve_branch(github, config, db, user, &repo, request.branch.as_deref()).await?;
    let budget = BudgetConfig::new(request.budget_limit).map_err(|_| ApiError::InvalidBudget)?;

    let machine_origin = if request.machine.is_some() {
        MachineOrigin::User
    } else {
        MachineOrigin::Auto
    };
    let choice = match request.machine {
        Some(choice) => choice,
        None => {
            machines::automatic(db, config, user.id, request.spot, None)
                .await?
                .choice
        }
    };
    let account = provisioning::account(db, config, user.id, choice.provider_account).await?;
    let spec = MachineSpec {
        provider: account.kind(),
        machine_type: choice.machine_type,
        region: choice.region,
        spot: choice.spot,
        disk_gib: choice.disk_gib,
    };
    provisioning::deployable(&account, &spec)
        .await
        .map_err(undeployable)?;

    let session = sessions::create(
        db,
        user.session_cap,
        sessions::Opening {
            user: user.id,
            title: &flyco_core::excerpt(&prompt, MAX_SESSION_TITLE_CHARS),
            harness: request.harness,
            repo: &repo,
            branch: &branch,
            machine_origin,
            budget,
        },
    )
    .await?;
    let id = session.summary.id;
    let machine = machines::reserve(db, id, account.id, &spec).await?;

    // The prompt is posted to the session's room *before* the machine is
    // queued, so the agent's first instruction is durable before anything
    // asynchronous can go wrong. No daemon exists yet — the room holds it
    // in its mailbox and hands it over on the daemon's first `Hello`.
    //
    // Both steps fail the session rather than return early: a session row
    // whose prompt never reached its room, or whose job never reached the
    // queue, would sit in `provisioning` waiting for something that is never
    // going to happen.
    fail_session_on(
        db,
        id,
        "the session's room would not take its first prompt",
        rooms
            .command(id, &ControlToDaemon::UserMessage { text: prompt })
            .await,
    )
    .await?;
    fail_session_on(
        db,
        id,
        "the provisioning queue would not accept this session's job",
        provisioning_queue::enqueue(queue, ProvisioningJob::first(id, machine)).await,
    )
    .await?;

    tracing::info!(
        repo = %repo,
        branch = %branch,
        harness = ?request.harness,
        machine_type = %spec.machine_type,
        region = %spec.region,
        spot = spec.spot,
        "opened a session and queued its machine"
    );
    Ok(Created(Json(session)))
}

/// Settles which branch a session works on, before any row is written.
///
/// A session must always know its branch — the machine has to clone
/// *something*, and the header renders `repo · branch` (docs/ux.md §9.1) —
/// so a request that names none has the repository's default read from
/// GitHub here rather than left for the provisioning queue to guess at
/// minutes later.
///
/// The same call establishes that the caller's stored GitHub authorization
/// can actually reach the repository. Checking it here as well as in the
/// queue is deliberate: this is where the user is watching, and being told
/// to sign in again in the moment they pressed send is worth a great deal
/// more than the same sentence attached to a session that failed while they
/// were elsewhere.
async fn resolve_branch(
    github: &GithubClient,
    config: &ApiConfig,
    db: &Db,
    user: &CurrentUser,
    repo: &RepoSlug,
    requested: Option<&str>,
) -> Result<BranchName, ApiError> {
    use crate::github::{GithubOauth as _, REPO_SCOPE};

    let token = users::github_token(db, config, user.id).await?;
    if !github.current_user(&token).await?.grants_repo_scope() {
        return Err(ApiError::GithubTokenInsufficient {
            scope: REPO_SCOPE,
            repo: repo.clone(),
        });
    }

    match requested {
        Some(name) => name
            .trim()
            .parse()
            .map_err(
                |error: flyco_core::BranchNameError| ApiError::InvalidBranch {
                    name: name.to_owned(),
                    reason: error.to_string(),
                },
            ),
        None => Ok(github.get_repo(&token, repo).await?.default_branch),
    }
}

/// Marks a freshly opened session failed when one of its hand-off steps
/// refused, and passes that refusal on.
async fn fail_session_on<T>(
    db: &Db,
    id: SessionId,
    reason: &str,
    step: Result<T, ApiError>,
) -> Result<T, ApiError> {
    match step {
        Ok(value) => Ok(value),
        Err(error) => {
            sessions::fail(db, id, reason).await?;
            Err(error)
        }
    }
}

/// Turns a provider's refusal into the answer the caller can act on.
///
/// A machine the account cannot deploy is the caller's mistake and names
/// which of the three gates it hit; anything else is the provider being
/// unreachable, which is not.
fn undeployable(error: flyco_provider::ProviderError) -> ApiError {
    match error {
        unavailable @ flyco_provider::ProviderError::Unavailable { .. } => {
            ApiError::MachineUnavailable(unavailable.to_string())
        }
        other => ApiError::Provisioning(other.to_string()),
    }
}

/// The verified per-harness feature matrix.
#[skyzen::openapi]
async fn list_harness_features() -> Outcome<Json<Vec<HarnessFeature>>> {
    Ok(Json(flyco_core::matrix())).into()
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

/// Renames one of the caller's sessions.
///
/// The title opens as the excerpt of the prompt the session was created
/// with; this is how it becomes something the user chose.
#[skyzen::openapi]
async fn update_session(
    State(user): State<CurrentUser>,
    params: Params,
    Json(update): Json<UpdateSession>,
    db: Db,
) -> Outcome<Json<SessionDetail>> {
    rename_session(&user, &params, &update, &db).await.into()
}

async fn rename_session(
    user: &CurrentUser,
    params: &Params,
    update: &UpdateSession,
    db: &Db,
) -> Result<Json<SessionDetail>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    sessions::rename(db, user.id, id, &update.title)
        .await
        .map(Json)
}

/// Narrows a manual archive.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
struct ArchiveQuery {
    /// Required when the working tree is dirty: the caller has seen the
    /// warning and still wants the disk released without keeping the
    /// uncommitted work.
    #[serde(default)]
    discard_uncommitted: bool,
}

/// Archives a session, releasing its execution environment for good.
#[skyzen::openapi]
async fn archive_session(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    Query(query): Query<ArchiveQuery>,
    params: Params,
    rooms: Rooms,
    db: Db,
) -> Outcome<Json<SessionDetail>> {
    end_session(
        user.id,
        &config,
        &params,
        &rooms,
        &db,
        ArchiveKind::Manual {
            discard_uncommitted: query.discard_uncommitted,
        },
    )
    .await
    .into()
}

/// How a session is being archived.
pub(crate) enum ArchiveKind {
    /// The user asked. A dirty tree requires confirmation and is discarded.
    Manual { discard_uncommitted: bool },
    /// The session sat idle. Uncommitted work is snapshotted first.
    Automatic,
}

async fn end_session(
    user: UserId,
    config: &ApiConfig,
    params: &Params,
    rooms: &Rooms,
    db: &Db,
    kind: ArchiveKind,
) -> Result<Json<SessionDetail>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    archive(db, config, rooms, user, id, kind).await.map(Json)
}

/// Archives one session.
///
/// # Errors
///
/// Returns [`ApiError::DirtyArchive`] when a manual archive would discard
/// uncommitted work without confirmation, [`ApiError::RepoStatusUnknown`]
/// when an active session has never reported its tree, or
/// [`ApiError::InvalidTransition`] when the lifecycle forbids the move.
pub(crate) async fn archive(
    db: &Db,
    config: &ApiConfig,
    rooms: &Rooms,
    user: UserId,
    id: SessionId,
    kind: ArchiveKind,
) -> Result<SessionDetail, ApiError> {
    let state = sessions::state_of(db, user, id).await?;
    state
        .transition(SessionState::Archived)
        .map_err(|error| ApiError::InvalidTransition {
            from: error.from,
            to: error.to,
        })?;

    let preserve_workdir = match kind {
        ArchiveKind::Automatic => true,
        ArchiveKind::Manual {
            discard_uncommitted,
        } => {
            confirm_manual_archive(rooms, id, state, discard_uncommitted).await?;
            false
        }
    };

    rooms
        .command(id, &ControlToDaemon::Archive { preserve_workdir })
        .await?;
    machines::destroy_for_archive(db, config, user, id).await?;
    sessions::transition(db, user, id, SessionState::Archived).await
}

async fn confirm_manual_archive(
    rooms: &Rooms,
    id: SessionId,
    state: SessionState,
    discard_uncommitted: bool,
) -> Result<(), ApiError> {
    if !matches!(
        state,
        SessionState::Active | SessionState::Paused | SessionState::Interrupted
    ) {
        return Ok(());
    }
    let status = rooms.repo_status(id).await?;
    if status.dirty && !discard_uncommitted {
        return Err(ApiError::DirtyArchive {
            summary: status.summary,
        });
    }
    Ok(())
}

/// Archives every session that has sat idle for a week.
///
/// # Errors
///
/// Returns [`ApiError`] if listing or archiving a session fails.
pub async fn archive_idle(
    db: &Db,
    config: &ApiConfig,
    rooms: &Rooms,
    at_unix: u64,
) -> Result<(), ApiError> {
    for idle in sessions::idle_since(db, at_unix).await? {
        archive(
            db,
            config,
            rooms,
            idle.user_id,
            idle.id,
            ArchiveKind::Automatic,
        )
        .await?;
    }
    Ok(())
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
    State(config): State<ApiConfig>,
    params: Params,
    Json(request): Json<DecideApproval>,
    rooms: Rooms,
    db: Db,
) -> Outcome<Json<ApprovalView>> {
    settle_approval(&user, &params, request, &config, &rooms, &db)
        .await
        .into()
}

async fn settle_approval(
    user: &CurrentUser,
    params: &Params,
    request: DecideApproval,
    config: &ApiConfig,
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

    perform_approved(&decided, request.decision, user.id, config, rooms, db).await?;

    tracing::info!(decision = ?request.decision, "decided an approval");
    Ok(Json(decided))
}

/// Carries out the approval the user just allowed, where allowing it *is* the
/// action.
///
/// Most approvals unblock something the daemon is holding — a tool call
/// waiting on a permission — and the daemon performs them. A license-bound
/// resize has nobody waiting: the agent was told the request is pending and
/// went on with its turn, and the machine is the control plane's to change.
/// So the decision and the resize happen in the same request, in that order,
/// and a resize that fails answers with why rather than reporting success on
/// a machine that did not change. The approval stays approved either way —
/// the user did allow it — and the retry is the ordinary resize.
async fn perform_approved(
    approval: &ApprovalView,
    decision: ApprovalDecision,
    user: UserId,
    config: &ApiConfig,
    rooms: &Rooms,
    db: &Db,
) -> Result<(), ApiError> {
    if decision != ApprovalDecision::Approved {
        return Ok(());
    }
    let ApprovalPayload::MachineResizeLicenseBound { machine_type, .. } = &approval.payload else {
        return Ok(());
    };
    tracing::info!(
        session = %approval.session,
        machine_type,
        "the user approved a license-bound resize; moving the machine"
    );
    machines::resize(db, config, rooms, user, approval.session, machine_type).await
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

// ── Driving a session ──

/// Sends a message to a session's agent.
///
/// Answers `202`: the message is handed to the session's room and the reply
/// arrives on the relay, not in this response.
#[skyzen::openapi]
async fn send_message(
    State(user): State<CurrentUser>,
    params: Params,
    Json(message): Json<SendMessage>,
    rooms: Rooms,
    db: Db,
) -> Outcome<Accepted> {
    say(&user, &params, message, &rooms, &db).await.into()
}

async fn say(
    user: &CurrentUser,
    params: &Params,
    message: SendMessage,
    rooms: &Rooms,
    db: &Db,
) -> Result<Accepted, ApiError> {
    if message.text.trim().is_empty() {
        return Err(ApiError::EmptyMessage);
    }
    drive(
        user,
        params,
        rooms,
        db,
        ControlToDaemon::UserMessage {
            text: message.text.clone(),
        },
    )
    .await
}

/// Ends a session's current turn.
///
/// The daemon interrupts the harness — SIGINT for Claude Code,
/// `turn/interrupt` for Codex — which stops the turn without ending the
/// session.
#[skyzen::openapi]
async fn interrupt_session(
    State(user): State<CurrentUser>,
    params: Params,
    rooms: Rooms,
    db: Db,
) -> Outcome<Accepted> {
    drive(&user, &params, &rooms, &db, ControlToDaemon::Interrupt)
        .await
        .into()
}

/// Compacts a session's conversation context.
///
/// Claude Code runs its native `/compact` command; Codex runs
/// `thread/compact/start`. The result is reported on the session relay.
#[skyzen::openapi]
async fn compact_session(
    State(user): State<CurrentUser>,
    params: Params,
    rooms: Rooms,
    db: Db,
) -> Outcome<Accepted> {
    drive(&user, &params, &rooms, &db, ControlToDaemon::Compact)
        .await
        .into()
}

/// Hands one command to a session's room.
///
/// The two checks are in this order for a reason. Ownership settles in D1,
/// because a Durable Object cannot reach it and a room asked to do something
/// has no way of knowing who asked. The lifecycle settles next, because a
/// command sent to a session that is provisioning, paused, or archived would
/// reach a room with no daemon attached and be dropped there — a `202` for
/// work nobody will do. The refusal names the state instead.
async fn drive(
    user: &CurrentUser,
    params: &Params,
    rooms: &Rooms,
    db: &Db,
    command: ControlToDaemon,
) -> Result<Accepted, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    sessions::require_active(db, user.id, id).await?;

    rooms.command(id, &command).await?;
    tracing::info!(session = %id, command = ?core::mem::discriminant(&command), "drove a session");
    Ok(Accepted)
}

/// Puts an interrupted, failed, or archived session back on a machine.
///
/// The same queue every other machine comes from — there is one
/// implementation of provisioning and this is not a second one. The session
/// moves back to `provisioning` durably before the job is enqueued, so a
/// browser that reloads immediately sees a session on its way back rather
/// than the state it was resumed out of.
///
/// The machine row keeps its identity, which is what lets the provider
/// recognise the machine it already made: what a resume rebuilds is the
/// session's own machine, not another one beside it.
#[skyzen::openapi]
async fn resume_session(
    State(user): State<CurrentUser>,
    params: Params,
    queue: Queue,
    db: Db,
) -> Outcome<Json<SessionDetail>> {
    restart_session(&user, &params, &queue, &db).await.into()
}

async fn restart_session(
    user: &CurrentUser,
    params: &Params,
    queue: &Queue,
    db: &Db,
) -> Result<Json<SessionDetail>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    let session = sessions::resume(db, user.id, id).await?;
    let machine = machines::reset_for_resume(db, id).await?;

    if let Err(error) =
        provisioning_queue::enqueue(queue, ProvisioningJob::first(id, machine)).await
    {
        sessions::fail(
            db,
            id,
            "the provisioning queue would not accept this session's job",
        )
        .await?;
        return Err(error);
    }

    tracing::info!(session = %id, machine = %machine, "resumed a session onto its machine");
    Ok(Json(session))
}

/// Where in a session's turn history to read from.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
pub struct TurnCursor {
    /// Opaque cursor from the previous page. Omitted starts at the oldest
    /// turn.
    pub cursor: Option<String>,
    /// How many turns to return; the control plane caps it.
    pub limit: Option<u32>,
}

/// Lists a session's turns, oldest first.
///
/// Folded out of the room's recorded event stream rather than read from a
/// table, because that stream is both what survives a machine and what a
/// browser replays: a turn list built from anything else would disagree
/// with the conversation shown beside it. See [`crate::turns`].
#[skyzen::openapi]
async fn list_turns(
    State(user): State<CurrentUser>,
    params: Params,
    Query(cursor): Query<TurnCursor>,
    rooms: Rooms,
    db: Db,
) -> Outcome<Json<TurnPage>> {
    read_turns(&user, &params, &cursor, &rooms, &db)
        .await
        .into()
}

async fn read_turns(
    user: &CurrentUser,
    params: &Params,
    cursor: &TurnCursor,
    rooms: &Rooms,
    db: &Db,
) -> Result<Json<TurnPage>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    // A room cannot reach D1, so ownership is settled here, before the
    // Worker will address the room at all.
    if !sessions::is_owned_by(db, user.id, id).await? {
        return Err(ApiError::SessionNotFound);
    }
    turns::page(rooms, id, cursor.cursor.as_deref(), cursor.limit)
        .await
        .map(Json)
}

/// Reads a session's `.env`.
///
/// The response repeats the standing warning that flyco does not restrict a
/// session's network access yet, so a client renders the caveat beside the
/// values instead of hard-coding it.
#[skyzen::openapi]
async fn get_session_env(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    params: Params,
    db: Db,
) -> Outcome<Json<EnvDocument>> {
    read_env(&user, &config, &params, &db).await.into()
}

async fn read_env(
    user: &CurrentUser,
    config: &ApiConfig,
    params: &Params,
    db: &Db,
) -> Result<Json<EnvDocument>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    env::read(db, &config.token_cipher(), user.id, id)
        .await
        .map(Json)
}

/// Replaces a session's `.env`.
///
/// The user's route, and the only one that writes: the agent is given the
/// environment read-only. Nothing is pushed to a running session — a
/// process's environment is fixed when it starts — so the new document is
/// what the *next* harness start reads. See [`crate::env`].
#[skyzen::openapi]
async fn put_session_env(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    params: Params,
    Json(update): Json<UpdateEnv>,
    db: Db,
) -> Outcome<Json<EnvDocument>> {
    write_env(&user, &config, &params, update, &db).await.into()
}

async fn write_env(
    user: &CurrentUser,
    config: &ApiConfig,
    params: &Params,
    update: UpdateEnv,
    db: &Db,
) -> Result<Json<EnvDocument>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    env::replace(db, &config.token_cipher(), user.id, id, update.entries)
        .await
        .map(Json)
}

/// Reports whether a session's working tree has uncommitted changes.
///
/// Load-bearing rather than informational: an agent may not stop while the
/// tree is dirty, and archiving a dirty session warns before the disk goes.
#[skyzen::openapi]
async fn get_repo_status(
    State(user): State<CurrentUser>,
    params: Params,
    rooms: Rooms,
    db: Db,
) -> Outcome<Json<RepoStatus>> {
    read_repo_status(&user, &params, &rooms, &db).await.into()
}

async fn read_repo_status(
    user: &CurrentUser,
    params: &Params,
    rooms: &Rooms,
    db: &Db,
) -> Result<Json<RepoStatus>, ApiError> {
    let id = path_id::<SessionId>(params, "id")?;
    if !sessions::is_owned_by(db, user.id, id).await? {
        return Err(ApiError::SessionNotFound);
    }
    rooms.repo_status(id).await.map(Json)
}

// ── Daemon-scoped routes ──

/// The harness-native session id the daemon announced at start.
#[derive(Debug, Deserialize, skyzen::ToSchema)]
struct HarnessSessionIdentity {
    /// Identity the harness minted; resume reopens this conversation.
    harness_session_id: String,
}

/// Records the harness-native session id so a later resume continues it.
#[skyzen::openapi]
async fn put_harness_session(
    State(session): State<DaemonSession>,
    Json(identity): Json<HarnessSessionIdentity>,
    db: Db,
) -> Outcome<NoContent> {
    sessions::record_harness_session(&db, session.0, &identity.harness_session_id)
        .await
        .map(|()| NoContent)
        .into()
}

/// Raises an approval against the daemon's own session.
///
/// The daemon records the durable approval here *before* it announces the
/// request on the relay, so the id a browser sees is always one the API can
/// decide.
///
/// Exported like the user-facing routes even though it is daemon-scoped:
/// flycod's contract deserves the same typed description the browser
/// client's does, and unlike the transcript and relay routes beside it, this
/// one answers with an ordinary JSON document.
#[skyzen::openapi]
async fn raise_approval(
    State(session): State<DaemonSession>,
    State(config): State<ApiConfig>,
    Json(payload): Json<ApprovalPayload>,
    db: Db,
) -> Outcome<Created<Json<ApprovalView>>> {
    record_approval(session.0, &payload, &db, &config)
        .await
        .into()
}

async fn record_approval(
    session: SessionId,
    payload: &ApprovalPayload,
    db: &Db,
    config: &ApiConfig,
) -> Result<Created<Json<ApprovalView>>, ApiError> {
    let id = approvals::raise(db, session, payload).await?;
    let view = approvals::find_for_session(db, session, id).await?;
    push::notify_approval(db, config, session).await?;
    tracing::info!(%session, "a daemon raised an approval");
    Ok(Created(Json(view)))
}

#[skyzen::openapi]
async fn notify_turn_completed(
    State(session): State<DaemonSession>,
    State(config): State<ApiConfig>,
    db: Db,
) -> Outcome<NoContent> {
    push::notify_turn(&db, &config, session.0, true)
        .await
        .map(|()| NoContent)
        .into()
}

/// Records a provisioning milestone the session's own machine reached.
///
/// The queue announces everything up to the machine existing; everything
/// after it is a fact only the daemon holds. Most of those ride the relay,
/// but the checkout happens *before* the harness exists and therefore before
/// there is a relay socket — so the one stage that cannot be a relay frame
/// gets a route (docs/ux.md §9.2).
///
/// The control plane stamps the time rather than taking the daemon's: a
/// session VM with a wrong clock must not be able to put a line of the
/// timeline in 1970.
#[skyzen::openapi]
async fn report_provisioning_stage(
    State(session): State<DaemonSession>,
    Json(report): Json<ReportProvisioningStage>,
    rooms: Rooms,
) -> Outcome<NoContent> {
    rooms
        .broadcast(
            session.0,
            &ClientEvent::ProvisioningStage {
                stage: report.stage,
                at_unix: crate::clock::now_unix(),
            },
        )
        .await
        .map(|()| NoContent)
        .into()
}

#[skyzen::openapi]
async fn notify_turn_failed(
    State(session): State<DaemonSession>,
    State(config): State<ApiConfig>,
    db: Db,
) -> Outcome<NoContent> {
    push::notify_turn(&db, &config, session.0, false)
        .await
        .map(|()| NoContent)
        .into()
}

/// Records one thing this session's daemon observed about the harness
/// account driving it.
///
/// The LLM usage panel is reactive by necessity — no vendor publishes a
/// remaining-quota API — so this is where its numbers come from: the daemon
/// holds the turn's `UsageReport` and is what receives `UsageLimited`, and
/// neither can be asked for after the fact.
///
/// Which account the observation lands on is derived from the session
/// itself, so a daemon never names one. Answers `204`: the row's identity
/// is of no use to the daemon that posted it.
#[skyzen::openapi]
async fn record_harness_observation(
    State(session): State<DaemonSession>,
    Json(observation): Json<HarnessObservation>,
    db: Db,
) -> Outcome<NoContent> {
    observe(session.0, observation, &db).await.into()
}

async fn observe(
    session: SessionId,
    observation: HarnessObservation,
    db: &Db,
) -> Result<NoContent, ApiError> {
    observations::record(db, session, observation).await?;
    Ok(NoContent)
}

/// Stores one batch of a session's transcript.
async fn put_transcript_batch(
    State(session): State<DaemonSession>,
    params: Params,
    body: Bytes,
    storage: Storage,
) -> Outcome<NoContent> {
    store_batch(session.0, &params, body, &storage).await.into()
}

async fn store_batch(
    session: SessionId,
    params: &Params,
    body: Bytes,
    storage: &Storage,
) -> Result<NoContent, ApiError> {
    let stream = path_segment(params, "stream")?;
    let raw = path_segment(params, "seq")?;
    let seq = raw.parse::<u64>().map_err(|_| ApiError::MalformedId(raw))?;

    transcripts::put_batch(storage, session, &stream, seq, body.to_vec()).await?;
    Ok(NoContent)
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

/// Stores the uncommitted diff of a session about to be archived automatically.
#[skyzen::openapi]
async fn put_workdir_patch(
    State(session): State<DaemonSession>,
    body: Bytes,
    storage: Storage,
) -> Outcome<NoContent> {
    store_workdir_patch(session.0, body, &storage).await.into()
}

async fn store_workdir_patch(
    session: SessionId,
    body: Bytes,
    storage: &Storage,
) -> Result<NoContent, ApiError> {
    workdirs::put(storage, session, body.to_vec()).await?;
    Ok(NoContent)
}

/// Reads a previously stored uncommitted diff, for a resume onto a new host.
#[skyzen::openapi]
async fn get_workdir_patch(
    State(session): State<DaemonSession>,
    storage: Storage,
) -> Outcome<Response> {
    read_workdir_patch(session.0, &storage).await.into()
}

async fn read_workdir_patch(session: SessionId, storage: &Storage) -> Result<Response, ApiError> {
    let Some(body) = workdirs::get(storage, session).await? else {
        return Err(ApiError::SessionNotFound);
    };
    let mut response = Response::new(skyzen::Body::from(body));
    response.headers_mut().insert(
        skyzen::header::CONTENT_TYPE,
        skyzen::header::HeaderValue::from_static(workdirs::CONTENT_TYPE),
    );
    Ok(response)
}

// ── What the agent is allowed to know and to change ──
//
// The daemon's local MCP server is the only sanctioned way an agent touches
// its own machine (docs/ARCHITECTURE.md), and these four routes are what it
// is made of. They are daemon-scoped rather than user-scoped because the
// agent has no user credential and must never be given one: an `fd_` token
// proves which session is calling, and the owner every read is scoped to is
// derived from that session here rather than taken from the caller.

/// Tells a session's agent which machine it is on and who chose it.
#[skyzen::openapi]
async fn get_agent_machine(
    State(session): State<DaemonSession>,
    db: Db,
) -> Outcome<Json<AgentMachineView>> {
    read_agent_machine(session.0, &db).await.map(Json).into()
}

async fn read_agent_machine(session: SessionId, db: &Db) -> Result<AgentMachineView, ApiError> {
    let user = sessions::owner(db, session).await?;
    machines::agent_view(db, user, session).await
}

/// Lists the machine types this session can be resized to, with prices.
///
/// The curated catalog of docs/ux.md §7.6, narrowed to the account and
/// region the session's disk already lives in — the two a resize cannot
/// cross. The agent reads exactly the list the user's own slider shows.
#[skyzen::openapi]
async fn get_agent_machine_catalog(
    State(session): State<DaemonSession>,
    State(config): State<ApiConfig>,
    db: Db,
) -> Outcome<Json<Vec<MachineCatalogEntry>>> {
    read_agent_catalog(session.0, &config, &db)
        .await
        .map(Json)
        .into()
}

async fn read_agent_catalog(
    session: SessionId,
    config: &ApiConfig,
    db: &Db,
) -> Result<Vec<MachineCatalogEntry>, ApiError> {
    let user = sessions::owner(db, session).await?;
    machines::resize_catalog(db, config, user, session).await
}

/// Moves the session onto another machine type, on the agent's own say-so.
///
/// Refused for a type that bills a minimum the moment it boots: that is the
/// user's money committed before anything runs, so the daemon raises an
/// approval instead and the resize happens when the user decides.
#[skyzen::openapi]
async fn agent_resize_machine(
    State(session): State<DaemonSession>,
    State(config): State<ApiConfig>,
    Json(request): Json<ResizeMachine>,
    rooms: Rooms,
    db: Db,
) -> Outcome<Accepted> {
    run_agent_resize(session.0, &request, &config, &rooms, &db)
        .await
        .into()
}

async fn run_agent_resize(
    session: SessionId,
    request: &ResizeMachine,
    config: &ApiConfig,
    rooms: &Rooms,
    db: &Db,
) -> Result<Accepted, ApiError> {
    let user = sessions::owner(db, session).await?;
    machines::resize_for_agent(db, config, rooms, user, session, &request.machine_type).await?;
    Ok(Accepted)
}

/// Tells a session's agent what it has spent and what is left.
#[skyzen::openapi]
async fn get_agent_budget(
    State(session): State<DaemonSession>,
    db: Db,
) -> Outcome<Json<BudgetView>> {
    read_agent_budget(session.0, &db).await.map(Json).into()
}

async fn read_agent_budget(session: SessionId, db: &Db) -> Result<BudgetView, ApiError> {
    let user = sessions::owner(db, session).await?;
    Ok(sessions::find(db, user, session).await?.budget)
}

/// Routes that anyone may call.
///
/// Three of them are public because they cannot be anything else: a browser
/// returning from a harness vendor carries no flyco credential, a browser
/// needs the VAPID public key *before* it can subscribe, and GitHub signs
/// its webhook deliveries rather than presenting a bearer token. Each says
/// in its own module how it establishes who is calling.
fn public_routes() -> Vec<RouteNode> {
    let mut nodes = Route::new((
        "/v1/healthz".at(healthz),
        "/install/{artifact}".at(get_release_artifact),
        "/v1/auth/github".route(("/start".post(oauth::start), "/callback".at(oauth::callback))),
    ))
    .into_route_nodes();
    nodes.extend(push::public_routes());
    nodes.extend(webhooks::routes());
    nodes
}

/// The two relay upgrades.
///
/// Neither carries a user credential — one presents a session's daemon
/// token, the other a single-use ticket — so they authenticate themselves
/// rather than sitting behind [`RequireAuth`]. Their success is a `101`
/// with a socket attached, which the `OpenAPI` response model cannot
/// describe, so they export a path and nothing about what comes back —
/// see [`responses::UNDECLARED`](crate::responses::UNDECLARED).
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
        "/v1/sessions/{id}/harness-session".put(put_harness_session),
        "/v1/sessions/{id}/harness-observations".post(record_harness_observation),
        "/v1/sessions/{id}/turn-completed".post(notify_turn_completed),
        "/v1/sessions/{id}/turn-failed".post(notify_turn_failed),
        "/v1/sessions/{id}/provisioning-stage".post(report_provisioning_stage),
        "/v1/sessions/{id}/transcript/{stream}".at(get_transcript),
        "/v1/sessions/{id}/transcript/{stream}/batches/{seq}".put(put_transcript_batch),
        "/v1/sessions/{id}/workdir-patch"
            .at(get_workdir_patch)
            .put(put_workdir_patch),
        "/v1/sessions/{id}/agent/machine".at(get_agent_machine),
        "/v1/sessions/{id}/agent/machine/catalog".at(get_agent_machine_catalog),
        "/v1/sessions/{id}/agent/machine/resize".post(agent_resize_machine),
        "/v1/sessions/{id}/agent/budget".at(get_agent_budget),
    ))
    .middleware(RequireDaemon::new())
    .into_route_nodes()
}

/// The caller's own account, keys, and approvals.
fn account_routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/me".at(me).patch(update_me),
        "/v1/harness-features".at(list_harness_features),
        "/v1/api-keys".post(create_api_key).get(list_api_keys),
        "/v1/api-keys/{id}".delete(revoke_api_key),
        "/v1/approvals".at(list_approvals),
        "/v1/approvals/{id}/decision".post(decide_approval),
    ))
    .into_route_nodes()
}

/// A session's lifecycle, and driving the agent inside it.
fn session_routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/sessions".post(create_session).get(list_sessions),
        "/v1/sessions/{id}".at(get_session).patch(update_session),
        "/v1/sessions/{id}/archive".post(archive_session),
        "/v1/sessions/{id}/budget".at(get_session_budget),
        "/v1/sessions/{id}/daemon-token".post(create_daemon_token),
        "/v1/sessions/{id}/relay-ticket".post(create_relay_ticket),
        "/v1/sessions/{id}/events".at(get_session_events),
        "/v1/sessions/{id}/messages".post(send_message),
        "/v1/sessions/{id}/interrupt".post(interrupt_session),
        "/v1/sessions/{id}/compact".post(compact_session),
        "/v1/sessions/{id}/resume".post(resume_session),
        "/v1/sessions/{id}/turns".at(list_turns),
        "/v1/sessions/{id}/env"
            .at(get_session_env)
            .put(put_session_env),
        "/v1/sessions/{id}/repo-status".at(get_repo_status),
    ))
    .into_route_nodes()
}

/// Routes that require a bearer credential.
///
/// One middleware for the whole set: every route below answers to a
/// `CurrentUser` and to nothing else, so authentication is applied once
/// here rather than per domain, where a module could forget it.
fn authenticated_routes() -> Vec<RouteNode> {
    let mut nodes = account_routes();
    nodes.extend(session_routes());
    nodes.extend(agents_md::routes());
    nodes.extend(claude_oauth::routes());
    nodes.extend(codex_oauth::routes());
    nodes.extend(harness_accounts::routes());
    nodes.extend(machines::routes());
    nodes.extend(mcp::routes());
    nodes.extend(memory::routes());
    nodes.extend(provider_accounts::routes());
    nodes.extend(push::routes());
    nodes.extend(repos::routes());
    nodes.extend(skills::routes());
    Route::new(nodes)
        .middleware(RequireAuth::new(FlycoAuthenticator::new()))
        .into_route_nodes()
}

/// The complete route tree, before state or middleware is attached.
///
/// Separated from [`router`] so the `OpenAPI` export can describe the API
/// without opening a database or reading any configuration.
fn routes() -> Route {
    let mut nodes = public_routes();
    nodes.extend(relay_routes());
    nodes.extend(daemon_routes());
    nodes.extend(authenticated_routes());
    Route::new(nodes)
}

/// The PWA, compiled into the Worker so the control plane and the UI share
/// an origin. Cloudflare Assets is not a skyzen capability; the files ride
/// the wasm binary instead.
fn frontend() -> EmbeddedStaticDir {
    static ASSETS: skyzen::include_dir::Dir<'static> =
        skyzen::embed_dir!("$CARGO_MANIFEST_DIR/../../frontend/dist");
    EmbeddedStaticDir::new("/", &ASSETS)
        .index_file("index.html")
        .spa()
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
///
/// # Panics
///
/// Panics when that metadata was compiled out, because a document with no
/// paths in it would be checked in as if it described the API.
#[must_use]
pub fn openapi_document() -> utoipa::openapi::OpenApi {
    let collected = routes().openapi();
    assert!(
        collected.is_enabled(),
        "OpenAPI collection is compiled out; build this in debug on a native target"
    );

    let mut spec = collected.to_utoipa_spec();
    // Skyzen stamps its own crate name and version into `info`; the document
    // describes flyco's API, whose version is the `/v1` prefix. Using the
    // crate version instead would churn the checked-in file on every release.
    spec.info = utoipa::openapi::Info::new("Flyco control plane", "v1");
    responses::finish(&mut spec);
    spec
}

/// Builds the control-plane router around an explicit configuration, vendor
/// clients, database, and provisioning queue.
///
/// All of them are injected rather than discovered, so tests can drive the
/// OAuth flows without reaching `github.com`, `console.anthropic.com`, or a
/// real D1, and can read back the jobs a session creation enqueued. In the
/// deployed control plane the database and the queue arrive the way the KV
/// namespace and the R2 bucket do — they are declared in `Skyzen.toml`, and
/// `#[skyzen::main]` wraps the router with them — which is what
/// [`router_from_environment`] builds instead.
#[must_use]
pub fn router(
    config: ApiConfig,
    github: GithubClient,
    vendors: Vendors,
    db: Db,
    queue: Queue,
) -> Router {
    configured(config, github, vendors)
        .with(db)
        .with(queue)
        .build()
}

/// The router without the database and queue the declared `[[database]]`
/// and `[[service]]` entries supply.
///
/// The two vendor clients are injected separately as well as together: a
/// handler that only redeems Anthropic's grant asks for
/// [`ClaudeClient`], the Codex routes ask for [`CodexClient`], and the one
/// place that may renew either — the provisioning consumer — asks for
/// [`Vendors`].
fn configured(config: ApiConfig, github: GithubClient, vendors: Vendors) -> Route {
    with_error_handling(
        with_rooms(Route::new((routes(), frontend())))
            .with(State(config))
            .with(State(github))
            .with(State(vendors.claude.clone()))
            .with(State(vendors.codex.clone()))
            .with(State(vendors)),
    )
}

/// Worker path: configuration is read from the request's `env`, not at
/// isolate startup. See [`crate::middleware::LoadApiConfig`].
#[cfg(target_arch = "wasm32")]
fn configured_from_request(github: GithubClient, vendors: Vendors) -> Route {
    with_error_handling(
        with_rooms(Route::new((routes(), frontend())))
            .with(crate::middleware::LoadApiConfig)
            .with(State(github))
            .with(State(vendors.claude.clone()))
            .with(State(vendors.codex.clone()))
            .with(State(vendors)),
    )
}

fn with_error_handling(route: Route) -> Route {
    // Outermost, so extractor and routing failures answer in the same
    // shape flyco's own errors do.
    route.with(ErrorHandlingMiddleware::new(
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
}

/// Builds the router the deployed control plane runs.
///
/// # Panics
///
/// On native targets, panics if any required configuration binding is
/// missing or malformed — a misconfigured control plane must fail at
/// startup, not at the first sign-in attempt. On the Worker the `env` is
/// not published during router construction, so missing bindings fail the
/// request that needs them instead. The database is the manifest's to
/// open, and its own failure to resolve panics there for the same reason.
#[must_use]
pub fn router_from_environment() -> Router {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let config = ApiConfig::from_environment()
            .unwrap_or_else(|error| panic!("flyco control plane is misconfigured: {error}"));
        configured(config, GithubClient::default(), Vendors::default()).build()
    }
    #[cfg(target_arch = "wasm32")]
    {
        configured_from_request(GithubClient::default(), Vendors::default()).build()
    }
}

#[cfg(test)]
mod tests {
    use flyco_core::{ApiKeySummary, CreateApiKey, CreatedApiKey, CurrentUser, Problem};
    use skyzen_services::{Db, Kv, Queue};
    use skyzen_test::TestContext;

    use crate::testing::{GITHUB_LOGIN, migrated_router, seed_user, test_router};
    use crate::{api_keys, session};

    #[skyzen::test]
    async fn healthz_reports_protocol_version(ctx: TestContext, db: Db, queue: Queue) {
        let client = ctx.client(test_router(db, queue));
        let response = client.get("/v1/healthz").send().await;
        response.assert_status(200);
        let body: serde_json::Value = response.json();
        assert_eq!(
            body["wire_protocol_version"],
            u64::from(flyco_core::WIRE_PROTOCOL_VERSION)
        );
    }

    #[skyzen::test]
    async fn the_spa_is_served_at_the_root(ctx: TestContext, db: Db, queue: Queue) {
        let client = ctx.client(test_router(db, queue));

        let root = client.get("/").send().await;
        root.assert_status(200);
        assert!(
            root.body_text().contains(r#"id="app""#),
            "the root document must mount the PWA"
        );

        let login = client.get("/login").send().await;
        login.assert_status(200);
        assert_eq!(login.body_text(), root.body_text());
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
        let stored: String = skyzen::sql!(
            db,
            "SELECT token_hash FROM api_keys WHERE id = {created.id}"
        )
        .fetch_scalar()
        .await
        .expect("read the stored key");
        assert_eq!(stored, crate::crypto::token_hash(&created.token));
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
