//! The `sessions` table.
//!
//! Ownership is enforced in the `WHERE` clause of every read: a session
//! belonging to somebody else is indistinguishable from one that does not
//! exist, so the API never confirms that an id is real to a caller who has
//! no business knowing.

use flyco_core::{
    ARCHIVE_AFTER_IDLE_SECS, ARCHIVE_FINISHED_AFTER_IDLE_SECS, ApprovalState, BranchName,
    BudgetConfig, BudgetId, BudgetStage, ClientEvent, HarnessKind, HarnessSessionView,
    InterruptedReason, KEEP_AWAKE_MAX_MINUTES, MAX_SESSION_TITLE_CHARS, MachineOrigin, ModelChoice,
    PROVISION_DEADLINE_SECS, PausedReason, PermissionMode, RepoAddedBy, RepoSlug,
    SUSPEND_AFTER_IDLE_SECS, SessionActivity, SessionDetail, SessionId, SessionRepo, SessionState,
    SessionSummary, UsageLimitPause, Usd, UserId, builtin_models,
};
use skyzen::sql;
use skyzen_services::Db;

use crate::budgets;
use crate::clock::now_unix;
use crate::error::ApiError;
use crate::machines;
use crate::rooms::Rooms;
use crate::session_repos;

/// The repositories a session checks out, packed into the session row.
///
/// A session's rows are one-to-many with `session_repos`, and the shape
/// the API serves is a summary carrying the whole set — so the queries
/// read them as a `json_group_array` document rather than joining rows
/// that would multiply every other column. Parsed in [`repos_of`], which
/// is where a row whose JSON does not describe repositories is reported
/// as corrupt rather than silently answered with none.
const REPOS_JSON: &str = "COALESCE(\
    (SELECT json_group_array(json_object(\
        'slug', r.slug, 'branch', r.branch, 'dir', r.dir, 'added_by', r.added_by)) \
     FROM (SELECT slug, branch, dir, added_by FROM session_repos \
           WHERE session_id = s.id ORDER BY position) r), \
    '[]') AS repos_json";

/// The session a caller may still archive, and its budget.
#[derive(Debug, skyzen::FromRow)]
struct SessionRow {
    id: SessionId,
    title: String,
    harness: HarnessKind,
    /// Every `session_repos` row for this session as one JSON array, in
    /// `position` order. Read by [`repos_of`].
    repos_json: String,
    state: SessionState,
    activity: SessionActivity,
    /// Whether an approval raised against this session is still undecided.
    ///
    /// Read from `approvals` on every projection rather than mirrored onto
    /// the session row: that table is where a decision is made durable, and
    /// a copy here would be a second answer free to disagree with it.
    approval_pending: bool,
    machine_origin: MachineOrigin,
    budget_id: BudgetId,
    failure_reason: Option<String>,
    interrupted_reason: Option<InterruptedReason>,
    paused_reason: Option<PausedReason>,
    /// The four columns of a usage-limit pause; see migration 0024.
    ///
    /// Read together because they are one fact — [`UsageLimitPause`] — and
    /// assembled into it by [`usage_limit_of`], which is also where the
    /// invariant that they are present exactly for a usage-limit pause is
    /// enforced rather than assumed.
    usage_limit_window: Option<String>,
    usage_limit_resets_at_unix: Option<u64>,
    usage_limit_resume_at_unix: Option<u64>,
    usage_limit_queued_message: Option<String>,
    created_at_unix: u64,
    last_active_unix: u64,
    /// The model the session was put on, or `NULL` for one opened before
    /// flyco recorded a model at all.
    model: Option<String>,
    /// The effort it runs at. `NULL` both for a legacy row and for a
    /// session that chose a model and left the harness's own effort alone.
    effort: Option<String>,
    /// The permission mode the session's agent runs under. `NULL` for a
    /// row opened before flyco recorded one — read through [`mode_of`],
    /// which resolves it the way [`model_of`] resolves a legacy model.
    permission_mode: Option<PermissionMode>,
    /// Whether the session may have a screen.
    ///
    /// `NOT NULL DEFAULT 0` since migration 0027, so no resolution: a row
    /// that predates the column was a session without a screen, which is
    /// what the default already says.
    computer_use: bool,
    /// Until when the idle sweep must leave this session's machine alone.
    ///
    /// `NULL` for the ordinary clock; see migration 0037.
    awake_until_unix: Option<u64>,
}

/// The choice a stored row names, resolving a legacy `NULL` model.
///
/// A row written before migration 0021 ran on whatever its harness
/// defaults to, so that is what it reads back as — today's default of that
/// harness, resolved here rather than backfilled once into the table, where
/// it would have frozen one afternoon's answer into rows nobody chose it
/// for.
fn model_of(harness: HarnessKind, model: Option<String>, effort: Option<String>) -> ModelChoice {
    model.map_or_else(
        || ModelChoice::default_of(&builtin_models(harness)),
        |model| ModelChoice { model, effort },
    )
}

/// The mode a stored row runs under, resolving a legacy `NULL`.
///
/// A row written before migration 0025 ran on the product default — that
/// is what it was provisioned with — so that is what it reads back as,
/// resolved here rather than backfilled once into the table, where it
/// would have frozen today's default into rows nobody chose it for.
fn mode_of(mode: Option<PermissionMode>) -> PermissionMode {
    mode.unwrap_or(PermissionMode::PRODUCT_DEFAULT)
}

/// The repositories a stored session checks out, as the summary carries
/// them.
///
/// A row whose `repos_json` does not decode is corrupt — nothing writes
/// the column but `session_repos` itself — and a summary built without the
/// set would tell the header a session has no repository at all, which is
/// a worse answer than an error.
fn repos_of(row: &SessionRow) -> Result<Vec<SessionRepo>, ApiError> {
    serde_json::from_str(&row.repos_json)
        .map_err(|_| ApiError::CorruptRecord("session_repos did not read back as repositories"))
}

impl SessionRow {
    /// The summary a stored row projects to, repositories included.
    fn into_summary(self) -> Result<SessionSummary, ApiError> {
        let repos = repos_of(&self)?;
        Ok(SessionSummary {
            id: self.id,
            title: self.title,
            harness: self.harness,
            repos,
            state: self.state,
            // An undecided approval blocks the agent whatever the last turn
            // event said, so it is applied here, once, where every reader
            // of a session goes through.
            activity: self.activity.with_pending_approval(self.approval_pending),
            machine_origin: self.machine_origin,
            interrupted_reason: self.interrupted_reason,
            paused_reason: self.paused_reason,
            created_at_unix: self.created_at_unix,
            last_active_unix: self.last_active_unix,
            model: model_of(self.harness, self.model, self.effort),
            permission_mode: mode_of(self.permission_mode),
            computer_use: self.computer_use,
            // A hold that has run out is no hold at all, and a row that
            // still carries yesterday's instant would put "kept awake" in
            // the menu of a machine the sweep is free to stop. The column
            // is cleared where it is read rather than swept.
            awake_until_unix: self
                .awake_until_unix
                .filter(|until| *until > crate::clock::now_unix()),
        })
    }
}

async fn detail_from(db: &Db, row: SessionRow) -> Result<SessionDetail, ApiError> {
    let budget = budgets::view(db, row.budget_id).await?;
    // The column outlives the state it explains — a retried session keeps the
    // sentence from the attempt before it until the next attempt clears it —
    // so the reason is reported only while the session is actually failed.
    let failure = (row.state == SessionState::Failed)
        .then(|| row.failure_reason.clone())
        .flatten();
    let usage_limit = usage_limit_of(&row)?;
    Ok(SessionDetail {
        summary: row.into_summary()?,
        budget,
        failure,
        usage_limit,
    })
}

