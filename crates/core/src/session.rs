//! Session lifecycle.
//!
//! The state machine is enforced here so the control plane can never
//! record an impossible transition — an invalid one is an error at the
//! point of the bug, not a corrupt row discovered later.

use serde::{Deserialize, Serialize};

use crate::budget::BudgetView;
use crate::harness::{HarnessKind, ModelChoice, UsageReport};
use crate::id::{ProviderAccountId, SessionId};
use crate::money::Usd;
use crate::repo::{RepoSelection, SessionRepo};

/// Lifecycle state of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "sql", derive(skyzen::Column))]
pub enum SessionState {
    /// A machine is being provisioned; code is not yet fetched.
    Provisioning,
    /// The daemon is connected and the harness can run turns.
    Active,
    /// Paused by budget exhaustion or by the user; machine kept.
    Paused,
    /// Compute was reclaimed (spot eviction); disk kept, resumable.
    Interrupted,
    /// Archived: turns kept, execution environment released.
    Archived,
    /// Provisioning gave up, and the session has no machine.
    ///
    /// Its own state rather than a flag on `Provisioning`, because the two
    /// say opposite things to everyone who reads them: a provisioning
    /// session is one to wait for, and a failed one is one to act on. A
    /// session that ran out of provisioning attempts must never be
    /// indistinguishable from one whose machine is thirty seconds away.
    Failed,
}

/// An invalid session state transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid session transition: {from:?} -> {to:?}")]
pub struct SessionTransitionError {
    /// State the session was in.
    pub from: SessionState,
    /// State the caller tried to move it to.
    pub to: SessionState,
}

impl SessionState {
    /// Validates and performs a transition.
    ///
    /// **Archived is not terminal**, and that is the History feature rather
    /// than a loophole: archiving releases the *execution environment* and
    /// keeps everything else — the turns, the transcript in R2, the room's
    /// event stream — precisely so the session can be put back on a machine
    /// later. `Archived -> Provisioning` is what `POST
    /// /v1/sessions/{id}/resume` performs; a session with no way back would
    /// make archiving a deletion, which is not what the product offers.
    ///
    /// [`Failed`](Self::Failed) is the same shape for the opposite reason: a
    /// session whose provisioning gave up can be retried (`Failed ->
    /// Provisioning`) or given up on (`Failed -> Archived`), and nothing
    /// else.
    ///
    /// `Interrupted -> Failed` is the one an interrupted session needs: its
    /// machine was reclaimed and flyco could not put it back, so it is not
    /// waiting for anything and the user has to decide what to do. Without
    /// it a recovery that ran out of attempts would leave the session
    /// looking like one that is still coming back.
    ///
    /// `Active -> Failed` is the same fact arriving a different way: the
    /// agent process on a live machine died, and the daemon said so on its
    /// way out (issue #193). The session is not paused, not interrupted and
    /// not archived — it stopped, for a reason worth reading — and only
    /// [`Failed`](Self::Failed) both releases the machine and carries the
    /// sentence explaining it.
    ///
    /// `Paused -> Provisioning` is what a usage-limit pause needs: the
    /// session's machine was released to cost nothing until the plan's
    /// window turns over, and ten minutes before it does flyco starts that
    /// machine again on its own disk. The session is genuinely provisioning
    /// while that happens — its daemon is not connected — and the move back
    /// to [`Active`](Self::Active) is the ordinary one every provisioning
    /// session makes when its daemon reaches the control plane.
    ///
    /// `Provisioning -> Interrupted` is the same fact arriving from the
    /// other side: a *recovery* runs through `Provisioning`, and the start
    /// it issued can be the call that learns the machine is gone — a
    /// codespace deleted while the start was in flight. The session is not
    /// failed: [`InterruptedReason::MachineLost`] is written beside it, and
    /// that reason is what tells the next resume to build a fresh machine
    /// rather than start the one it just lost.
    ///
    /// # Errors
    ///
    /// Returns [`SessionTransitionError`] when the move is not part of the
    /// lifecycle graph.
    pub const fn transition(self, to: Self) -> Result<Self, SessionTransitionError> {
        let allowed = matches!(
            (self, to),
            (
                Self::Provisioning,
                Self::Active | Self::Archived | Self::Failed | Self::Interrupted
            ) | (
                Self::Paused,
                Self::Active | Self::Archived | Self::Provisioning
            ) | (
                Self::Active,
                Self::Paused | Self::Interrupted | Self::Archived | Self::Failed
            ) | (
                Self::Interrupted,
                Self::Provisioning | Self::Archived | Self::Failed
            ) | (Self::Failed, Self::Provisioning | Self::Archived)
                | (Self::Archived, Self::Provisioning)
        );
        if allowed {
            Ok(to)
        } else {
            Err(SessionTransitionError { from: self, to })
        }
    }

    /// Whether the session still holds an execution environment.
    #[must_use]
    pub const fn holds_environment(self) -> bool {
        !matches!(self, Self::Archived | Self::Failed)
    }

    /// Whether the session can be put back on a machine.
    #[must_use]
    pub const fn is_resumable(self) -> bool {
        matches!(self, Self::Interrupted | Self::Archived | Self::Failed)
    }
}

/// Why a session lost the machine it was running on.
///
/// Recorded beside [`SessionState::Interrupted`] rather than folded into
/// it, because the state says the session is off its compute and this says
/// what the user is looking at: `Interrupted · spot reclaimed` is a status
/// flyco is already recovering from, and a status with no reason would read
/// as a session somebody has to rescue by hand (docs/ux.md §6).
///
/// Kept while flyco puts the session back — the recovery runs through
/// [`SessionState::Provisioning`], and the reason is what tells that
/// provisioning apart from a first one, which is the whole of `Migrating` —
/// and cleared when the session's daemon reaches the control plane again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "sql", derive(skyzen::Column))]
pub enum InterruptedReason {
    /// The provider reclaimed the session's interruptible capacity.
    ///
    /// The disk is kept — every provider flyco provisions spot on is
    /// configured to stop the machine rather than delete it — so the
    /// session is put back on the same disk rather than rebuilt.
    SpotReclaimed,
    /// The provider stopped the machine for inactivity.
    ///
    /// A Codespace stops itself once its idle timeout passes — no notice
    /// reaches the daemon, because nothing on the machine is asked. The
    /// disk is kept exactly as a reclamation's is, so the recovery is the
    /// same `start` — but nothing automatic asks for one, because a machine
    /// stopped for being unused should stay stopped until somebody uses it.
    Suspended,
    /// The provider no longer holds the machine at all.
    ///
    /// A Codespace past its retention period is *deleted*, disk included —
    /// there is nothing to start, so a resume builds the session a fresh
    /// machine rather than starting the one it had.
    MachineLost,
}

