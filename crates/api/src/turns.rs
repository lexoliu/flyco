//! A session's turn history, folded out of the room's recorded stream.
//!
//! A turn is not a row anywhere. It is a shape in the event stream the
//! session room records — a `user_message`, then `turn_started`, then
//! whatever the agent did, then `turn_completed` or `turn_failed` — and this
//! module is the fold that recovers it.
//!
//! # Why the room's stream and not the R2 transcript
//!
//! The frozen contract says the history is read from what survives a
//! machine, and what survives a machine is the control plane's copy. Two of
//! those exist. R2 holds the *harness's own* store: for Claude Code it is
//! the Agent SDK's `SessionStore` entries, opaque values whose shape is
//! Anthropic's to change, keyed by a stream name the Worker never learns;
//! Codex has no such store at all. Folding turns out of it would put
//! harness-specific interpretation in the control plane, which is exactly
//! what `flyco_core::HarnessEvent` exists to prevent — every interpretation
//! happens in flycod — and would answer nothing for half the harnesses.
//!
//! The room's `events` table holds the normalized stream instead: the same
//! events, harness-neutral, each stamped with when the control plane
//! recorded it, and it is what a browser replays. Reading the history from
//! anywhere else would let the list disagree with the conversation beside
//! it — which is the reason the contract gave for not using a table.
//!
//! # Pagination
//!
//! The cursor is a position in that stream, and a page ends on a turn
//! boundary rather than in the middle of one. A turn still running is
//! returned with no completion, and the cursor stays *before* it, so the
//! next read describes it whole rather than losing its start. `next_cursor`
//! is `None` once the read reaches the end of what is recorded: a client
//! following a live session watches the relay for what happens next and
//! re-lists when it wants the history again.

use flyco_core::{ClientEvent, HarnessEvent, SessionId, TurnPage, TurnSummary};

use crate::error::ApiError;
use crate::room::StoredEvent;
use crate::rooms::Rooms;

/// Most turns one page returns.
pub const MAX_TURNS_PER_PAGE: u32 = 50;

/// How much of the prompt a turn is named by.
///
/// Enough to recognise the turn in a list, short enough that a history of
/// fifty turns is not a transcript in its own right.
pub const PROMPT_EXCERPT_CHARS: usize = 200;

/// How many event pages one request may read.
///
/// A turn is many events, and a session that produced tens of thousands of
/// them between two turn boundaries would otherwise hold the request open
/// for as long as it took to walk all of them. Stopping early is not a lost
/// page: the cursor comes back and the client asks again.
const MAX_EVENT_PAGES: usize = 8;

/// Reads one page of a session's turns.
///
/// # Errors
///
/// Returns [`ApiError::InvalidCursor`] if the cursor is not one this API
/// issued, or [`ApiError`] if the room could not be reached or recorded
/// something that is not a client event.
pub async fn page(
    rooms: &Rooms,
    session: SessionId,
    cursor: Option<&str>,
    limit: Option<u32>,
) -> Result<TurnPage, ApiError> {
    let after = match cursor {
        Some(cursor) => cursor
            .parse::<u64>()
            .map_err(|_| ApiError::InvalidCursor(cursor.to_owned()))?,
        None => 0,
    };
    let limit = limit.unwrap_or(MAX_TURNS_PER_PAGE).min(MAX_TURNS_PER_PAGE) as usize;

    let mut fold = Fold::new(after, limit);
    for _ in 0..MAX_EVENT_PAGES {
        let events = rooms.events(session, fold.resume_from()).await?;
        fold.absorb(&events.events)?;
        if fold.is_full() {
            return Ok(fold.into_page(true));
        }
        if !events.more {
            return Ok(fold.into_page(false));
        }
    }
    Ok(fold.into_page(true))
}

/// The turns recovered from a stretch of the room's stream.
struct Fold {
    /// Turns in the order they started, the last of which may still be open.
    turns: Vec<TurnSummary>,
    /// Position of the turn that is still running, if one is.
    open: Option<usize>,
    /// The last thing the user said, waiting for the turn it began.
    prompt: Option<String>,
    /// Stream position the last completed turn ended at.
    closed_at: Option<u64>,
    /// Stream position of the last event looked at.
    seen_to: u64,
    /// Where this page started.
    from: u64,
    /// How many completed turns fill a page.
    limit: usize,
    /// How many turns have completed in this page.
    closed: usize,
}

impl Fold {
    const fn new(from: u64, limit: usize) -> Self {
        Self {
            turns: Vec::new(),
            open: None,
            prompt: None,
            closed_at: None,
            seen_to: from,
            from,
            limit,
            closed: 0,
        }
    }