/// The wait a usage-limit pause is, as the session page reads it.
///
/// `None` for every session that is not waiting on one, which is decided by
/// [`SessionRow::paused_reason`] and not by whether the columns happen to
/// hold something: they outlive the pause until the continuation clears
/// them, and reading them without the reason would keep a finished wait on
/// the page.
///
/// A row whose reason says `usage_limit` and whose window or reset is
/// missing is corrupt rather than a pause with less detail — nothing writes
/// one without both, and answering with a partial wait would put a countdown
/// to nowhere in front of the user — so it is reported as the bug it is.
fn usage_limit_of(row: &SessionRow) -> Result<Option<UsageLimitPause>, ApiError> {
    if row.paused_reason != Some(PausedReason::UsageLimit) {
        return Ok(None);
    }
    let (Some(window), Some(resets_at_unix)) = (
        row.usage_limit_window.clone(),
        row.usage_limit_resets_at_unix,
    ) else {
        return Err(ApiError::CorruptRecord(
            "a session paused on a usage limit names no window or no reset time",
        ));
    };
    Ok(Some(UsageLimitPause {
        window,
        resets_at_unix,
        resume_at_unix: row.usage_limit_resume_at_unix,
        queued_message: row.usage_limit_queued_message.clone(),
    }))
}

/// How many sessions the user currently holds that still occupy their cap.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn live_count(db: &Db, user: UserId) -> Result<u32, ApiError> {
    // A session that holds no execution environment occupies no slot: an
    // archived one released it, and a failed one never got one. Counting
    // either would let a run of failed provisions lock a user out of their
    // own account.
    let archived = SessionState::Archived;
    let failed = SessionState::Failed;
    Ok(sql!(
        db,
        "SELECT COUNT(*) AS live FROM sessions \
         WHERE user_id = {user} AND state != {archived} AND state != {failed}"
    )
    .fetch_scalar()
    .await?)
}

/// How many of the user's sessions still run on `harness`.
///
/// What `DELETE /v1/harness-accounts/{id}` refuses on. Archived is the only
/// state excluded, and deliberately so: an interrupted or failed session is
/// one the user can still resume, and it would resume onto a harness with
/// no credential left to renew. Only a session that has released its
/// machine for good has finished with the account.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn live_on_harness(db: &Db, user: UserId, harness: HarnessKind) -> Result<u32, ApiError> {
    let archived = SessionState::Archived;
    Ok(sql!(
        db,
        "SELECT COUNT(*) AS live FROM sessions \
         WHERE user_id = {user} AND harness = {harness} AND state != {archived}"
    )
    .fetch_scalar()
    .await?)
}

/// One repository a session opens with.
///
/// The branch is the concrete name [`crate::repos`] resolved — the caller's
/// or the repository's default — never the `None` the request carries:
/// what a checkout is on is recorded once, here, and the provisioning
/// queue reads it rather than asking GitHub again.
#[derive(Debug, Clone)]
pub struct RepoOpening {
    /// Repository it works in.
    pub slug: RepoSlug,
    /// Branch it works on, already resolved.
    pub branch: BranchName,
}

/// Everything `POST /v1/sessions` decided before a row could be written.
///
/// One argument rather than six positional ones: `harness`, `repos`, and
/// `title` are all things a session is opened with, and a call site that
/// swaps two of them would still compile.
#[derive(Debug, Clone)]
pub struct Opening<'a> {
    /// Whose session it is.
    pub user: UserId,
    /// What to call it — the excerpt of the prompt that opened it.
    pub title: &'a str,
    /// Which coding harness drives it.
    pub harness: HarnessKind,
    /// Repositories it works in, in the order the caller chose them. The
    /// first is the primary the session header names; at least one is
    /// required — a session that checks out nothing has nothing to work in.
    pub repos: &'a [RepoOpening],
    /// Whether flyco or the caller chose the machine.
    pub machine_origin: MachineOrigin,
    /// What it may spend.
    pub budget: BudgetConfig,
    /// What it runs on, already resolved against the harness account's own
    /// model list — so a row is never written naming a model nobody
    /// checked.
    pub model: &'a ModelChoice,
    /// The approval policy it opens under — `None` leaves the column
    /// `NULL`, which [`mode_of`] reads as the product default, so an
    /// unopinionated create stores no opinion.
    pub permission_mode: Option<PermissionMode>,
}

/// Creates a session and the budget it accounts against.
///
/// The session starts in [`SessionState::Provisioning`] and stays there
/// until its daemon reaches the control plane. The machine it will run on is
/// reserved by [`crate::machines::reserve`] and built by the provisioning
/// queue; this function writes neither.
///
/// D1 has no transactions, so the caller's cap is checked in a separate
/// query first. Two simultaneous creates can therefore both pass a check at
/// the boundary; the cap is a spend guard, not a security boundary, and the
/// next read reports the true count.
///
/// # Errors
///
/// Returns [`ApiError::SessionCapReached`] when the caller is at their cap,
/// or a database error otherwise.
pub async fn create(db: &Db, cap: u32, opening: Opening<'_>) -> Result<SessionDetail, ApiError> {
    let user = opening.user;
    let live = live_count(db, user).await?;
    if live >= cap {
        return Err(ApiError::SessionCapReached { cap });
    }

    let id = SessionId::generate();
    let budget_id = budgets::create(db, id, opening.budget).await?;
    let now = now_unix();
    let Opening {
        title,
        harness,
        repos,
        machine_origin,
        model,
        permission_mode,
        ..
    } = opening;
    let effort = model.effort.as_deref();
    let model = model.model.as_str();
    // Every machine carries the display stack — the image ships it and a
    // screen is not a create-time choice. `UpdateSession::computer_use`
    // flips the row for a session that wants it gone.
    let computer_use = true;

    sql!(
        db,
        "INSERT INTO sessions \
         (id, user_id, title, harness, state, machine_origin, budget_id, \
          created_at_unix, last_active_unix, model, effort, permission_mode, computer_use) \
         VALUES ({id}, {user}, {title}, {harness}, \
                 {SessionState::Provisioning}, {machine_origin}, {budget_id}, {now}, {now}, \
                 {model}, {effort}, {permission_mode}, {computer_use})"
    )
    .execute()
    .await?;

    // Each repository gets its row through `attach` so the directory it is
    // checked out under is chosen against the same set the write sees —
    // `owner--name` when the repository's own name is already taken — and
    // a request that names one twice is refused rather than doubled.
    for repo in repos {
        session_repos::attach(db, id, &repo.slug, &repo.branch, RepoAddedBy::User).await?;
    }

    find(db, user, id).await
}

/// Renames one of the caller's sessions.
///
/// The title is the only thing about a session the user names directly, so
/// it is validated here rather than at the boundary: an empty title would
/// leave a list row identified by a UUID, and an unbounded one would push
/// everything else out of it.
///
/// # Errors
///
/// Returns [`ApiError::InvalidTitle`] if the trimmed title is empty or
/// longer than [`MAX_SESSION_TITLE_CHARS`], or
/// [`ApiError::SessionNotFound`] if the session is not the caller's.
pub async fn rename(
    db: &Db,
    user: UserId,
    id: SessionId,
    title: &str,
) -> Result<SessionDetail, ApiError> {
    let trimmed = title.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_SESSION_TITLE_CHARS {
        return Err(ApiError::InvalidTitle {
            max: MAX_SESSION_TITLE_CHARS,
        });
    }
    // The row is loaded first so a session that is not the caller's is a
    // 404 rather than an UPDATE that quietly writes nothing.
    load(db, user, id).await?;

    let title = trimmed.to_owned();
    sql!(
        db,
        "UPDATE sessions SET title = {title} WHERE id = {id} AND user_id = {user}"
    )
    .execute()
    .await?;

    find(db, user, id).await
}

/// Puts one of the caller's sessions on another model.
///
/// The durable half alone: the running harness is told separately, by the
/// [`SetModel`](flyco_core::ControlToDaemon::SetModel) the caller sends the
/// session's room once this has returned. Written in that order because the
/// room is the live announcement and the row is what a daemon reads when it
/// comes back — a harness told first would, for the window between the two,
/// be running a model the control plane does not know about.
///
/// The choice is validated against the account's model list by the caller,
/// not here: this module writes sessions and has no business fetching a
/// harness account.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is not the
/// caller's, or [`ApiError`] if the database fails.
pub async fn set_model(
    db: &Db,
    user: UserId,
    id: SessionId,
    choice: &ModelChoice,
) -> Result<SessionDetail, ApiError> {
    // The row is loaded first so a session that is not the caller's is a
    // 404 rather than an UPDATE that quietly writes nothing.
    load(db, user, id).await?;

    let model = choice.model.as_str();
    let effort = choice.effort.as_deref();
    sql!(
        db,
        "UPDATE sessions SET model = {model}, effort = {effort} \
         WHERE id = {id} AND user_id = {user}"
    )
    .execute()
    .await?;

    find(db, user, id).await
}