/// Why a session is [`SessionState::Paused`].
///
/// Recorded beside the state for the reason [`InterruptedReason`] is: the
/// state says the session is stopped on purpose, and this says what would
/// start it again. The two answers point the reader at completely different
/// things — a spent budget is a number only the user can raise, and a spent
/// plan window is a wait flyco ends by itself — so a single `Paused` pill
/// covering both would send half the people who read it to the wrong
/// control (docs/ux.md §6).
///
/// `None` is a session the user paused, which is the only pause with no
/// mechanism behind it and nothing to say beyond the word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "sql", derive(skyzen::Column))]
pub enum PausedReason {
    /// The session's compute budget is spent, and only a higher limit
    /// releases it.
    Budget,
    /// A window of the harness account's plan is spent.
    ///
    /// Flyco ends this pause itself: the window turns over at a stated
    /// instant, and [`UsageLimitPause`] carries everything the wait is
    /// scheduled around.
    UsageLimit,
}

/// How far ahead of a window's reset a stopped session's machine is
/// started again.
///
/// Ten minutes, which is longer than any provision flyco has measured from
/// a kept disk and short enough that the user is not paying for an idle
/// machine: the point is that the agent is *already up* when the window
/// turns over, so the first thing that happens at the reset is a turn and
/// not a boot.
pub const USAGE_LIMIT_WAKE_LEAD_SECS: u64 = 10 * 60;

/// How close a reset has to be for the session to keep its machine.
///
/// Below this the machine stays up and the session simply waits: stopping
/// and starting a machine costs minutes at both ends and a provider bills a
/// stopped disk anyway, so releasing compute for less than half an hour
/// buys the user a slower resume and almost no money. Above it the machine
/// is released, because a weekly window can be days out and a session must
/// not sit on paid compute doing nothing for days.
pub const USAGE_LIMIT_STOP_AFTER_SECS: u64 = 30 * 60;

/// What flyco says on the user's behalf when the window turns over.
///
/// Deliberately a plain instruction and not an explanation: the agent is
/// being asked to pick up the task it was working on, and a sentence about
/// rate limits would be context it has to reason about first. Used unless
/// the user typed something into the composer while the session was
/// waiting, in which case what they typed is sent instead — they had
/// something to say, and saying it is a better continuation than a canned
/// nudge.
pub const USAGE_LIMIT_CONTINUE_MESSAGE: &str = "usage limit reset, please continue";

/// Everything a session paused on a harness usage limit is waiting for.
///
/// Present exactly while [`SessionSummary::paused_reason`] is
/// [`PausedReason::UsageLimit`], and cleared when the window turns over and
/// the session is continued.
///
/// Whether the machine was released is *derived* from
/// [`resume_at_unix`](Self::resume_at_unix) rather than stored beside it:
/// there is a wake to schedule if and only if there is a machine to start,
/// so one field answers both questions and they cannot disagree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct UsageLimitPause {
    /// What the window that struck is called, as
    /// [`UsageWindow::label`](crate::wire::UsageWindow::label) names it —
    /// `5-hour`, `Weekly (Opus)`.
    ///
    /// The label and not the whole window: the percentages behind the rings
    /// are the account's and move while this session waits, and a copy
    /// frozen at the moment of the pause would be a second answer going
    /// stale. What the pause is about is *which* window, and that is a name.
    pub window: String,
    /// When that window turns over, seconds since the Unix epoch.
    pub resets_at_unix: u64,
    /// When flyco starts the machine again, seconds since the Unix epoch.
    ///
    /// `None` for a pause that kept the machine, which is every reset less
    /// than [`USAGE_LIMIT_STOP_AFTER_SECS`] away. Present means the machine
    /// was released and costs nothing until this instant, which is what the
    /// session page states.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_at_unix: Option<u64>,
    /// What the user typed while the session was waiting, if anything.
    ///
    /// The composer stays usable through a usage-limit pause, and what is
    /// typed into it is held here and sent as the continuation instead of
    /// [`USAGE_LIMIT_CONTINUE_MESSAGE`]. Shown back to the user so the
    /// message they queued is visibly queued rather than apparently lost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_message: Option<String>,
}

impl UsageLimitPause {
    /// The pause a limit reported at `now` calls for.
    ///
    /// The one place the two shapes of pause are decided, so the sweep that
    /// ends one and the handler that begins it cannot disagree about which
    /// it is looking at: a reset more than [`USAGE_LIMIT_STOP_AFTER_SECS`]
    /// away releases the machine and books a wake, and a nearer one keeps
    /// it.
    #[must_use]
    pub fn beginning(window: String, resets_at_unix: u64, now_unix: u64) -> Self {
        let stop = resets_at_unix.saturating_sub(now_unix) > USAGE_LIMIT_STOP_AFTER_SECS;
        Self {
            window,
            resets_at_unix,
            resume_at_unix: stop.then(|| resets_at_unix.saturating_sub(USAGE_LIMIT_WAKE_LEAD_SECS)),
            queued_message: None,
        }
    }

    /// Whether this pause released the session's compute.
    #[must_use]
    pub const fn machine_stopped(&self) -> bool {
        self.resume_at_unix.is_some()
    }

    /// What to say to the agent when the window turns over.
    #[must_use]
    pub fn continuation(&self) -> &str {
        self.queued_message
            .as_deref()
            .unwrap_or(USAGE_LIMIT_CONTINUE_MESSAGE)
    }
}

