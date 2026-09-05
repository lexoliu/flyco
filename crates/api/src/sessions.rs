//! The `sessions` table.
//!
//! Ownership is enforced in the `WHERE` clause of every read: a session
//! belonging to somebody else is indistinguishable from one that does not
//! exist, so the API never confirms that an id is real to a caller who has
//! no business knowing.

use flyco_core::{
    ARCHIVE_AFTER_IDLE_SECS, ApprovalState, BranchName, BudgetConfig, BudgetId, BudgetStage,
    ClientEvent, HarnessKind, InterruptedReason, MAX_SESSION_TITLE_CHARS, MachineOrigin,
    PROVISION_DEADLINE_SECS, RepoSlug, SessionActivity, SessionDetail, SessionId, SessionState,
    SessionSummary, Usd, UserId,
};
use skyzen::sql;
use skyzen_services::Db;

use crate::budgets;
use crate::clock::now_unix;
use crate::error::ApiError;
use crate::machines;
use crate::rooms::Rooms;

/// The session a caller may still archive, and its budget.
#[derive(Debug, skyzen::FromRow)]
struct SessionRow {
    id: SessionId,
    title: String,
    harness: HarnessKind,
    repo: RepoSlug,
    branch: Option<BranchName>,
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
    created_at_unix: u64,
    last_active_unix: u64,
}

impl From<SessionRow> for SessionSummary {
    fn from(row: SessionRow) -> Self {
        Self {
            id: row.id,
            title: row.title,
            harness: row.harness,
            repo: row.repo,
            branch: row.branch,
            state: row.state,
            // An undecided approval blocks the agent whatever the last turn
            // event said, so it is applied here, once, where every reader
            // of a session goes through.
            activity: row.activity.with_pending_approval(row.approval_pending),
            machine_origin: row.machine_origin,
            interrupted_reason: row.interrupted_reason,
            created_at_unix: row.created_at_unix,
            last_active_unix: row.last_active_unix,
        }
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
    Ok(SessionDetail {
        summary: row.into(),
        budget,
        failure,
    })
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

/// Everything `POST /v1/sessions` decided before a row could be written.
///
/// One argument rather than six positional ones: `harness`, `repo`, and
/// `title` are all things a session is opened with, and a call site that
/// swaps two of them would still compile.
#[derive(Debug, Clone, Copy)]
pub struct Opening<'a> {
    /// Whose session it is.
    pub user: UserId,
    /// What to call it — the excerpt of the prompt that opened it.
    pub title: &'a str,
    /// Which coding harness drives it.
    pub harness: HarnessKind,
    /// Repository it works in.
    pub repo: &'a RepoSlug,
    /// Branch it works on — the one the caller named, or the repository's
    /// default as GitHub reported it while the session was being created.
    pub branch: &'a BranchName,
    /// Whether flyco or the caller chose the machine.
    pub machine_origin: MachineOrigin,
    /// What it may spend.
    pub budget: BudgetConfig,
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
        repo,
        branch,
        machine_origin,
        ..
    } = opening;