/// Puts one of the caller's sessions under another permission mode.
///
/// The durable half alone, on the same terms as [`set_model`]: the running
/// harness is told separately, by the
/// [`SetPermissionMode`](flyco_core::ControlToDaemon::SetPermissionMode)
/// the caller sends the session's room once this has returned. Written in
/// that order because the room is the live announcement and the row is
/// what a daemon reads when it comes back.
///
/// Every declared [`PermissionMode`] is one both harnesses honor, so there
/// is no account-list validation the way a model has: the enum is the
/// contract, and a body that names one the type does not have is refused
/// at the boundary by serde rather than here.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is not the
/// caller's, or [`ApiError`] if the database fails.
pub async fn set_mode(
    db: &Db,
    user: UserId,
    id: SessionId,
    mode: PermissionMode,
) -> Result<SessionDetail, ApiError> {
    // The row is loaded first so a session that is not the caller's is a
    // 404 rather than an UPDATE that quietly writes nothing.
    load(db, user, id).await?;

    let mode = Some(mode);
    sql!(
        db,
        "UPDATE sessions SET permission_mode = {mode} \
         WHERE id = {id} AND user_id = {user}"
    )
    .execute()
    .await?;

    find(db, user, id).await
}

/// Holds one of the caller's sessions awake, or lets the idle sweep have
/// it back.
///
/// `minutes` is how much longer the machine must not be stopped for
/// idleness; `None` ends the hold now. The instant is computed here rather
/// than sent by the browser, so a hold is as long as it was asked for
/// whatever the clock on the asking machine says.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is not the
/// caller's, [`ApiError::KeepAwakeTooLong`] if the hold is longer than
/// [`KEEP_AWAKE_MAX_MINUTES`], or [`ApiError`] if the database fails.
pub async fn keep_awake(
    db: &Db,
    user: UserId,
    id: SessionId,
    minutes: Option<u32>,
) -> Result<SessionDetail, ApiError> {
    // The row is loaded first so a session that is not the caller's is a
    // 404 rather than an UPDATE that quietly writes nothing.
    load(db, user, id).await?;

    let until = match minutes {
        Some(minutes) if minutes > KEEP_AWAKE_MAX_MINUTES => {
            return Err(ApiError::KeepAwakeTooLong {
                maximum_minutes: KEEP_AWAKE_MAX_MINUTES,
            });
        }
        Some(minutes) => Some(now_unix().saturating_add(u64::from(minutes) * 60)),
        None => None,
    };
    sql!(
        db,
        "UPDATE sessions SET awake_until_unix = {until} WHERE id = {id}"
    )
    .execute()
    .await?;
    tracing::info!(session = %id, ?until, "the machine is held awake");

    find(db, user, id).await
}

/// Gives one of the caller's sessions a screen, or takes it away.
///
/// The durable half alone, on the same terms as [`set_mode`]: the running
/// daemon is told separately, by the
/// [`SetComputerUse`](flyco_core::ControlToDaemon::SetComputerUse) the
/// caller sends the session's room once this has returned — and held for
/// one that is away, `survives_a_disconnect`, because it is what the next
/// machine comes up with.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is not the
/// caller's, or [`ApiError`] if the database fails.
pub async fn set_computer_use(
    db: &Db,
    user: UserId,
    id: SessionId,
    enabled: bool,
) -> Result<SessionDetail, ApiError> {
    // The row is loaded first so a session that is not the caller's is a
    // 404 rather than an UPDATE that quietly writes nothing.
    load(db, user, id).await?;

    sql!(
        db,
        "UPDATE sessions SET computer_use = {enabled} \
         WHERE id = {id} AND user_id = {user}"
    )
    .execute()
    .await?;

    find(db, user, id).await
}

/// What changing a session's budget did to the session.
#[derive(Debug)]
pub struct BudgetRaise {
    /// The session as it now stands.
    pub session: SessionDetail,
    /// Whether the new limit released a session paused on the old one.
    ///
    /// The daemon holds a pause of its own — it stops accepting work the
    /// moment [`BudgetSignal::Pause`](flyco_core::BudgetSignal::Pause)
    /// reaches it — and nothing in the database can lift that. So this is
    /// what tells the caller a
    /// [`ControlToDaemon::BudgetRaised`](flyco_core::ControlToDaemon::BudgetRaised)
    /// is owed to the session's room.
    pub resumed: bool,
}

/// Changes what one of the caller's sessions may spend, releasing it if it
/// was paused on the old limit.
///
/// The ledger is untouched: the limit is re-read against the same history,
/// and a session paused because that history exhausted the old limit is put
/// back to [`SessionState::Active`] exactly when the replay no longer says
/// [`BudgetStage::Exhausted`](flyco_core::BudgetStage::Exhausted). A limit
/// raised to less than the session has already spent is accepted and leaves
/// it paused, because it still is.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is not the
/// caller's, [`ApiError::InvalidBudget`] if the limit is zero, or
/// [`ApiError::InvalidTransition`] if the lifecycle refuses the release.
pub async fn set_budget_limit(
    db: &Db,
    user: UserId,
    id: SessionId,
    limit: Usd,
) -> Result<BudgetRaise, ApiError> {
    // The row is loaded first so a session that is not the caller's is a
    // 404 rather than a budget somebody else's session accounts against.
    let row = load(db, user, id).await?;
    let budget = budgets::set_limit(db, row.budget_id, limit).await?;

    if row.state == SessionState::Paused && budget.stage != BudgetStage::Exhausted {
        let session = transition(db, user, id, SessionState::Active).await?;
        return Ok(BudgetRaise {
            session,
            resumed: true,
        });
    }

    Ok(BudgetRaise {
        session: find(db, user, id).await?,
        resumed: false,
    })
}

/// Lists the caller's sessions, newest first.
///
/// The statement is built rather than literal because [`REPOS_JSON`] — the
/// `session_repos` document the summary is served with — is written once
/// and shared with [`load`].
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails or a stored row is malformed.
pub async fn list(db: &Db, user: UserId) -> Result<Vec<SessionSummary>, ApiError> {
    let statement = format!(
        "SELECT s.id, s.title, s.harness, {REPOS_JSON}, s.state, s.activity, \
         EXISTS (SELECT 1 FROM approvals a WHERE a.session_id = s.id AND a.state = ?) \
         AS approval_pending, \
         s.machine_origin, s.budget_id, s.failure_reason, s.interrupted_reason, \
         s.paused_reason, s.usage_limit_window, s.usage_limit_resets_at_unix, \
         s.usage_limit_resume_at_unix, s.usage_limit_queued_message, \
         s.created_at_unix, s.last_active_unix, s.model, s.effort, s.permission_mode, \
         s.computer_use, s.awake_until_unix \
         FROM sessions s WHERE s.user_id = ? \
         ORDER BY s.created_at_unix DESC, s.id DESC"
    );
    let rows: Vec<SessionRow> = db
        .query(&statement)
        .bind(ApprovalState::Pending)
        .bind(user)
        .fetch_all()
        .await?;

    rows.into_iter().map(SessionRow::into_summary).collect()
}

/// Loads one of the caller's sessions.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session does not exist or
/// belongs to somebody else.
pub async fn find(db: &Db, user: UserId, id: SessionId) -> Result<SessionDetail, ApiError> {
    let row = load(db, user, id).await?;
    detail_from(db, row).await
}