/// What a session is doing right now, as the home list reads it.
///
/// [`SessionState`] says where the machine is; this says whose move it is.
/// An `active` session may be thinking, waiting for a decision, or sitting
/// idle since yesterday, and docs/ux.md §6 gives those three different
/// statuses — so the fact is recorded rather than guessed from a lifecycle
/// enum that cannot tell them apart.
///
/// Maintained by the control plane from the turn events the session's
/// daemon reports (`turn-started`, `turn-completed`, `turn-failed`) and
/// from the messages the user sends, which is the whole of the
/// conversation's position. A *pending approval* is deliberately not
/// written here: the `approvals` table already owns that fact, and a copy
/// on the session row would be a second answer free to disagree with it.
/// It is applied on the way out instead, by
/// [`with_pending_approval`](Self::with_pending_approval).
///
/// Meaningless for a session that is not [`Active`](SessionState::Active):
/// archiving, pausing and interruption leave it exactly as it was, so a
/// session that comes back reads as whatever it was doing when it went, and
/// the UI ignores it for every other state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "sql", derive(skyzen::Column))]
pub enum SessionActivity {
    /// A turn is in flight: the agent is working and nobody is waiting.
    Working,
    /// The agent has stopped and it is the user's move — the last turn
    /// ended with no message after it, or an approval is pending.
    NeedsInput,
    /// Nothing is running and nobody is blocked: the user has spoken and
    /// no turn has started yet, or the session has never run one.
    Idle,
}

impl SessionActivity {
    /// The activity a session takes on when its daemon reports a turn
    /// event.
    ///
    /// The one mapping from a harness turn to what a person reads, so the
    /// three routes that receive those events cannot each decide it
    /// differently.
    #[must_use]
    pub const fn after_turn(started: bool) -> Self {
        if started {
            Self::Working
        } else {
            Self::NeedsInput
        }
    }

    /// The same activity, raised to [`NeedsInput`](Self::NeedsInput) when
    /// the session has an undecided approval against it.
    ///
    /// An approval blocks the agent whatever the turn was doing, so it wins
    /// over every stored position. Applied where a session is read rather
    /// than written, so deciding an approval needs no compensating write:
    /// the row goes back to the position the turn left it in by itself.
    #[must_use]
    pub const fn with_pending_approval(self, pending: bool) -> Self {
        if pending { Self::NeedsInput } else { self }
    }
}

/// Which machine a session asks for.
///
/// Named on [`CreateSession`] when the caller picks a type themselves. The
/// choice is validated against the named account's own catalog before
/// anything is written, so a machine the account cannot deploy is refused
/// where the user made the choice. Omitted, flyco picks a machine itself
/// with [`auto_linux_choice`](crate::machine::auto_linux_choice) instead of
/// guessing and resizing afterwards, and the session records that the
/// choice was flyco's ([`MachineOrigin::Auto`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MachineChoice {
    /// Which linked provider account to provision on.
    pub provider_account: ProviderAccountId,
    /// Provider-native machine type, as `GET /v1/machines/catalog` names it.
    pub machine_type: String,
    /// Whether that type is a virtual machine or a managed container, as
    /// the same catalog entry says.
    ///
    /// Sent rather than derived, and checked against the entry before the
    /// session is written: the two facts came off one row in the picker,
    /// and a request whose runtime disagrees with the type it names is a
    /// caller working from a catalog that has since changed — which is a
    /// refusal at the point of choice rather than a machine of the wrong
    /// shape. Defaulted to [`Runtime::Vm`], which is what every caller
    /// written before this axis existed means.
    #[serde(default)]
    pub runtime: crate::machine::Runtime,
    /// Provider-native region, as the catalog entry names it.
    pub region: String,
    /// Whether to ask for interruptible spot capacity. Spot is the default
    /// because it is the cheaper option and flyco handles eviction.
    ///
    /// A request, not a promise: a provider that cannot honour it is
    /// answered with on-demand capacity, and
    /// [`MachineView::spot`](crate::machine::MachineView::spot) is what was
    /// actually obtained.
    #[serde(default = "default_spot")]
    pub spot: bool,
    /// Disk size in GiB.
    #[serde(default = "default_disk_gib")]
    pub disk_gib: u32,
}

const fn default_spot() -> bool {
    true
}

/// How long a session may sit idle before flyco archives it automatically.
///
/// A week is long enough that a paused thought is not destroyed overnight,
/// and short enough that forgotten machines do not sit on a disk forever.
pub const ARCHIVE_AFTER_IDLE_SECS: u64 = 7 * 24 * 60 * 60;

/// How long a session that is *over* may sit before flyco archives it —
/// a day, against the week a session still in play gets.
///
/// [`Failed`](SessionState::Failed) is the only terminal state flyco
/// writes, and there is nothing to come back to on one: the machine is
/// already released and archiving keeps the transcript, so a resume from
/// `Archived` loses nothing a `Failed` session still had. A live session
/// that is merely idle might still be picked up — a paused thought, an
/// interrupted machine a resume could restart — which is what the longer
/// clock is for. Twenty-four hours is long enough to read what the
/// failure said and act on it; past that an unarchived failure is sidebar
/// clutter, and the sidebar is finite.
pub const ARCHIVE_FINISHED_AFTER_IDLE_SECS: u64 = 24 * 60 * 60;

/// How long an idle session keeps its machine's compute before flyco
/// suspends it.
///
/// A codespace is suspended by GitHub on roughly this clock already; every
/// other machine — an Azure VM, an AWS spot instance, a container on a
/// user's own host — bills or holds resources for as long as it runs, so
/// flyco stops it itself. Thirty minutes is the same allowance GitHub
/// gives, long enough that reading what a turn produced and thinking about
/// the next message never loses the machine, and the disk is always kept:
/// suspension interrupts the session, it does not end it.
pub const SUSPEND_AFTER_IDLE_SECS: u64 = 30 * 60;

/// The longest a user may hold a machine awake in one go, in minutes.
///
/// Eight hours: long enough for a build, a soak test or a watch loop to
/// finish unattended, and short enough that a hold forgotten at the end of
/// a day is over before the next one starts. A hold is renewed by asking
/// again, which is a decision made while looking at the bill rather than
/// one made once and never revisited.
pub const KEEP_AWAKE_MAX_MINUTES: u32 = 8 * 60;

/// How long a session may be built for before flyco calls it failed.
///
/// A machine that is reserved, booted, installed and cloned and still has
/// not said its agent is ready is not slow, it is broken: the daemon is
/// crash-looping, the image is wrong, or the machine cannot reach the
/// control plane. Fifteen minutes is several times the worst honest
/// provision — a cold image plus a large repository — so nothing legitimate
/// is cut short, and a session past it is told what happened rather than
/// left spinning under a timeline that never advances.
pub const PROVISION_DEADLINE_SECS: u64 = 15 * 60;

/// Disk a session gets when the request names no size.
///
/// Big enough for a repository, a toolchain and a build cache, which is what
/// a coding agent fills a disk with; small enough that the default is not an
/// expensive one.
pub const DEFAULT_DISK_GIB: u32 = 64;