    /// Whether this page has all the completed turns it was asked for.
    const fn is_full(&self) -> bool {
        self.closed >= self.limit
    }

    /// Where the next read of the room's stream begins.
    const fn resume_from(&self) -> u64 {
        self.seen_to
    }

    /// Folds one page of recorded events in.
    fn absorb(&mut self, events: &[StoredEvent]) -> Result<(), ApiError> {
        for stored in events {
            if self.is_full() {
                return Ok(());
            }
            self.seen_to = stored.seq;

            let event: ClientEvent = serde_json::from_value(stored.event.clone())
                .map_err(|_| ApiError::CorruptRecord("a room recorded a non-client event"))?;
            match event {
                ClientEvent::UserMessage { text } => self.prompt = Some(excerpt(&text)),
                ClientEvent::Harness { event } => self.harness(&event, stored),
                _ => {}
            }
        }
        Ok(())
    }

    /// Applies one harness event to the turn it belongs to.
    ///
    /// A completion whose turn was never opened is ignored rather than
    /// invented: a page that begins in the middle of a turn cannot describe
    /// that turn, and the previous page already returned it.
    fn harness(&mut self, event: &HarnessEvent, stored: &StoredEvent) {
        match event {
            HarnessEvent::TurnStarted { turn_id } => {
                self.open = Some(self.turns.len());
                self.turns.push(TurnSummary {
                    turn_id: turn_id.clone(),
                    started_at_unix: stored.at_unix,
                    completed_at_unix: None,
                    prompt_excerpt: self.prompt.take().unwrap_or_default(),
                    usage: None,
                });
            }
            HarnessEvent::TurnCompleted { turn_id, usage } => {
                self.close(turn_id, stored.seq, stored.at_unix, Some(*usage));
            }
            HarnessEvent::TurnFailed { turn_id, .. } => {
                self.close(turn_id, stored.seq, stored.at_unix, None);
            }
            _ => {}
        }
    }

    fn close(
        &mut self,
        turn_id: &str,
        seq: u64,
        at_unix: u64,
        usage: Option<flyco_core::UsageReport>,
    ) {
        let Some(index) = self.open else { return };
        let turn = &mut self.turns[index];
        if turn.turn_id != turn_id {
            return;
        }
        turn.completed_at_unix = Some(at_unix);
        turn.usage = usage;
        self.open = None;
        self.closed_at = Some(seq);
        self.closed += 1;
    }

    /// The page, and where a client resumes if there is more to read.
    ///
    /// When a turn is still running the cursor stays at the end of the last
    /// completed one, so the next read sees that turn's start again rather
    /// than a completion with nothing to attach it to.
    fn into_page(self, more: bool) -> TurnPage {
        let resume = if self.open.is_some() {
            self.closed_at.unwrap_or(self.from)
        } else {
            self.seen_to
        };
        TurnPage {
            turns: self.turns,
            next_cursor: more.then(|| resume.to_string()),
        }
    }
}