/// Moves a session to a new lifecycle state.
///
/// The move is validated by [`SessionState::transition`], so the table can
/// never record a state the domain model does not allow.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is not the caller's,
/// or [`ApiError::InvalidTransition`] if the lifecycle forbids the move.
pub async fn transition(
    db: &Db,
    user: UserId,
    id: SessionId,
    to: SessionState,
) -> Result<SessionDetail, ApiError> {
    let row = load(db, user, id).await?;
    let next = row
        .state
        .transition(to)
        .map_err(|error| ApiError::InvalidTransition {
            from: error.from,
            to: error.to,
        })?;

    sql!(
        db,
        "UPDATE sessions SET state = {next}, last_active_unix = {now_unix()} \
         WHERE id = {id} AND user_id = {user}"
    )
    .execute()
    .await?;

    find(db, user, id).await
}

/// Whether `user` owns `session`, used to scope approvals.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn is_owned_by(db: &Db, user: UserId, session: SessionId) -> Result<bool, ApiError> {
    let owned: u32 = sql!(
        db,
        "SELECT COUNT(*) AS live FROM sessions WHERE id = {session} AND user_id = {user}"
    )
    .fetch_scalar()
    .await?;
    Ok(owned > 0)
}

/// The lifecycle state of one of the caller's sessions.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session does not exist or
/// belongs to somebody else.
pub async fn state_of(db: &Db, user: UserId, id: SessionId) -> Result<SessionState, ApiError> {
    Ok(load(db, user, id).await?.state)
}

/// Refuses unless the session is running.
///
/// What every route that *drives* a session checks first. A provisioning
/// session has no daemon to hear the command, a paused one is stopped on
/// purpose, and an archived one has no machine at all — so the refusal names
/// the state rather than letting the command disappear into a room nobody is
/// listening to.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is not the caller's,
/// or [`ApiError::SessionNotActive`] if it is not [`SessionState::Active`].
pub async fn require_active(db: &Db, user: UserId, id: SessionId) -> Result<(), ApiError> {
    let state = state_of(db, user, id).await?;
    if state == SessionState::Active {
        Ok(())
    } else {
        Err(ApiError::SessionNotActive { state })
    }
}

async fn load(db: &Db, user: UserId, id: SessionId) -> Result<SessionRow, ApiError> {
    let statement = format!(
        "SELECT s.id, s.title, s.harness, {REPOS_JSON}, s.state, s.activity, \
         EXISTS (SELECT 1 FROM approvals a WHERE a.session_id = s.id AND a.state = ?) \
         AS approval_pending, \
         s.machine_origin, s.budget_id, s.failure_reason, s.interrupted_reason, \
         s.paused_reason, s.usage_limit_window, s.usage_limit_resets_at_unix, \
         s.usage_limit_resume_at_unix, s.usage_limit_queued_message, \
         s.created_at_unix, s.last_active_unix, s.model, s.effort, s.permission_mode, \
         s.computer_use, s.awake_until_unix \
         FROM sessions s WHERE s.id = ? AND s.user_id = ?"
    );
    db.query(&statement)
        .bind(ApprovalState::Pending)
        .bind(id)
        .bind(user)
        .fetch_optional()
        .await?
        .ok_or(ApiError::SessionNotFound)
}

// ── The provisioning queue's own reads and writes ──
//
// A queue job carries no user, and it does not need one: the job was
// enqueued by a handler that had already proved the caller owns the session,
// so re-scoping these statements by `user_id` would only be a second copy of
// a check that already happened. They are therefore the one path in this
// module that reads and writes a session by id alone, and they say so.

/// What the provisioning queue needs to know about the session it is
/// building a machine for.
#[derive(Debug, Clone, skyzen::FromRow)]
pub struct ProvisioningTarget {
    /// Whose session it is, which is whose harness account funds it, and
    /// whose GitHub authorization its checkouts are made with.
    pub user_id: UserId,
    /// Which harness the machine's daemon will drive.
    pub harness: HarnessKind,
    /// Whether flyco or the user chose the machine being built.
    ///
    /// Carried into the daemon's bootstrap so the agent can be told, and
    /// told what it means: a machine the user picked is not one to trade
    /// away for a faster build (docs/ux.md §9.5).
    pub machine_origin: MachineOrigin,
    /// Where the session is in its lifecycle right now.
    pub state: SessionState,
    /// The model the machine's harness is provisioned to run.
    ///
    /// `None` for a session opened before flyco recorded one; read through
    /// [`model_choice`](Self::model_choice), which resolves that to the
    /// harness's own default.
    pub model: Option<String>,
    /// The effort it runs at, where one was chosen.
    pub effort: Option<String>,
    /// The permission mode the machine's `flycod` is configured to run the
    /// session under.
    ///
    /// `NULL` for a session opened before flyco recorded one; read through
    /// [`permission_mode`](Self::permission_mode), which resolves that to
    /// the product default the machine was provisioned under all along.
    pub permission_mode: Option<PermissionMode>,
    /// Whether the machine's `flycod` is configured to give the session a
    /// screen — the `[computer]` table's `enabled`.
    pub computer_use: bool,
}

impl ProvisioningTarget {
    /// What the machine's `flycod` is configured to run the session on.
    ///
    /// A legacy row resolves to the harness's built-in default, which is
    /// the model it has been running all along — the configuration this
    /// feeds is the first place flyco has ever stated it.
    #[must_use]
    pub fn model_choice(&self) -> ModelChoice {
        model_of(self.harness, self.model.clone(), self.effort.clone())
    }

    /// The mode the machine's `flycod` is configured to run under.
    ///
    /// A legacy row resolves to the product default, which is the mode it
    /// was provisioned under — so this states the fact the machine has
    /// been living rather than a new decision.
    #[must_use]
    pub fn permission_mode(&self) -> PermissionMode {
        mode_of(self.permission_mode)
    }
}

/// Reads the session a provisioning job names.
///
/// `None` means the row is gone, which is a job to drop rather than a job to
/// retry.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn provisioning_target(
    db: &Db,
    id: SessionId,
) -> Result<Option<ProvisioningTarget>, ApiError> {
    Ok(sql!(
        db,
        "SELECT user_id, harness, machine_origin, state, model, effort, \
         permission_mode, computer_use FROM sessions WHERE id = {id}"
    )
    .fetch_optional()
    .await?)
}

/// Reads whose session this is.
///
/// The daemon-scoped routes' one lookup: an `fd_` token proves which
/// *session* is calling and nothing about a user, while everything the
/// daemon then asks for — the account a machine was provisioned through,
/// that account's catalog — is scoped to the owner. Deriving it here rather
/// than trusting a caller is what keeps a daemon token from reaching another
/// user's clouds.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is gone, or
/// [`ApiError`] if the database fails.
pub async fn owner(db: &Db, id: SessionId) -> Result<UserId, ApiError> {
    Ok(provisioning_target(db, id)
        .await?
        .ok_or(ApiError::SessionNotFound)?
        .user_id)
}

/// Reads whether flyco or the user chose this session's machine.
///
/// A session fact rather than a machine one, which is why it is read here
/// and not off the `machines` row: it says who made the decision that put
/// this session on a machine at all, and it survives every resize the
/// session goes through.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is gone, or
/// [`ApiError`] if the database fails.
pub async fn machine_origin(db: &Db, id: SessionId) -> Result<MachineOrigin, ApiError> {
    Ok(provisioning_target(db, id)
        .await?
        .ok_or(ApiError::SessionNotFound)?
        .machine_origin)
}