const fn default_disk_gib() -> u32 {
    DEFAULT_DISK_GIB
}

/// How the machine a session runs on was chosen.
///
/// Persisted because the two are not interchangeable afterwards: a machine
/// the user picked is a decision flyco must not quietly undo, while an
/// automatic one is flyco's own guess and is free to be revisited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "sql", derive(skyzen::Column))]
pub enum MachineOrigin {
    /// Flyco chose it, with [`auto_linux_choice`](crate::machine::auto_linux_choice).
    Auto,
    /// The request named a machine type, so the user chose it.
    User,
}

/// How much of a prompt names the turn it began, in
/// [`TurnSummary::prompt_excerpt`].
///
/// Enough to recognise the turn in a list, short enough that a history of
/// fifty turns is not a transcript in its own right.
pub const PROMPT_EXCERPT_CHARS: usize = 200;

/// Longest title a session may carry.
///
/// The same bound whether the title was derived from the first prompt or
/// typed by hand, so a renamed session can never be longer than one flyco
/// named itself.
pub const MAX_SESSION_TITLE_CHARS: usize = 120;

/// The opening of a piece of text, at most `max_chars` characters including
/// the ellipsis that marks the cut.
///
/// The one place flyco shortens prose for a list: a turn's
/// [`prompt_excerpt`](TurnSummary::prompt_excerpt) and a session's default
/// [`title`](SessionSummary::title) are the same operation at two lengths.
///
/// # Panics
///
/// Panics if `max_chars` is zero, which would leave nowhere to put the
/// ellipsis.
#[must_use]
pub fn excerpt(text: &str, max_chars: usize) -> String {
    assert!(max_chars > 0, "an excerpt of zero characters says nothing");
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_owned();
    }
    let end = trimmed
        .char_indices()
        .nth(max_chars - 1)
        .map_or(trimmed.len(), |(index, _)| index);
    format!("{}\u{2026}", trimmed[..end].trim_end())
}

/// Request body of `POST /v1/sessions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CreateSession {
    /// What the agent should do first.
    ///
    /// Required, because a session with nothing to do is a machine nobody
    /// asked for: the prompt is recorded as the session's first user
    /// message and delivered to the daemon as soon as one connects. Its
    /// excerpt is also the session's opening [`title`](SessionSummary::title).
    pub prompt: String,
    /// Which coding harness drives the session.
    pub harness: HarnessKind,
    /// Repositories the session works across, primary first.
    ///
    /// A session always has at least one: the workdir is laid out as a
    /// workspace holding every checkout, and the first entry is the
    /// repository the header names. Untrusted input — the control plane
    /// parses each `repo` into a [`RepoSlug`](crate::repo::RepoSlug) and
    /// each `branch` into a [`BranchName`](crate::repo::BranchName), and
    /// refuses anything else.
    pub repos: Vec<RepoSelection>,
    /// Spending limit for the whole session.
    pub budget_limit: Usd,
    /// The machine to provision for it.
    ///
    /// Omitted, flyco picks the cheapest Linux type in the caller's catalog
    /// of at least
    /// [`AUTO_MIN_VCPUS`](crate::machine::AUTO_MIN_VCPUS) vCPUs and
    /// [`AUTO_MIN_MEMORY_MIB`](crate::machine::AUTO_MIN_MEMORY_MIB) of
    /// memory, and records the machine as automatically chosen.
    #[serde(default)]
    pub machine: Option<MachineChoice>,
    /// Whether to ask for interruptible spot capacity when flyco picks the
    /// machine. Ignored when [`Self::machine`] names a type, because that
    /// choice already carries its own `spot`.
    #[serde(default = "default_spot")]
    pub spot: bool,
    /// The model the session runs on, and the effort it runs at.
    ///
    /// Omitted, the control plane records the default of the harness
    /// account's own model list — resolved *here*, once, so the session row
    /// always names what it is running rather than deferring to whatever
    /// the CLI happens to default to on the day its machine boots.
    #[serde(default)]
    pub model: Option<ModelChoice>,
    /// The approval policy the session's agent runs under.
    ///
    /// Omitted, the session opens on
    /// [`PermissionMode::PRODUCT_DEFAULT`](crate::harness::PermissionMode::PRODUCT_DEFAULT).
    /// Named at creation because an autonomous caller cannot answer a
    /// permission prompt: a mode has to be on the row before the machine
    /// boots, not patched in after the first turn is already waiting on
    /// one.
    #[serde(default)]
    pub permission_mode: Option<crate::harness::PermissionMode>,
    /// Where the session's context comes from — absent for a fresh
    /// session, [`SessionSource::LocalHandoff`](crate::handoff::SessionSource)
    /// when `flyco handoff` is importing a local session's state. A
    /// handoff session holds its provisioning until `handoff/complete`
    /// says the patch and transcript objects landed.
    #[serde(default)]
    pub source: Option<crate::handoff::SessionSource>,
}