    sql!(
        db,
        "INSERT INTO sessions \
         (id, user_id, title, harness, repo, branch, state, machine_origin, budget_id, \
          created_at_unix, last_active_unix) \
         VALUES ({id}, {user}, {title}, {harness}, {repo}, {branch}, \
                 {SessionState::Provisioning}, {machine_origin}, {budget_id}, {now}, {now})"
    )
    .execute()
    .await?;

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
/// # Errors
///
/// Returns [`ApiError`] if the database fails or a stored row is malformed.
pub async fn list(db: &Db, user: UserId) -> Result<Vec<SessionSummary>, ApiError> {
    let pending = ApprovalState::Pending;
    let rows: Vec<SessionRow> = sql!(
        db,
        "SELECT s.id, s.title, s.harness, s.repo, s.branch, s.state, s.activity, \
         EXISTS (SELECT 1 FROM approvals a WHERE a.session_id = s.id AND a.state = {pending}) \
         AS approval_pending, \
         s.machine_origin, s.budget_id, s.failure_reason, s.interrupted_reason, \
         s.created_at_unix, s.last_active_unix \
         FROM sessions s WHERE s.user_id = {user} \
         ORDER BY s.created_at_unix DESC, s.id DESC"
    )
    .fetch_all()
    .await?;

    Ok(rows.into_iter().map(Into::into).collect())
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
    let pending = ApprovalState::Pending;
    sql!(
        db,
        "SELECT s.id, s.title, s.harness, s.repo, s.branch, s.state, s.activity, \
         EXISTS (SELECT 1 FROM approvals a WHERE a.session_id = s.id AND a.state = {pending}) \
         AS approval_pending, \
         s.machine_origin, s.budget_id, s.failure_reason, s.interrupted_reason, \
         s.created_at_unix, s.last_active_unix \
         FROM sessions s WHERE s.id = {id} AND s.user_id = {user}"
    )
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
    /// whose GitHub authorization its checkout is made with.
    pub user_id: UserId,
    /// Which harness the machine's daemon will drive.
    pub harness: HarnessKind,
    /// The repository the machine checks out.
    pub repo: RepoSlug,
    /// The branch it checks out.
    ///
    /// `None` for a session opened before flyco recorded one; the queue
    /// resolves the repository's default branch from GitHub and writes it
    /// back, so it is `None` at most once per session.
    pub branch: Option<BranchName>,
    /// Whether flyco or the user chose the machine being built.
    ///
    /// Carried into the daemon's bootstrap so the agent can be told, and
    /// told what it means: a machine the user picked is not one to trade
    /// away for a faster build (docs/ux.md §9.5).
    pub machine_origin: MachineOrigin,
    /// Where the session is in its lifecycle right now.
    pub state: SessionState,
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
        "SELECT user_id, harness, repo, branch, machine_origin, state \
         FROM sessions WHERE id = {id}"
    )
    .fetch_optional()
    .await?)
}