/// Records that provisioning gave up, and why.
///
/// The reason is stored rather than only logged: the user is the one who has
/// to act on it — change region, ask for a quota increase, pick another
/// machine type — and a session that fails silently is the state this whole
/// path exists to prevent.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is gone, or
/// [`ApiError::InvalidTransition`] if it is not in a state that can fail.
pub async fn fail(db: &Db, rooms: &Rooms, id: SessionId, reason: &str) -> Result<(), ApiError> {
    let state = provisioning_target(db, id)
        .await?
        .ok_or(ApiError::SessionNotFound)?
        .state;
    let next =
        state
            .transition(SessionState::Failed)
            .map_err(|error| ApiError::InvalidTransition {
                from: error.from,
                to: error.to,
            })?;

    sql!(
        db,
        "UPDATE sessions SET state = {next}, failure_reason = {reason.to_owned()}, \
         last_active_unix = {now_unix()} WHERE id = {id}"
    )
    .execute()
    .await?;

    // The row reserved for the machine that was never built is released
    // with the session: left in `provisioning`, the page would keep calling
    // a machine that does not exist `starting` under a `Failed` pill.
    //
    // Only the unbuilt one. A machine that exists is the caller's to
    // destroy, because destroying it means talking to a provider and this
    // does not have the account to do it with — so every path that can fail
    // a session holding a *built* machine destroys it first, as the stall
    // sweep and the agent-death report both do (issue #199).
    machines::release_unbuilt(db, id).await?;
    tracing::warn!(session = %id, %reason, "a session's machine could not be provisioned");

    // The page learns of the failure the way it learns of everything else,
    // from the room; without this it goes on drawing the provisioning
    // timeline until somebody reloads. A room that cannot be reached costs
    // the watcher a live update, not the session its (already recorded)
    // failure, so it is logged rather than returned.
    if let Err(error) = rooms
        .broadcast(
            db,
            id,
            &ClientEvent::SessionStateChanged {
                state: SessionState::Failed,
            },
        )
        .await
    {
        tracing::warn!(session = %id, %error, "a session's failure did not reach its room");
    }
    Ok(())
}

/// Records that a session lost the machine it was running on, and why.
///
/// Called from the session's own daemon in the seconds between a
/// provider's reclaim notice and the machine going, and from the
/// reconcile that learns a codespace suspended itself — and it is
/// idempotent for the same reason [`pause_for_budget`] is: nothing about
/// either is delivered exactly once, and a second report must not be an
/// error.
///
/// A session already interrupted keeps its state and has only the reason
/// refreshed — the reconcile's answer can change under it, `suspended`
/// when written and `machine_lost` once the codespace is actually gone —
/// and its activity clock is left alone, so an abandoned session still
/// archives on the idleness it was already accumulating. A session still
/// provisioning is interrupted mid-flight — the recovery running is the
/// call that learned the machine is gone — and that *is* activity.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is gone, or
/// [`ApiError::InvalidTransition`] if it is in a state that cannot be
/// interrupted — a session being archived, say, whose machine is going
/// anyway.
pub async fn interrupted(
    db: &Db,
    id: SessionId,
    reason: InterruptedReason,
) -> Result<(), ApiError> {
    let state: SessionState = sql!(db, "SELECT state FROM sessions WHERE id = {id}")
        .fetch_scalar_optional()
        .await?
        .ok_or(ApiError::SessionNotFound)?;
    if state == SessionState::Interrupted {
        // Already off its machine. The reason is rewritten — it is what the
        // UI has to render and what the next resume reads — but the clock
        // is not: a reconcile confirming a suspension every minute would
        // otherwise keep an abandoned session from ever going idle.
        sql!(
            db,
            "UPDATE sessions SET interrupted_reason = {reason} WHERE id = {id}"
        )
        .execute()
        .await?;
        return Ok(());
    }
    if state == SessionState::Provisioning {
        // Being put back on a machine already. The reason is written
        // anyway: a suspension or a reclaim during a provision is still
        // what the UI has to render, and the column is what tells a
        // recovery apart from a first provision.
        sql!(
            db,
            "UPDATE sessions SET interrupted_reason = {reason}, \
             last_active_unix = {now_unix()} WHERE id = {id}"
        )
        .execute()
        .await?;
        return Ok(());
    }

    let next = state
        .transition(SessionState::Interrupted)
        .map_err(|error| ApiError::InvalidTransition {
            from: error.from,
            to: error.to,
        })?;
    sql!(
        db,
        "UPDATE sessions SET state = {next}, interrupted_reason = {reason}, \
         last_active_unix = {now_unix()} WHERE id = {id}"
    )
    .execute()
    .await?;
    tracing::warn!(session = %id, ?reason, "a session lost its machine");
    Ok(())
}

/// Records that a session's machine ceased to exist — deleted, not stopped.
///
/// The write for a codespace GitHub deleted outright, or failed past
/// starting, or whose start answered 404: there is no disk to come back
/// to, so [`InterruptedReason::MachineLost`] is what tells the next resume
/// to *provision* rather than start.
///
/// Unlike [`interrupted`], a `provisioning` session *moves* here rather
/// than keeping only the reason — nothing is in flight that could still
/// deliver a machine, so the session waits interrupted until it is spoken
/// to again. A session already interrupted has the reason rewritten —
/// `suspended` was true when it was written and is not true now — and a
/// paused session is deliberately left paused: its pause reason is still
/// true, the machine row records the loss, and its wake reads the row and
/// provisions around the gap on its own.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is gone, or
/// [`ApiError::InvalidTransition`] if it is in a state that cannot be
/// interrupted — a session that already ended, whose machine the caller
/// should not have been reconciling.
pub async fn machine_lost(db: &Db, id: SessionId) -> Result<(), ApiError> {
    let state: SessionState = sql!(db, "SELECT state FROM sessions WHERE id = {id}")
        .fetch_scalar_optional()
        .await?
        .ok_or(ApiError::SessionNotFound)?;
    let reason = InterruptedReason::MachineLost;
    match state {
        SessionState::Paused => return Ok(()),
        SessionState::Interrupted => {
            sql!(
                db,
                "UPDATE sessions SET interrupted_reason = {reason} WHERE id = {id}"
            )
            .execute()
            .await?;
            return Ok(());
        }
        _ => {}
    }
    let next = state
        .transition(SessionState::Interrupted)
        .map_err(|error| ApiError::InvalidTransition {
            from: error.from,
            to: error.to,
        })?;
    sql!(
        db,
        "UPDATE sessions SET state = {next}, interrupted_reason = {reason}, \
         last_active_unix = {now_unix()} WHERE id = {id}"
    )
    .execute()
    .await?;
    tracing::warn!(session = %id, "a session's machine ceased to exist");
    Ok(())
}

/// Why a session is interrupted, when it is.
///
/// The reason is written with the state and cleared when the session is
/// put back, so its presence and the `interrupted` state are one fact.
/// Callers that resume a session read it *first*, because the resume's own
/// write is what clears it.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is gone, or
/// [`ApiError`] if the database fails.
pub async fn interruption_reason(
    db: &Db,
    id: SessionId,
) -> Result<Option<InterruptedReason>, ApiError> {
    sql!(
        db,
        "SELECT interrupted_reason FROM sessions WHERE id = {id}"
    )
    .fetch_scalar_optional()
    .await?
    .ok_or(ApiError::SessionNotFound)
}

/// Puts a session that lost its machine back into
/// [`SessionState::Provisioning`], keeping the reason it lost it.
///
/// What a [`Recover`](crate::provisioning_queue::ProvisioningJob::Recover)
/// job does before it asks the provider for the machine back. The reason
/// survives the move deliberately: it is the only thing that tells this
/// provisioning apart from a first one, which is what the UI renders as
/// `Migrating` rather than `Provisioning` (docs/ux.md §6).
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is gone, or
/// [`ApiError::InvalidTransition`] if it cannot be put back on a machine.
pub async fn recovering(db: &Db, id: SessionId) -> Result<(), ApiError> {
    let state: SessionState = sql!(db, "SELECT state FROM sessions WHERE id = {id}")
        .fetch_scalar_optional()
        .await?
        .ok_or(ApiError::SessionNotFound)?;
    if state == SessionState::Provisioning {
        return Ok(());
    }
    let next = state
        .transition(SessionState::Provisioning)
        .map_err(|error| ApiError::InvalidTransition {
            from: error.from,
            to: error.to,
        })?;
    sql!(
        db,
        "UPDATE sessions SET state = {next}, last_active_unix = {now_unix()} WHERE id = {id}"
    )
    .execute()
    .await?;
    Ok(())
}