/// A session in a list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SessionSummary {
    /// Identifier.
    pub id: SessionId,
    /// What the session is called in a list.
    ///
    /// Opens as the excerpt of the first prompt and is editable through
    /// `PATCH /v1/sessions/{id}`. Never empty: a row with no name would
    /// leave every list entry identified by a UUID.
    pub title: String,
    /// Whether flyco or the user chose the machine it runs on.
    pub machine_origin: MachineOrigin,
    /// Which coding harness drives it.
    pub harness: HarnessKind,
    /// Repositories it works across, in `session_repos` order.
    ///
    /// The first entry is the session's primary repository — the one the
    /// header renders as `repo · branch` (docs/ux.md §9.1), with `+N` for
    /// the rest. Never empty: a session is created with at least one
    /// repository and the list only grows when one is added later.
    pub repos: Vec<SessionRepo>,
    /// Where it is in its lifecycle.
    pub state: SessionState,
    /// What it is doing, for an [`Active`](SessionState::Active) session.
    ///
    /// What lets the home list say `Working` and `Needs input` rather than
    /// reading every running session as `Idle`: the facts behind those two
    /// live in the relay and in the `approvals` table, and this is the
    /// control plane's own answer, carried on the row the list is built
    /// from (docs/ux.md §6).
    ///
    /// Meaningless for every other state, and the UI ignores it there — a
    /// session keeps the activity it had when it was paused, interrupted or
    /// archived rather than being reset to a position it was never in.
    pub activity: SessionActivity,
    /// Why the session lost its machine, while it is off one or being put
    /// back on one.
    ///
    /// `None` for a session that never lost a machine. Present through both
    /// halves of a reclamation — the
    /// [`Interrupted`](SessionState::Interrupted) wait and the
    /// [`Provisioning`](SessionState::Provisioning) that recovers from it —
    /// so the UI renders `Interrupted · spot reclaimed` and then
    /// `Migrating` from one field rather than guessing which kind of
    /// provisioning it is watching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted_reason: Option<InterruptedReason>,
    /// Why the session is [`Paused`](SessionState::Paused).
    ///
    /// On the summary rather than only on [`SessionDetail`] because the
    /// status dot in the rail is drawn from a summary, and a session waiting
    /// out a plan window is not the same status as one that ran out of money
    /// (docs/ux.md §6). `None` for every session that is not paused, and for
    /// a pause the user asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_reason: Option<PausedReason>,
    /// When it was created, seconds since the Unix epoch.
    pub created_at_unix: u64,
    /// Last time anything happened on it, seconds since the Unix epoch.
    pub last_active_unix: u64,
    /// What it runs on, and at what effort.
    ///
    /// Always concrete, never "whatever the CLI picks": a session opened
    /// without a model named carries the harness's default resolved at
    /// creation, so the header can state the model on every row rather than
    /// leaving one blank for the sessions nobody chose one for.
    pub model: ModelChoice,
    /// The permission mode the session's agent runs under.
    ///
    /// Always concrete for the same reason `model` is: a session opened
    /// before flyco recorded a mode resolves to
    /// [`PermissionMode::PRODUCT_DEFAULT`](crate::harness::PermissionMode::PRODUCT_DEFAULT)
    /// at read, so every row states the mode it is on rather than leaving
    /// the composer's chip to guess.
    pub permission_mode: crate::harness::PermissionMode,
    /// Until when the idle sweep must leave this session's machine alone,
    /// seconds since the Unix epoch.
    ///
    /// `None` for a session on the ordinary clock, which is almost all of
    /// them. A machine is stopped after
    /// [`SUSPEND_AFTER_IDLE_SECS`] because compute bills by the minute,
    /// and that is wrong exactly when the agent is doing something the
    /// control plane cannot see it doing — a long build, a soak test, a
    /// watch loop — so the user holds the machine open for a while. An
    /// instant rather than a flag: a machine held awake for ever is a bill
    /// nobody chose, and the hold has to expire on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub awake_until_unix: Option<u64>,
    /// Whether the session may have a desktop.
    ///
    /// On the summary rather than only on [`SessionDetail`] because the
    /// `Screen` drawer entry is drawn from it: a session without the flag
    /// has no panel to open. Every session created today carries it — the
    /// stack ships in the image and is not a choice at create — but rows
    /// from before that change record 0, and a live session can still
    /// toggle it through [`UpdateSession::computer_use`].
    pub computer_use: bool,
}

/// A single session, with its budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SessionDetail {
    /// Everything a list entry carries.
    #[serde(flatten)]
    pub summary: SessionSummary,
    /// Budget accounting as of this request.
    pub budget: BudgetView,
    /// Why the session is [`SessionState::Failed`], in the provider's own
    /// words where it has any.
    ///
    /// `None` for every other state. A failed session that could not say
    /// why would leave the user with a dead session and no idea whether to
    /// retry it, pick another region, or ask for a quota increase.
    pub failure: Option<String>,
    /// What a session waiting out a harness usage limit is waiting for.
    ///
    /// `Some` exactly while
    /// [`paused_reason`](SessionSummary::paused_reason) is
    /// [`PausedReason::UsageLimit`], and on the detail rather than the
    /// summary because this is the page's story and not the rail's dot: a
    /// list row says the session is waiting, and the page says which window,
    /// until when, and whether the machine is still costing anything.
    ///
    /// It outlives the [`Paused`](SessionState::Paused) state on purpose.
    /// The wake runs through [`Provisioning`](SessionState::Provisioning),
    /// and this is the only thing that tells that provisioning apart from a
    /// first one — the same job [`InterruptedReason`] does for a
    /// reclamation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_limit: Option<UsageLimitPause>,
}

/// What `GET /v1/sessions/{id}/harness-session` answers.
///
/// The conversation a daemon starting on this session must continue, as the
/// control plane last recorded it. Asked rather than read out of the
/// daemon's own configuration, because that file was written when the
/// machine was *created*: a machine that was stopped and started again on
/// the same disk — which is what recovering from a spot reclamation is —
/// boots the same file, and a daemon that trusted it would open a second
/// conversation beside the one the user is watching.
///
/// `None` is a session whose harness has never announced an identity, which
/// is every session until its first daemon connects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HarnessSessionView {
    /// Harness-native session id; resume reopens this conversation.
    pub harness_session_id: Option<String>,
    /// The model this session runs on, as the control plane last recorded
    /// it.
    ///
    /// Asked for the same reason the conversation is: the model in the
    /// configuration on the machine's disk is the one it was *provisioned*
    /// with, so a session whose model was changed while it ran would come
    /// back on the old one after a reclamation.
    pub model: ModelChoice,
    /// The permission mode this session runs under, as the control plane
    /// last recorded it.
    ///
    /// Same reason as `model`: the configuration on disk is the one the
    /// machine was provisioned with, so a session put on `plan` while it
    /// ran would otherwise come back on whatever it was provisioned under.
    pub permission_mode: crate::harness::PermissionMode,
}

/// Request body of `PUT /v1/sessions/{id}/awake`.
///
/// A route of its own rather than a field on [`UpdateSession`]: this is
/// not a property of the session the user is editing, it is an instruction
/// to the idle sweep with a clock attached, and the two are asked for from
/// different places and answered at different times.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct KeepAwake {
    /// How much longer the machine must not be suspended for idleness, in
    /// minutes, up to [`KEEP_AWAKE_MAX_MINUTES`].
    ///
    /// `None` ends the hold and gives the machine back to the sweep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minutes: Option<u32>,
}

