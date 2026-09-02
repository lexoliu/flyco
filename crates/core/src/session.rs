//! Session lifecycle.
//!
//! The state machine is enforced here so the control plane can never
//! record an impossible transition — an invalid one is an error at the
//! point of the bug, not a corrupt row discovered later.

use serde::{Deserialize, Serialize};

use crate::budget::BudgetView;
use crate::harness::{HarnessKind, UsageReport};
use crate::id::{ProviderAccountId, SessionId};
use crate::money::Usd;
use crate::repo::RepoSlug;

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
    /// # Errors
    ///
    /// Returns [`SessionTransitionError`] when the move is not part of the
    /// lifecycle graph.
    pub const fn transition(self, to: Self) -> Result<Self, SessionTransitionError> {
        let allowed = matches!(
            (self, to),
            (
                Self::Provisioning,
                Self::Active | Self::Archived | Self::Failed
            ) | (Self::Paused, Self::Active | Self::Archived)
                | (
                    Self::Active,
                    Self::Paused | Self::Interrupted | Self::Archived
                )
                | (
                    Self::Interrupted | Self::Failed,
                    Self::Provisioning | Self::Archived
                )
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

/// Which machine a session asks for.
///
/// Named on [`CreateSession`] when the caller picks a type themselves. The
/// choice is validated against the named account's own catalog before
/// anything is written, so a machine the account cannot deploy is refused
/// where the user made the choice. Omitted, flyco picks the cheapest
/// deployable Linux type instead of guessing and resizing afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MachineChoice {
    /// Which linked provider account to provision on.
    pub provider_account: ProviderAccountId,
    /// Provider-native machine type, as `GET /v1/machines/catalog` names it.
    pub machine_type: String,
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
    /// Repository to work in, `owner/name`. Untyped here because it is
    /// untrusted input; the control plane parses it into a
    /// [`RepoSlug`](crate::repo::RepoSlug) and rejects anything else.
    pub repo: String,
    /// Spending limit for the whole session.
    pub budget_limit: Usd,
    /// The machine to provision for it. Omitted, flyco picks the cheapest
    /// deployable Linux type from the caller's catalog.
    #[serde(default)]
    pub machine: Option<MachineChoice>,
    /// Whether to ask for interruptible spot capacity when flyco picks the
    /// machine. Ignored when [`Self::machine`] names a type, because that
    /// choice already carries its own `spot`.
    #[serde(default = "default_spot")]
    pub spot: bool,
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
    /// Repository it works in.
    pub repo: RepoSlug,
    /// Where it is in its lifecycle.
    pub state: SessionState,
    /// When it was created, seconds since the Unix epoch.
    pub created_at_unix: u64,
    /// Last time anything happened on it, seconds since the Unix epoch.
    pub last_active_unix: u64,
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
}

/// Request body of `PATCH /v1/sessions/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct UpdateSession {
    /// What to call the session, 1 to
    /// [`MAX_SESSION_TITLE_CHARS`] characters once trimmed.
    pub title: String,
}

/// Request body of `POST /v1/sessions/{id}/messages`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SendMessage {
    /// What to say to the agent. A leading `!` is the terminal escape the
    /// UI documents; the control plane forwards the text either way and the
    /// daemon decides.
    pub text: String,
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
            r#"{{"prompt":"add a test","harness":"claude_code","repo":"lexoliu/flyco",
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
            r#"{"prompt":"add a test","harness":"claude_code","repo":"lexoliu/flyco",
                "budget_limit":10000000}"#,
        )
        .expect("deserialize");
        assert!(request.machine.is_none());
        assert!(request.spot);
    }

    #[test]
    fn a_session_cannot_be_opened_without_a_prompt() {
        assert!(
            serde_json::from_str::<CreateSession>(
                r#"{"harness":"claude_code","repo":"lexoliu/flyco","budget_limit":10000000}"#,
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
    fn cannot_skip_provisioning_when_resuming_interrupted() {
        assert!(
            SessionState::Interrupted
                .transition(SessionState::Active)
                .is_err()
        );
    }
}