/// Pauses an active session because its compute budget is exhausted.
///
/// Repeating the call is intentional: the budget-signal outbox is delivered
/// at least once, so a cron retry may see the durable pause already written.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session disappeared, or
/// [`ApiError::InvalidTransition`] when a non-active session is asked to
/// enter the budget-paused state.
pub async fn pause_for_budget(db: &Db, id: SessionId) -> Result<(), ApiError> {
    let state: SessionState = sql!(db, "SELECT state FROM sessions WHERE id = {id}")
        .fetch_scalar_optional()
        .await?
        .ok_or(ApiError::SessionNotFound)?;
    let budget = PausedReason::Budget;
    if state == SessionState::Paused {
        return Ok(());
    }
    let next =
        state
            .transition(SessionState::Paused)
            .map_err(|error| ApiError::InvalidTransition {
                from: error.from,
                to: error.to,
            })?;
    sql!(
        db,
        "UPDATE sessions SET state = {next}, paused_reason = {budget}, \
         last_active_unix = {now_unix()} WHERE id = {id}"
    )
    .execute()
    .await?;
    Ok(())
}

/// Puts a session back into [`SessionState::Provisioning`] and clears the
/// reason the previous attempt left behind.
///
/// The one move `POST /v1/sessions/{id}/resume` makes durable before the job
/// is enqueued, so a browser that reloads immediately sees a session on its
/// way back rather than the state it was resumed out of.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is not the caller's,
/// or [`ApiError::InvalidTransition`] if it is not resumable.
pub async fn resume(db: &Db, user: UserId, id: SessionId) -> Result<SessionDetail, ApiError> {
    let row = load(db, user, id).await?;
    let next = row
        .state
        .transition(SessionState::Provisioning)
        .map_err(|error| ApiError::InvalidTransition {
            from: error.from,
            to: error.to,
        })?;

    sql!(
        db,
        // The usage-limit wait goes with the rest: a session the user has
        // decided to put back on a machine themselves is not waiting for a
        // plan window any more, and a stale countdown would go on offering
        // to continue a conversation nobody is waiting on.
        "UPDATE sessions SET state = {next}, failure_reason = NULL, \
         interrupted_reason = NULL, paused_reason = NULL, \
         usage_limit_window = NULL, usage_limit_resets_at_unix = NULL, \
         usage_limit_resume_at_unix = NULL, usage_limit_queued_message = NULL, \
         last_active_unix = {now_unix()} \
         WHERE id = {id} AND user_id = {user}"
    )
    .execute()
    .await?;

    find(db, user, id).await
}

/// Records the harness-native session id a later resume must reopen.
///
/// Written when the daemon announces `Started`, and kept across archive so
/// a rebuilt machine continues the same conversation rather than opening a
/// fresh one.
///
/// # Errors
///
/// Returns [`ApiError`] if the write fails.
pub async fn record_harness_session(
    db: &Db,
    id: SessionId,
    harness_session_id: &str,
) -> Result<(), ApiError> {
    sql!(
        db,
        "UPDATE sessions SET harness_session_id = {harness_session_id}, \
         last_active_unix = {now_unix()} WHERE id = {id}"
    )
    .execute()
    .await?;
    Ok(())
}

/// Records what a session is doing, as its own daemon or its user just
/// said.
///
/// The three writes that maintain it are the turn events the daemon reports
/// (`turn-started`, `turn-completed`, `turn-failed`) and the user's own
/// messages, which between them are the whole position of a conversation.
/// A pending approval is not one of them on purpose: `approvals` owns that
/// fact and [`SessionActivity::with_pending_approval`] applies it where a
/// session is read, so deciding one needs no compensating write here.
///
/// Written by session id rather than by owner: the daemon-scoped routes
/// carry no user, and the user-scoped one has already proved ownership
/// before it drives the session at all.
///
/// The write also stamps `last_active_unix`, because a turn starting or
/// ending is exactly what that column means — and a session running turns
/// must not be swept up by the idle archiver.
///
/// # Errors
///
/// Returns [`ApiError`] if the write fails.
pub async fn record_activity(
    db: &Db,
    id: SessionId,
    activity: SessionActivity,
) -> Result<(), ApiError> {
    sql!(
        db,
        "UPDATE sessions SET activity = {activity}, last_active_unix = {now_unix()} \
         WHERE id = {id}"
    )
    .execute()
    .await?;
    Ok(())
}

/// Stamps the activity clock without moving the conversation's position.
///
/// What a client command that is not a message means for idleness: the
/// session is being *used* — a terminal keystroke, a shell run, a decided
/// approval — which says nothing about `activity` and everything about
/// `last_active_unix`. [`suspendable`] reads that column, so a machine its
/// user is typing into is never the one the sweep stops.
///
/// # Errors
///
/// Returns [`ApiError`] if the write fails.
pub async fn touch(db: &Db, id: SessionId) -> Result<(), ApiError> {
    sql!(
        db,
        "UPDATE sessions SET last_active_unix = {now_unix()} WHERE id = {id}"
    )
    .execute()
    .await?;
    Ok(())
}

/// The conversation and the model a daemon starting on this session must
/// continue it on.
///
/// One read rather than two, because a daemon that asked for them
/// separately could get an answer from either side of a model change and
/// resume the right conversation on the wrong model.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is gone, or
/// [`ApiError`] if the read fails.
pub async fn harness_session(db: &Db, id: SessionId) -> Result<HarnessSessionView, ApiError> {
    #[derive(Debug, skyzen::FromRow)]
    struct Row {
        harness_session_id: Option<String>,
        harness: HarnessKind,
        model: Option<String>,
        effort: Option<String>,
        permission_mode: Option<PermissionMode>,
    }

    let row: Row = sql!(
        db,
        "SELECT harness_session_id, harness, model, effort, permission_mode \
         FROM sessions WHERE id = {id}"
    )
    .fetch_optional()
    .await?
    .ok_or(ApiError::SessionNotFound)?;
    Ok(HarnessSessionView {
        harness_session_id: row.harness_session_id,
        model: model_of(row.harness, row.model, row.effort),
        permission_mode: mode_of(row.permission_mode),
    })
}

/// Marks a provisioning session active because its daemon has arrived.
///
/// A session goes live when its daemon greets the control plane, not when a
/// provider's API call returns: a machine that exists is not an agent that
/// is ready. The greeting itself is the attach the session's room
/// validates — and a Durable Object cannot reach D1, so the durable half
/// of it happens here, in the Worker, on the very request the daemon
/// attaches with.
///
/// Silently a no-op for a session that is already past provisioning: a
/// daemon re-attaches after every eviction, every redeploy and every
/// dropped stream, and none of those is a lifecycle event.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn daemon_arrived(db: &Db, rooms: &Rooms, id: SessionId) -> Result<(), ApiError> {
    let provisioning = SessionState::Provisioning;
    let interrupted = SessionState::Interrupted;
    let suspended = InterruptedReason::Suspended;
    let active = SessionState::Active;
    // The reason the session lost its machine is cleared with the same
    // write that says it has one again: it is what the UI renders
    // `Migrating` from, and a session whose daemon is back is not
    // migrating any more.
    //
    // The interrupted arm is the codespace somebody started by hand:
    // nothing queued a recovery for it — a suspension waits to be spoken
    // to — but the user opened it on github.com, `postStart` ran, and its
    // daemon attached. Only `suspended` is taken this way: a machine flyco
    // recorded as *lost* has nothing to arrive from, and one that does is
    // the row being wrong rather than the session being back.
    let written = sql!(
        db,
        "UPDATE sessions SET state = {active}, interrupted_reason = NULL, \
         last_active_unix = {now_unix()} \
         WHERE id = {id} AND (state = {provisioning} \
         OR (state = {interrupted} AND interrupted_reason = {suspended}))"
    )
    .execute()
    .await?;

    if written.rows_written > 0 {
        tracing::info!(session = %id, "a session went live: its daemon reached the control plane");
        // The one lifecycle move every session makes, and the one the page
        // cannot deduce: the timeline's last stage says the agent is up,
        // not that the session is. Without this frame the header goes on
        // counting the provisioning clock while the agent answers below it
        // (issue #209). Logged rather than raised for the same reason
        // `fail` logs it: the transition is already durable, and a room
        // that cannot be reached costs a watcher a live update.
        if let Err(error) = rooms
            .broadcast(
                db,
                id,
                &ClientEvent::SessionStateChanged {
                    state: SessionState::Active,
                },
            )
            .await
        {
            tracing::warn!(session = %id, %error, "a session going live did not reach its room");
        }
    }
    Ok(())
}