/// Records the branch a session works on.
///
/// Written by `POST /v1/sessions` through [`create`] for every session
/// opened since flyco recorded branches, and by the provisioning queue for
/// the ones opened before it — which resolve the repository's default branch
/// from GitHub the first time they are put on a machine. Recording it there
/// rather than resolving it again on every provision is what makes a
/// session's branch stable: a repository whose default moves must not move
/// a session that has already been built on the old one.
///
/// # Errors
///
/// Returns [`ApiError`] if the write fails.
pub async fn record_branch(db: &Db, id: SessionId, branch: &BranchName) -> Result<(), ApiError> {
    sql!(db, "UPDATE sessions SET branch = {branch} WHERE id = {id}")
        .execute()
        .await?;
    Ok(())
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
    machines::release_unbuilt(db, id).await?;
    tracing::warn!(session = %id, %reason, "a session's machine could not be provisioned");

    // The page learns of the failure the way it learns of everything else,
    // from the room; without this it goes on drawing the provisioning
    // timeline until somebody reloads. A room that cannot be reached costs
    // the watcher a live update, not the session its (already recorded)
    // failure, so it is logged rather than returned.
    if let Err(error) = rooms
        .broadcast(
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

/// Records that a session's machine is being reclaimed by its provider.
///
/// Called from the session's own daemon, in the seconds between the
/// provider's notice and the machine going, and it is idempotent for the
/// same reason [`pause_for_budget`] is: nothing about a reclamation is
/// delivered exactly once, and a second report must not be an error.
///
/// A session that is already off its machine — interrupted by an earlier
/// notice, or provisioning because the recovery has already started — keeps
/// the reason it has and is left alone. Only an [`Active`](SessionState::Active)
/// session actually moves.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is gone, or
/// [`ApiError::InvalidTransition`] if it is in a state that cannot be
/// interrupted — a session being archived, say, whose machine is going
/// anyway.
pub async fn interrupt_for_spot(db: &Db, id: SessionId) -> Result<(), ApiError> {
    let state: SessionState = sql!(db, "SELECT state FROM sessions WHERE id = {id}")
        .fetch_scalar_optional()
        .await?
        .ok_or(ApiError::SessionNotFound)?;
    let reason = InterruptedReason::SpotReclaimed;
    if matches!(
        state,
        SessionState::Interrupted | SessionState::Provisioning
    ) {
        // Already off its machine, or already being put back on one. The
        // reason is written anyway: a reclamation during a provision is
        // still what the UI has to render, and the column is what tells a
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
    tracing::warn!(session = %id, "a session's spot capacity is being reclaimed");
    Ok(())
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
        "UPDATE sessions SET state = {next}, last_active_unix = {now_unix()} WHERE id = {id}"
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
        "UPDATE sessions SET state = {next}, failure_reason = NULL, \
         interrupted_reason = NULL, last_active_unix = {now_unix()} \
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

/// The harness-native session id recorded for this session, if any.
///
/// # Errors
///
/// Returns [`ApiError`] if the read fails.
pub async fn harness_session_id(db: &Db, id: SessionId) -> Result<Option<String>, ApiError> {
    #[derive(Debug, skyzen::FromRow)]
    struct Row {
        harness_session_id: Option<String>,
    }

    let row: Option<Row> = sql!(
        db,
        "SELECT harness_session_id FROM sessions WHERE id = {id}"
    )
    .fetch_optional()
    .await?;
    Ok(row.and_then(|row| row.harness_session_id))
}

/// Marks a provisioning session active because its daemon has arrived.
///
/// A session goes live when its daemon greets the control plane, not when a
/// provider's API call returns: a machine that exists is not an agent that
/// is ready. The greeting itself is a `Hello` frame validated inside the
/// session's room — and a Durable Object cannot reach D1, so the durable
/// half of it happens here, in the Worker, on the very upgrade the daemon
/// sends that frame down.
///
/// Silently a no-op for a session that is already past provisioning: a
/// daemon reconnects after every eviction, every redeploy and every dropped
/// socket, and none of those is a lifecycle event.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn daemon_arrived(db: &Db, id: SessionId) -> Result<(), ApiError> {
    let provisioning = SessionState::Provisioning;
    let active = SessionState::Active;
    // The reason the session lost its machine is cleared with the same
    // write that says it has one again: it is what the UI renders
    // `Migrating` from, and a session whose daemon is back is not
    // migrating any more.
    let written = sql!(
        db,
        "UPDATE sessions SET state = {active}, interrupted_reason = NULL, \
         last_active_unix = {now_unix()} \
         WHERE id = {id} AND state = {provisioning}"
    )
    .execute()
    .await?;

    if written.rows_written > 0 {
        tracing::info!(session = %id, "a session went live: its daemon reached the control plane");
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
    Ok(sql!(
        db,
        "SELECT id, user_id FROM sessions \
         WHERE state = {provisioning} AND created_at_unix <= {cutoff} \
         AND last_active_unix <= {cutoff}"
    )
    .fetch_all()
    .await?)
}

/// Sessions that have sat idle past [`ARCHIVE_AFTER_IDLE_SECS`] and still
/// hold an environment.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn idle_since(db: &Db, at_unix: u64) -> Result<Vec<IdleSession>, ApiError> {
    let cutoff = at_unix.saturating_sub(ARCHIVE_AFTER_IDLE_SECS);
    let active = SessionState::Active;
    let paused = SessionState::Paused;
    let interrupted = SessionState::Interrupted;
    Ok(sql!(
        db,
        "SELECT id, user_id FROM sessions \
         WHERE last_active_unix <= {cutoff} \
         AND (state = {active} OR state = {paused} OR state = {interrupted})"
    )
    .fetch_all()
    .await?)
}