/// Request body of `PATCH /v1/sessions/{id}`.
///
/// All three fields are independently optional, because the things a user
/// changes about a live session are changed from different parts of the UI
/// and none has any business restating another's value. A body carrying
/// none of them is refused rather than answered with a session nothing
/// happened to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct UpdateSession {
    /// What to call the session, 1 to
    /// [`MAX_SESSION_TITLE_CHARS`] characters once trimmed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// What the session may spend on compute, in microdollars.
    ///
    /// The one thing that releases a session paused on an exhausted
    /// budget: a limit above what the ledger has already spent puts the
    /// session back to [`SessionState::Active`] and tells its daemon to
    /// carry on. A limit that is still under the spend is accepted and
    /// changes nothing else — the session stays paused, because it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_limit: Option<Usd>,
    /// What the session should run on from here on.
    ///
    /// Takes effect on the conversation already in progress rather than at
    /// the next turn's discretion: the control plane records it and sends
    /// the session's daemon a
    /// [`SetModel`](crate::wire::ControlToDaemon::SetModel), which both
    /// harnesses apply to the running session. Refused when the account's
    /// own model list does not offer it, so a stale picker cannot put a
    /// session on a model its harness has dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelChoice>,
    /// The permission mode the session's agent should run under.
    ///
    /// Applied on the same terms as `model`: the control plane records it
    /// and sends the session's daemon a
    /// [`SetPermissionMode`](crate::wire::ControlToDaemon::SetPermissionMode),
    /// which Claude applies to the live query and Codex applies from the
    /// next `turn/start`. Every declared mode is one both harnesses honor —
    /// Codex translates it into its approval/sandbox pair — so the body
    /// needs no per-harness validation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<crate::harness::PermissionMode>,
    /// Give the session a screen, or take it away.
    ///
    /// Applied to the session already in progress: the control plane
    /// records it and sends the daemon a
    /// [`SetComputerUse`](crate::wire::ControlToDaemon::SetComputerUse),
    /// which starts or stops the display stack without a reboot. Turning
    /// it off does not kill anything the agent started on the display —
    /// the screen is gone, and whatever was on it went with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computer_use: Option<bool>,
}

/// Request body of `POST /v1/sessions/{id}/messages`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SendMessage {
    /// What to say to the agent. A leading `!` is the terminal escape the
    /// UI documents; the control plane forwards the text either way and the
    /// daemon decides.
    pub text: String,
}

/// Request body of `POST /v1/sessions/{id}/shell`.
///
/// The composer's `!` escape: a command for the session's machine, not for
/// the agent. The room assigns the run id it will be tracked under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RunShell {
    /// The command, without the `!`, to be run through `bash -c`.
    pub command: String,
}

/// Request body of `POST /v1/sessions/{id}/terminal/input`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TerminalInput {
    /// Bytes to write to the terminal, UTF-8.
    pub data: String,
}

/// Request body of `POST /v1/sessions/{id}/terminal/resize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TerminalSize {
    /// Columns the pane shows.
    pub cols: u16,
    /// Rows the pane shows.
    pub rows: u16,
}

/// Request body of `POST /v1/sessions/{id}/terminal/harness`.
///
/// The `flyco claude`/`flyco codex`/`flyco resume` path: put the session's
/// harness TUI in the terminal's foreground. The daemon supplies the
/// credentials from its own configuration, so the body carries none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HarnessTui {
    /// Re-enter the last conversation (`claude --continue`,
    /// `codex resume --last`) rather than start a new one. Also makes the
    /// request an *ensure*: a TUI already in the foreground is left alone,
    /// because re-attaching to a session must not kill the turn on its
    /// screen.
    #[serde(default, skip_serializing_if = "crate::wire::is_false")]
    pub resume: bool,
}

/// Request body of `POST /v1/sessions/{id}/desktop/takeover`.
///
/// Takeover is scoped to a watcher — the lease the desktop stream minted
/// — because the screen belongs to an open viewer, not to the account:
/// a browser that dies mid-takeover hands the screen back when its
/// watcher expires, rather than locking the agent out of a session
/// nobody is looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DesktopTakeoverRequest {
    /// The watcher id the desktop stream's `hello` event named.
    pub watcher: u64,
    /// Whether the user is taking the screen (`true`) or handing it back.
    pub active: bool,
}

/// Request body of `POST /v1/sessions/{id}/desktop/input`.
///
/// One batch of user input for the desktop — the keystrokes and pointer
/// moves the screen panel collected since its last send. The room
/// forwards it only while the named watcher holds takeover, which is
/// what keeps a second open tab from reaching into the screen the first
/// one is driving.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DesktopInputRequest {
    /// The watcher id the desktop stream's `hello` event named.
    pub watcher: u64,
    /// The input, in display coordinates.
    pub events: Vec<crate::wire::DesktopInputEvent>,
}

/// One turn of a session, as the history list renders it.
///
/// Folded out of the event stream the session's room records rather than
/// read from a table of its own: that stream is what survives a machine and
/// what a browser replays, so a turn list built from anything else would
/// disagree with the conversation shown beside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TurnSummary {
    /// Harness-native turn identifier, which is what the transcript keys on.
    pub turn_id: String,
    /// When the turn started, seconds since the Unix epoch.
    pub started_at_unix: u64,
    /// When it finished, if it has.
    pub completed_at_unix: Option<u64>,
    /// The opening of the user message that began the turn, for the list.
    pub prompt_excerpt: String,
    /// Token accounting after the turn, when it completed.
    pub usage: Option<UsageReport>,
}