/// A session idle long enough that flyco archives it automatically.
#[derive(Debug, skyzen::FromRow)]
pub struct IdleSession {
    /// Identifier.
    pub id: SessionId,
    /// Owner, so archive can destroy their machine.
    pub user_id: UserId,
}

/// Records that a machine being built got somewhere.
///
/// Every provisioning milestone moves this clock, which is what makes
/// [`stalled_provisions`] mean "stopped making progress" rather than
/// "taking a while". Scoped to `provisioning` so a stage that arrives late
/// cannot move a session that has since gone on with its life.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn note_progress(db: &Db, id: SessionId) -> Result<(), ApiError> {
    let provisioning = SessionState::Provisioning;
    sql!(
        db,
        "UPDATE sessions SET last_active_unix = {now_unix()} \
         WHERE id = {id} AND state = {provisioning}"
    )
    .execute()
    .await?;
    Ok(())
}

/// Records why a session's daemon stopped before it reported in.
///
/// Written into `failure_reason` while the session is still
/// `provisioning`, which is not yet a failure: `detail_from` reports that
/// column only for a session that actually failed, and a daemon that
/// restarts successfully leaves it behind. It is the sentence the stall
/// sweep uses if the machine never does come up.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn note_startup_failure(db: &Db, id: SessionId, message: &str) -> Result<(), ApiError> {
    let provisioning = SessionState::Provisioning;
    sql!(
        db,
        "UPDATE sessions SET failure_reason = {message.to_owned()} \
         WHERE id = {id} AND state = {provisioning}"
    )
    .execute()
    .await?;
    tracing::warn!(session = %id, message, "a session's daemon reported that it could not start");
    Ok(())
}

/// What [`note_startup_failure`] last recorded for a session, if anything.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn startup_failure(db: &Db, id: SessionId) -> Result<Option<String>, ApiError> {
    let row: Option<StartupFailure> =
        sql!(db, "SELECT failure_reason FROM sessions WHERE id = {id}")
            .fetch_optional()
            .await?;
    Ok(row.and_then(|row| row.failure_reason))
}

/// One row of [`startup_failure`].
#[derive(Debug, skyzen::FromRow)]
struct StartupFailure {
    /// The daemon's sentence, when it sent one.
    failure_reason: Option<String>,
}

/// Sessions still being built past [`PROVISION_DEADLINE_SECS`].
///
/// The clock runs from the last thing that happened to the session, which
/// a provisioning stage moves: a machine that is making progress is never
/// in this list, and one whose daemon is crash-looping stops moving it and
/// is.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn stalled_provisions(db: &Db, at_unix: u64) -> Result<Vec<IdleSession>, ApiError> {
    let cutoff = at_unix.saturating_sub(PROVISION_DEADLINE_SECS);
    let provisioning = SessionState::Provisioning;
    // Pending handoffs are excluded rather than merely given a longer
    // clock: they sit in `provisioning` by design while the sender uploads,
    // so "no machine progress" is the expected state and not a stall.
    // `handoffs::abandoned` reaps them on their own deadline.
    Ok(sql!(
        db,
        "SELECT id, user_id FROM sessions \
         WHERE state = {provisioning} AND created_at_unix <= {cutoff} \
         AND last_active_unix <= {cutoff} \
         AND NOT EXISTS (SELECT 1 FROM handoffs h WHERE h.session_id = sessions.id \
                         AND h.completed_at_unix IS NULL)"
    )
    .fetch_all()
    .await?)
}

/// Sessions flyco has stopped that still name a machine it is paying for.
///
/// The invariant the release paths are supposed to keep, checked rather
/// than assumed. Every one of those paths — archive, the stall sweep, a
/// daemon's failure report — is a request that can be cut short partway
/// through, and the one that is cut short is exactly the one whose caller
/// was dying: a daemon reporting why it cannot start is gone the moment it
/// has said so. A machine outliving its session is silent and expensive, so
/// it is swept rather than left to whoever failed to tear it down.
///
/// Reservations a provider never filled in are selected too: destroying
/// one is a row update and no provider call, and a predicate that had to
/// tell the two apart would be one more place for them to disagree.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn ended_holding_a_machine(db: &Db) -> Result<Vec<IdleSession>, ApiError> {
    let failed = SessionState::Failed;
    let archived = SessionState::Archived;
    let destroyed = flyco_core::machine::MachineState::Destroyed;
    Ok(sql!(
        db,
        "SELECT sessions.id AS id, sessions.user_id AS user_id FROM sessions \
         JOIN machines ON machines.session_id = sessions.id \
         WHERE (sessions.state = {failed} OR sessions.state = {archived}) \
         AND machines.state != {destroyed}"
    )
    .fetch_all()
    .await?)
}

/// Sessions that have sat idle long enough that flyco archives them.
///
/// Two clocks, one list. A session still in play — `active`, `paused`,
/// `interrupted` — gets the week: idle is not over, and a paused thought
/// or an interrupted machine might still be picked back up. A session
/// that is over — [`SessionState::Failed`], the only terminal state flyco
/// writes — gets the day [`ARCHIVE_FINISHED_AFTER_IDLE_SECS`] allows,
/// because there is nothing on it to come back to.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn idle_since(db: &Db, at_unix: u64) -> Result<Vec<IdleSession>, ApiError> {
    let idle_cutoff = at_unix.saturating_sub(ARCHIVE_AFTER_IDLE_SECS);
    let finished_cutoff = at_unix.saturating_sub(ARCHIVE_FINISHED_AFTER_IDLE_SECS);
    let active = SessionState::Active;
    let paused = SessionState::Paused;
    let interrupted = SessionState::Interrupted;
    let failed = SessionState::Failed;
    Ok(sql!(
        db,
        "SELECT id, user_id FROM sessions \
         WHERE ((state = {active} OR state = {paused} OR state = {interrupted}) \
                AND last_active_unix <= {idle_cutoff}) \
            OR (state = {failed} AND last_active_unix <= {finished_cutoff})"
    )
    .fetch_all()
    .await?)
}

/// Sessions whose machine [`crate::app::suspend_idle`] should stop for
/// idleness, once a sweep.
///
/// Three conditions, each guarding a different cost. `active` means nothing
/// else has already ended the machine's life — a paused session's machine
/// is the usage-limit sweep's decision, an interrupted one's is already
/// gone. `last_active_unix` past the cutoff is the idleness itself —
/// [`SUSPEND_AFTER_IDLE_SECS`], not the archive week, because compute
/// bills by the minute and a disk does not. And no turn is in flight, or
/// the one in flight is blocked: a suspension that killed the agent
/// mid-answer would lose the work the machine was running to produce, but
/// an agent that raised an approval and has waited on it as long as the
/// idle threshold is doing nothing the machine is needed for — the user is
/// away and the agent cannot move until they are back, which is what
/// suspension is *for* (issue #355). The blocked turn's clock is the
/// approval's own age rather than the turn's, so a long turn that only
/// just asked keeps its machine.
///
/// A session the user is holding awake is left alone until the hold runs
/// out, whatever the idleness says: the sweep cannot see a build, a soak
/// test or a watch loop, and the user asking for the machine to stay is
/// the only evidence there is that one is running.
///
/// The machine join admits `deallocated` as well as `running`: a machine
/// already off needs no provider call, but the session write that goes
/// with the stop is still owed — either a previous pass was cut short
/// between them, or the user stopped the machine by hand and the session
/// should read interrupted for it.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn suspendable(db: &Db, at_unix: u64) -> Result<Vec<IdleSession>, ApiError> {
    let cutoff = at_unix.saturating_sub(SUSPEND_AFTER_IDLE_SECS);
    let active = SessionState::Active;
    let working = SessionActivity::Working;
    let pending = ApprovalState::Pending;
    let running = flyco_core::machine::MachineState::Running;
    let deallocated = flyco_core::machine::MachineState::Deallocated;
    Ok(sql!(
        db,
        "SELECT sessions.id AS id, sessions.user_id AS user_id FROM sessions \
         JOIN machines ON machines.session_id = sessions.id \
         WHERE sessions.state = {active} \
         AND sessions.last_active_unix <= {cutoff} \
         AND (sessions.activity != {working} \
              OR EXISTS (SELECT 1 FROM approvals \
                         WHERE approvals.session_id = sessions.id \
                         AND approvals.state = {pending} \
                         AND approvals.created_at_unix <= {cutoff})) \
         AND (sessions.awake_until_unix IS NULL \
              OR sessions.awake_until_unix <= {at_unix}) \
         AND (machines.state = {running} OR machines.state = {deallocated})"
    )
    .fetch_all()
    .await?)
}