/// The opening of a prompt, as the history list names a turn by it.
fn excerpt(text: &str) -> String {
    let trimmed = text.trim();
    match trimmed.char_indices().nth(PROMPT_EXCERPT_CHARS) {
        Some((end, _)) => format!("{}…", trimmed[..end].trim_end()),
        None => trimmed.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use flyco_core::{ClientEvent, ContextWindow, HarnessEvent, TurnPage, UsageReport, Usd};

    use super::{Fold, PROMPT_EXCERPT_CHARS, excerpt};
    use crate::room::StoredEvent;

    fn usage() -> UsageReport {
        UsageReport {
            input_tokens: 12,
            output_tokens: 34,
            context: Some(ContextWindow {
                used_tokens: 1_000,
                size_tokens: 200_000,
            }),
            estimated_cost: Some(Usd::from_cents(7)),
        }
    }

    /// Builds a stream, numbering the events the way the room's
    /// `AUTOINCREMENT` sequence does.
    fn stream(events: Vec<ClientEvent>) -> Vec<StoredEvent> {
        events
            .into_iter()
            .enumerate()
            .map(|(index, event)| StoredEvent {
                seq: index as u64 + 1,
                event: serde_json::to_value(event).expect("serialize"),
                at_unix: 1_800_000_000 + index as u64,
            })
            .collect()
    }

    fn said(text: &str) -> ClientEvent {
        ClientEvent::UserMessage {
            text: text.to_owned(),
        }
    }

    fn harness(event: HarnessEvent) -> ClientEvent {
        ClientEvent::Harness { event }
    }

    fn started(turn: &str) -> ClientEvent {
        harness(HarnessEvent::TurnStarted {
            turn_id: turn.to_owned(),
        })
    }

    fn completed(turn: &str) -> ClientEvent {
        harness(HarnessEvent::TurnCompleted {
            turn_id: turn.to_owned(),
            usage: usage(),
        })
    }

    /// Folds a whole stream, as [`page`](super::page) does when the room
    /// reports nothing more to read.
    fn fold(events: Vec<ClientEvent>) -> TurnPage {
        let mut fold = Fold::new(0, 10);
        fold.absorb(&stream(events)).expect("fold");
        fold.into_page(false)
    }

    #[test]
    fn a_turn_is_named_by_the_message_that_began_it() {
        let page = fold(vec![
            said("what does this crate do?"),
            started("t-1"),
            harness(HarnessEvent::AssistantDelta {
                turn_id: "t-1".to_owned(),
                text: "it…".to_owned(),
            }),
            completed("t-1"),
        ]);

        assert_eq!(page.turns.len(), 1);
        let turn = &page.turns[0];
        assert_eq!(turn.turn_id, "t-1");
        assert_eq!(turn.prompt_excerpt, "what does this crate do?");
        assert_eq!(turn.started_at_unix, 1_800_000_001);
        assert_eq!(turn.completed_at_unix, Some(1_800_000_003));
        assert_eq!(turn.usage, Some(usage()));
        assert!(page.next_cursor.is_none());
    }

    #[test]
    fn a_failed_turn_completes_without_usage() {
        let page = fold(vec![
            said("break something"),
            started("t-1"),
            harness(HarnessEvent::TurnFailed {
                turn_id: "t-1".to_owned(),
                error: "the model refused".to_owned(),
            }),
        ]);

        assert_eq!(page.turns.len(), 1);
        assert!(page.turns[0].completed_at_unix.is_some());
        assert!(page.turns[0].usage.is_none());
    }

    #[test]
    fn a_running_turn_is_returned_open_and_the_cursor_stays_before_it() {
        let mut fold = Fold::new(0, 10);
        fold.absorb(&stream(vec![
            said("first"),
            started("t-1"),
            completed("t-1"),
            said("second"),
            started("t-2"),
        ]))
        .expect("fold");
        let page = fold.into_page(true);

        assert_eq!(page.turns.len(), 2);
        assert!(page.turns[1].completed_at_unix.is_none());
        assert_eq!(
            page.next_cursor.as_deref(),
            Some("3"),
            "the cursor is the end of the last completed turn, so the open one is read again"
        );
    }

    #[test]
    fn a_page_stops_on_a_turn_boundary() {
        let mut fold = Fold::new(0, 1);
        fold.absorb(&stream(vec![
            said("first"),
            started("t-1"),
            completed("t-1"),
            said("second"),
            started("t-2"),
            completed("t-2"),
        ]))
        .expect("fold");

        assert!(fold.is_full());
        let page = fold.into_page(true);
        assert_eq!(
            page.turns
                .iter()
                .map(|turn| &turn.turn_id)
                .collect::<Vec<_>>(),
            vec!["t-1"]
        );
        assert_eq!(page.next_cursor.as_deref(), Some("3"));
    }

    #[test]
    fn a_completion_whose_start_is_on_an_earlier_page_is_not_invented() {
        let page = fold(vec![completed("t-1"), said("next"), started("t-2")]);
        assert_eq!(
            page.turns
                .iter()
                .map(|turn| &turn.turn_id)
                .collect::<Vec<_>>(),
            vec!["t-2"],
            "a completion with no start on this page describes nothing"
        );
    }

    #[test]
    fn a_turn_nobody_prompted_has_no_excerpt() {
        // A budget notice reaches the harness as a message the *control
        // plane* sent, so a turn can begin with nothing the user said.
        let page = fold(vec![started("t-1"), completed("t-1")]);
        assert_eq!(page.turns[0].prompt_excerpt, "");
    }

    #[test]
    fn a_prompt_is_shortened_rather_than_returned_whole() {
        let long = "x".repeat(PROMPT_EXCERPT_CHARS * 2);
        let short = excerpt(&long);
        assert_eq!(short.chars().count(), PROMPT_EXCERPT_CHARS + 1);
        assert!(short.ends_with('…'));

        assert_eq!(excerpt("  spaced  "), "spaced");
        assert_eq!(excerpt("短い"), "短い");
    }
}