/// One page of `GET /v1/sessions/{id}/turns`.
///
/// The cursor is opaque: it encodes a position in the session's recorded
/// event stream, and a client that stores it must hand it back unread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TurnPage {
    /// The turns, oldest first.
    pub turns: Vec<TurnSummary>,
    /// Cursor to pass as `cursor` for the next page, or `None` at the end.
    pub next_cursor: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::ProviderAccountId;

    #[test]
    fn spot_defaults_to_on_when_the_client_omits_it() {
        let account = ProviderAccountId::generate();
        let request: CreateSession = serde_json::from_str(&format!(
            r#"{{"prompt":"add a test","harness":"claude_code",
                "repos":[{{"repo":"lexoliu/flyco"}}],
                "budget_limit":10000000,
                "machine":{{"provider_account":"{account}","machine_type":"Standard_B2ats_v2",
                            "region":"northcentralus"}}}}"#
        ))
        .expect("deserialize");
        let machine = request.machine.expect("the JSON named a machine");
        assert!(machine.spot);
        assert_eq!(machine.disk_gib, DEFAULT_DISK_GIB);
        assert!(request.spot);
    }

    #[test]
    fn omitting_the_machine_lets_flyco_choose() {
        let request: CreateSession = serde_json::from_str(
            r#"{"prompt":"add a test","harness":"claude_code",
                "repos":[{"repo":"lexoliu/flyco"}],
                "budget_limit":10000000}"#,
        )
        .expect("deserialize");
        assert!(request.machine.is_none());
        assert!(request.spot);
        assert!(
            request.repos[0].branch.is_none(),
            "a request that names no branch takes the repository's default"
        );
    }

    #[test]
    fn a_named_branch_survives_deserialization() {
        let request: CreateSession = serde_json::from_str(
            r#"{"prompt":"add a test","harness":"claude_code",
                "repos":[{"repo":"lexoliu/flyco","branch":"dev"}],
                "budget_limit":10000000}"#,
        )
        .expect("deserialize");
        assert_eq!(request.repos[0].branch.as_deref(), Some("dev"));
    }

    #[test]
    fn several_repositories_keep_their_order() {
        let request: CreateSession = serde_json::from_str(
            r#"{"prompt":"add a test","harness":"claude_code",
                "repos":[{"repo":"lexoliu/flyco"},{"repo":"lexoliu/aither","branch":"main"}],
                "budget_limit":10000000}"#,
        )
        .expect("deserialize");
        assert_eq!(request.repos.len(), 2);
        assert_eq!(request.repos[0].repo, "lexoliu/flyco");
        assert_eq!(request.repos[1].branch.as_deref(), Some("main"));
    }

    #[test]
    fn a_session_cannot_be_opened_without_a_prompt() {
        assert!(
            serde_json::from_str::<CreateSession>(
                r#"{"harness":"claude_code","repos":[{"repo":"lexoliu/flyco"}],"budget_limit":10000000}"#,
            )
            .is_err(),
            "a session with nothing to do is a machine nobody asked for"
        );
    }

    #[test]
    fn an_excerpt_never_exceeds_the_length_it_was_given() {
        let long = "x".repeat(MAX_SESSION_TITLE_CHARS * 2);
        let short = excerpt(&long, MAX_SESSION_TITLE_CHARS);
        assert_eq!(short.chars().count(), MAX_SESSION_TITLE_CHARS);
        assert!(short.ends_with('\u{2026}'));
    }

    #[test]
    fn an_excerpt_that_fits_is_only_trimmed() {
        assert_eq!(excerpt("  spaced  ", PROMPT_EXCERPT_CHARS), "spaced");
        assert_eq!(
            excerpt("\u{77ed}\u{3044}", PROMPT_EXCERPT_CHARS),
            "\u{77ed}\u{3044}"
        );
        assert_eq!(excerpt("abcdef", 6), "abcdef");
    }

    #[test]
    fn an_excerpt_cuts_on_a_character_boundary() {
        // Cutting on a byte index would split the second character in half
        // and panic; the cut is counted in characters for exactly that
        // reason.
        assert_eq!(excerpt("\u{65e5}\u{672c}\u{8a9e}", 2), "\u{65e5}\u{2026}");
    }

    #[test]
    fn machine_origin_uses_the_tokens_the_schema_stores() {
        assert_eq!(
            serde_json::to_value(MachineOrigin::Auto).expect("serialize"),
            serde_json::Value::String("auto".to_owned())
        );
        assert_eq!(
            serde_json::to_value(MachineOrigin::User).expect("serialize"),
            serde_json::Value::String("user".to_owned())
        );
    }

    #[test]
    fn a_summary_that_never_lost_a_machine_says_nothing_about_why() {
        // The field is skipped rather than serialized as null: a session
        // that was never interrupted has no reason, and `"interrupted_reason":
        // null` in every list row would be a fact about nothing.
        let summary = SessionSummary {
            id: SessionId::generate(),
            title: "add a test".to_owned(),
            machine_origin: MachineOrigin::Auto,
            harness: HarnessKind::ClaudeCode,
            repos: vec![SessionRepo {
                slug: "lexoliu/flyco".parse().expect("a repository"),
                branch: None,
                dir: "flyco".to_owned(),
                added_by: crate::repo::RepoAddedBy::User,
            }],
            state: SessionState::Active,
            activity: SessionActivity::Idle,
            interrupted_reason: None,
            paused_reason: None,
            created_at_unix: 0,
            last_active_unix: 0,
            model: ModelChoice {
                model: "default".to_owned(),
                effort: None,
            },
            permission_mode: crate::PermissionMode::Auto,
            computer_use: false,
            awake_until_unix: None,
        };
        let json = serde_json::to_string(&summary).expect("serialize");
        assert!(!json.contains("interrupted_reason"), "{json}");

        let reclaimed = SessionSummary {
            state: SessionState::Interrupted,
            interrupted_reason: Some(InterruptedReason::SpotReclaimed),
            ..summary
        };
        let json = serde_json::to_string(&reclaimed).expect("serialize");
        assert!(
            json.contains(r#""interrupted_reason":"spot_reclaimed""#),
            "{json}"
        );
        let back: SessionSummary = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, reclaimed);

        // The same rule for the other reason a session is not running: an
        // active row says nothing about why it might have been paused, and a
        // paused one names the mechanism that will release it.
        let waiting = SessionSummary {
            state: SessionState::Paused,
            interrupted_reason: None,
            paused_reason: Some(PausedReason::UsageLimit),
            ..back
        };
        let json = serde_json::to_string(&waiting).expect("serialize");
        assert!(json.contains(r#""paused_reason":"usage_limit""#), "{json}");
        assert_eq!(
            serde_json::from_str::<SessionSummary>(&json).expect("deserialize"),
            waiting
        );
    }

    #[test]
    fn activities_use_the_tokens_the_schema_stores() {
        for (activity, token) in [
            (SessionActivity::Working, "working"),
            (SessionActivity::NeedsInput, "needs_input"),
            (SessionActivity::Idle, "idle"),
        ] {
            assert_eq!(
                serde_json::to_value(activity).expect("serialize"),
                serde_json::Value::String(token.to_owned())
            );
        }
    }

    #[test]
    fn a_turn_event_says_whose_move_it_is() {
        assert_eq!(SessionActivity::after_turn(true), SessionActivity::Working);
        assert_eq!(
            SessionActivity::after_turn(false),
            SessionActivity::NeedsInput,
            "a turn that ended with nothing said back is the user's move"
        );
    }

    #[test]
    fn a_pending_approval_outranks_whatever_the_turn_was_doing() {
        for activity in [
            SessionActivity::Working,
            SessionActivity::NeedsInput,
            SessionActivity::Idle,
        ] {
            assert_eq!(
                activity.with_pending_approval(true),
                SessionActivity::NeedsInput,
                "an undecided approval blocks the agent whatever {activity:?} said"
            );
            assert_eq!(
                activity.with_pending_approval(false),
                activity,
                "deciding the last approval puts the session back where the turn left it"
            );
        }
    }

    #[test]
    fn states_use_the_tokens_the_schema_stores() {
        for (state, token) in [
            (SessionState::Provisioning, "provisioning"),
            (SessionState::Active, "active"),
            (SessionState::Paused, "paused"),
            (SessionState::Interrupted, "interrupted"),
            (SessionState::Archived, "archived"),
            (SessionState::Failed, "failed"),
        ] {
            assert_eq!(
                serde_json::to_value(state).expect("serialize"),
                serde_json::Value::String(token.to_owned())
            );
        }
    }

    #[test]
    fn happy_path_lifecycle() {
        let s = SessionState::Provisioning;
        let s = s.transition(SessionState::Active).expect("provisioned");
        let s = s.transition(SessionState::Interrupted).expect("evicted");
        let s = s.transition(SessionState::Provisioning).expect("resuming");
        let s = s.transition(SessionState::Active).expect("resumed");
        let s = s.transition(SessionState::Paused).expect("budget pause");
        let s = s.transition(SessionState::Archived).expect("archive");
        assert!(!s.holds_environment());
    }

    #[test]
    fn an_archived_session_comes_back_through_provisioning() {
        // History keeps an archived session's turns so it can be resumed;
        // the only way back is the same queue every other machine comes
        // from, which is why this is the one move archived allows.
        let s = SessionState::Archived
            .transition(SessionState::Provisioning)
            .expect("resume an archived session");
        assert!(s.transition(SessionState::Active).is_ok());

        for to in [
            SessionState::Active,
            SessionState::Paused,
            SessionState::Interrupted,
            SessionState::Failed,
        ] {
            assert!(
                SessionState::Archived.transition(to).is_err(),
                "an archived session may only be resumed, not moved to {to:?}"
            );
        }
    }

    #[test]
    fn a_failed_provision_can_be_retried_or_given_up_on() {
        assert!(
            SessionState::Provisioning
                .transition(SessionState::Failed)
                .is_ok()
        );
        assert!(
            SessionState::Failed
                .transition(SessionState::Provisioning)
                .is_ok()
        );
        assert!(
            SessionState::Failed
                .transition(SessionState::Archived)
                .is_ok()
        );
        assert!(
            SessionState::Failed
                .transition(SessionState::Active)
                .is_err(),
            "a failed session has no machine, so it cannot become active"
        );
        assert!(!SessionState::Failed.holds_environment());
    }

    #[test]
    fn only_a_session_off_its_machine_is_resumable() {
        for state in [
            SessionState::Interrupted,
            SessionState::Archived,
            SessionState::Failed,
        ] {
            assert!(state.is_resumable());
        }
        for state in [
            SessionState::Provisioning,
            SessionState::Active,
            SessionState::Paused,
        ] {
            assert!(!state.is_resumable());
        }
    }

    #[test]
    fn a_reclaimed_session_flyco_cannot_recover_ends_up_failed() {
        // The recovery gave up, so the session is not waiting for anything
        // and the user has to decide: retry it, or archive it.
        let failed = SessionState::Interrupted
            .transition(SessionState::Failed)
            .expect("a recovery that ran out of attempts fails the session");
        assert!(failed.transition(SessionState::Provisioning).is_ok());
        assert!(failed.transition(SessionState::Archived).is_ok());
    }

    #[test]
    fn cannot_skip_provisioning_when_resuming_interrupted() {
        assert!(
            SessionState::Interrupted
                .transition(SessionState::Active)
                .is_err()
        );
    }

    #[test]
    fn a_session_waiting_out_a_plan_window_can_be_put_back_on_its_machine() {
        // The wake ten minutes before the reset starts the machine again,
        // and the session is provisioning while that happens.
        assert!(
            SessionState::Paused
                .transition(SessionState::Provisioning)
                .is_ok()
        );
    }

    /// A weekly window, far enough out that paying for the machine is not
    /// on the table.
    #[test]
    fn a_reset_days_away_releases_the_machine_and_books_a_wake() {
        let now = 1_800_000_000;
        let resets = now + 3 * 24 * 60 * 60;
        let pause = UsageLimitPause::beginning("Weekly".to_owned(), resets, now);

        assert!(pause.machine_stopped());
        assert_eq!(
            pause.resume_at_unix,
            Some(resets - USAGE_LIMIT_WAKE_LEAD_SECS)
        );
        assert_eq!(pause.continuation(), USAGE_LIMIT_CONTINUE_MESSAGE);
    }

    #[test]
    fn a_reset_within_half_an_hour_keeps_the_machine_and_books_nothing() {
        let now = 1_800_000_000;
        let pause = UsageLimitPause::beginning("5-hour".to_owned(), now + 12 * 60, now);

        assert!(!pause.machine_stopped());
        assert!(
            pause.resume_at_unix.is_none(),
            "there is no machine to start, so there is nothing to wake for"
        );
    }

    #[test]
    fn exactly_the_threshold_keeps_the_machine() {
        // The boundary is stated once, and `>` is what makes "less than
        // half an hour" and "half an hour" both keep the machine: below the
        // threshold the stop buys nothing, and at it, nothing either.
        let now = 1_800_000_000;
        let pause =
            UsageLimitPause::beginning("5-hour".to_owned(), now + USAGE_LIMIT_STOP_AFTER_SECS, now);
        assert!(!pause.machine_stopped());
    }

    #[test]
    fn what_the_user_typed_while_waiting_is_what_gets_sent() {
        let now = 1_800_000_000;
        let mut pause = UsageLimitPause::beginning("Weekly".to_owned(), now + 86_400, now);
        pause.queued_message = Some("carry on with the migration".to_owned());
        assert_eq!(pause.continuation(), "carry on with the migration");
    }

    #[test]
    fn a_pause_that_kept_its_machine_serializes_without_the_fields_it_has_no_answer_for() {
        let now = 1_800_000_000;
        let json = serde_json::to_value(UsageLimitPause::beginning(
            "5-hour".to_owned(),
            now + 600,
            now,
        ))
        .expect("serialize");
        assert!(json.get("resume_at_unix").is_none());
        assert!(json.get("queued_message").is_none());
        assert_eq!(json["window"], "5-hour");
    }
}