// ── The usage-limit pause ──
//
// One mechanism, four columns and one reason token (migration 0024). The
// reads and writes live here because this is the `sessions` table's module;
// what *decides* to pause a session, stop its machine and continue it lives
// in `crate::usage_limits`, which is the mechanism and reads none of these
// columns itself.

/// A session waiting out a spent plan window, as the minute sweep reads it.
///
/// Every column of the wait in one row, because the sweep asks three
/// questions of each waiting session — does it still hold a machine, is it
/// time to start one, is it time to continue the conversation — and three
/// queries would be three chances for them to disagree about the same row.
#[derive(Debug, skyzen::FromRow)]
pub struct UsageLimitWait {
    /// The waiting session.
    pub id: SessionId,
    /// Its owner, which is whose credentials act on its machine.
    pub user_id: UserId,
    /// Where it is in its lifecycle, which is how far through the wait it
    /// is: [`Paused`](SessionState::Paused) is still waiting,
    /// [`Provisioning`](SessionState::Provisioning) is coming back, and
    /// [`Active`](SessionState::Active) is up and waiting for the reset.
    pub state: SessionState,
    usage_limit_window: String,
    usage_limit_resets_at_unix: u64,
    usage_limit_resume_at_unix: Option<u64>,
    usage_limit_queued_message: Option<String>,
}

impl UsageLimitWait {
    /// The wait itself, in the shape the domain model states it in.
    #[must_use]
    pub fn pause(&self) -> UsageLimitPause {
        UsageLimitPause {
            window: self.usage_limit_window.clone(),
            resets_at_unix: self.usage_limit_resets_at_unix,
            resume_at_unix: self.usage_limit_resume_at_unix,
            queued_message: self.usage_limit_queued_message.clone(),
        }
    }
}

/// Pauses a session because a window of its harness plan is spent.
///
/// Answers whether this call is the one that paused it. A daemon reports the
/// limit once per limit, but the report is a request that can be retried and
/// both harnesses can name the same limit twice — a refused turn and the
/// snapshot that explains it — so a second report of a session already
/// waiting is a no-op rather than a second pause, a second push and a
/// second interrupt.
///
/// The machine is *not* touched here. Stopping it is a provider call
/// measured in tens of seconds, and the caller is a daemon that has just
/// been refused a turn; the durable wait is written now and the minute sweep
/// releases the compute, which is also what makes the release survive a
/// request that dies half way through.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is gone, or
/// [`ApiError::InvalidTransition`] when a session that cannot be paused —
/// one being archived, one with no machine left — is asked to wait.
pub async fn pause_for_usage_limit(
    db: &Db,
    id: SessionId,
    pause: &UsageLimitPause,
) -> Result<bool, ApiError> {
    let row: PausedState = sql!(
        db,
        "SELECT state, paused_reason FROM sessions WHERE id = {id}"
    )
    .fetch_optional()
    .await?
    .ok_or(ApiError::SessionNotFound)?;
    let limit = PausedReason::UsageLimit;
    if row.paused_reason == Some(limit) {
        return Ok(false);
    }
    let next = row
        .state
        .transition(SessionState::Paused)
        .map_err(|error| ApiError::InvalidTransition {
            from: error.from,
            to: error.to,
        })?;
    let window = pause.window.as_str();
    let resets_at = pause.resets_at_unix;
    let resume_at = pause.resume_at_unix;
    sql!(
        db,
        "UPDATE sessions SET state = {next}, paused_reason = {limit}, \
         usage_limit_window = {window}, usage_limit_resets_at_unix = {resets_at}, \
         usage_limit_resume_at_unix = {resume_at}, usage_limit_queued_message = NULL, \
         last_active_unix = {now_unix()} WHERE id = {id}"
    )
    .execute()
    .await?;
    Ok(true)
}

/// The two columns that say whether a session is already waiting.
#[derive(Debug, skyzen::FromRow)]
struct PausedState {
    state: SessionState,
    paused_reason: Option<PausedReason>,
}

/// Holds what the user typed while their session waits for a plan window.
///
/// Answers whether there was a wait to hold it against. The composer stays
/// usable through a usage-limit pause precisely so that the answer to "can I
/// tell it what to do next" is yes, and what is typed becomes the
/// continuation sent at the reset instead of flyco's canned nudge.
///
/// The newest message wins rather than accumulating a queue: this is the
/// next thing the user wants said, and a session that came back with four
/// half-formed instructions in a row would be a worse conversation than one
/// that came back with the last of them.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is not the caller's.
pub async fn queue_usage_limit_message(
    db: &Db,
    user: UserId,
    id: SessionId,
    text: &str,
) -> Result<bool, ApiError> {
    let limit = PausedReason::UsageLimit;
    let written = sql!(
        db,
        "UPDATE sessions SET usage_limit_queued_message = {text}, \
         last_active_unix = {now_unix()} \
         WHERE id = {id} AND user_id = {user} AND paused_reason = {limit}"
    )
    .execute()
    .await?;
    Ok(written.rows_written > 0)
}

/// Every session waiting out a spent plan window.
///
/// Not filtered by time: the sweep has three different deadlines to compare
/// each row against, and a query per deadline would read the same handful of
/// rows three times. There are never many — a waiting session is one whose
/// account is out of plan — so the whole set is read and the arithmetic is
/// done once in one place.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn usage_limit_waits(db: &Db) -> Result<Vec<UsageLimitWait>, ApiError> {
    let limit = PausedReason::UsageLimit;
    Ok(sql!(
        db,
        "SELECT id, user_id, state, usage_limit_window, usage_limit_resets_at_unix, \
         usage_limit_resume_at_unix, usage_limit_queued_message FROM sessions \
         WHERE paused_reason = {limit} AND usage_limit_window IS NOT NULL \
         AND usage_limit_resets_at_unix IS NOT NULL \
         ORDER BY usage_limit_resets_at_unix"
    )
    .fetch_all()
    .await?)
}

/// Ends a usage-limit wait: the window has turned over and the session is
/// running again.
///
/// `from` is the state the caller read, and it is both the guard and the
/// decision. A session whose machine was kept is still
/// [`Paused`](SessionState::Paused) and is moved back to
/// [`Active`](SessionState::Active) through [`SessionState::transition`],
/// like every other lifecycle move in this module; one whose machine was
/// stopped came back through [`recovering`] and [`daemon_arrived`] and is
/// already active, so its state is left exactly as it is.
///
/// The guard is what keeps one continuation per pause: the statement writes
/// only a row that is still in the state it was read in, so two overlapping
/// crons cannot both win, and the answer is whether this call is the one
/// that did.
///
/// # Errors
///
/// Returns [`ApiError::InvalidTransition`] if the session cannot leave the
/// state it is in, or [`ApiError`] if the database fails.
pub async fn end_usage_limit_wait(
    db: &Db,
    id: SessionId,
    from: SessionState,
) -> Result<bool, ApiError> {
    let next = if from == SessionState::Paused {
        from.transition(SessionState::Active)
            .map_err(|error| ApiError::InvalidTransition {
                from: error.from,
                to: error.to,
            })?
    } else {
        from
    };
    let limit = PausedReason::UsageLimit;
    let written = sql!(
        db,
        "UPDATE sessions SET state = {next}, paused_reason = NULL, \
         usage_limit_window = NULL, usage_limit_resets_at_unix = NULL, \
         usage_limit_resume_at_unix = NULL, usage_limit_queued_message = NULL, \
         last_active_unix = {now_unix()} \
         WHERE id = {id} AND paused_reason = {limit} AND state = {from}"
    )
    .execute()
    .await?;
    Ok(written.rows_written > 0)
}
