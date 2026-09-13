//! `flyco run` — one prompt, one session, one turn.
//!
//! The orchestrated one-shot the agent contract is built around:
//! `session create`, then `wait --for idle,approval,paused,failed,stopped`,
//! then the teardown the flags asked for. Stdout is a JSONL stream of the
//! session's own events — `StoredEvent` rows on catch-up, `SessionEvent`
//! envelopes live — closed by the one line this command synthesizes,
//! `run_result`, which carries the outcome and the session id.

use flyco_core::SessionDetail;
use serde::Serialize;

use crate::cli::SessionSpec;
use crate::client::Api;
use crate::follow::{Follow, Item};
use crate::session::Condition;
use crate::{Exit, Failure, Outcome, out};

/// `flyco run`.
///
/// Output is always JSONL — the command exists for agents, and a human
/// asking for a table gets one anyway because there is no human rendering
/// of an event stream.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when creation, the event stream, or
/// teardown fails, or the timeout elapses.
pub async fn run(
    api: &Api,
    harness: flyco_core::HarnessKind,
    spec: &SessionSpec,
    detach: bool,
    stop: bool,
    archive: bool,
    timeout: Option<core::time::Duration>,
) -> Outcome<Exit> {
    // The conditions a run ends on: the turn finished, the agent needs a
    // decision, or the session left the running states for good.
    const ENDINGS: [Condition; 5] = [
        Condition::Idle,
        Condition::Approval,
        Condition::Paused,
        Condition::Failed,
        Condition::Stopped,
    ];

    let session = crate::session::create(api, harness, spec, out::Mode::Json).await?;
    if detach {
        return Ok(Exit::Ok);
    }
    let id = session.summary.id;

    let ending = async {
        // Catch up from the beginning: a fast machine can finish its turn
        // before the first SSE byte lands, and `idle` on the row is the
        // same `idle` a `TurnCompleted` event would have said.
        let detail: SessionDetail = api.get(&format!("/v1/sessions/{id}")).await?;
        if let Some(condition) = ENDINGS.iter().find(|c| c.met_by(&detail)) {
            return Ok(*condition);
        }
        let mut follow = Follow::new(api, id);
        loop {
            let item = follow.next().await?;
            match &item {
                Item::Live(envelope) => out::emit_line(envelope)?,
                Item::Replayed(stored) => out::emit_line(stored)?,
            }
            let Some(event) = item.event() else { continue };
            if let Some(condition) = ENDINGS.iter().find(|c| c.met_by_event(&event)) {
                return Ok(*condition);
            }
        }
    };
    let ending = match timeout {
        Some(limit) => match tokio::time::timeout(limit, ending).await {
            Ok(ending) => ending?,
            Err(_elapsed) => {
                return Err(Failure::problem(
                    Exit::Timeout,
                    format!("the turn did not end within {}s", limit.as_secs()),
                ));
            }
        },
        None => ending.await?,
    };

    // Teardown the flags asked for. Stop before archive: an archived
    // session releases the environment anyway, but `--stop` alone must
    // leave the session resumable.
    if archive {
        api.post::<serde_json::Value, serde_json::Value>(
            &format!("/v1/sessions/{id}/archive"),
            &serde_json::json!({}),
        )
        .await?;
    } else if stop {
        api.post_empty(&format!("/v1/sessions/{id}/machine/stop"))
            .await?;
    }

    out::emit_line(&RunResult {
        kind: "run_result",
        outcome: ending.name(),
        session_id: id,
    })?;
    Ok(ending.exit())
}

/// The closing line of a `run`'s JSONL stream — the one synthesized
/// document, named so a consumer can tell it from the API's own events.
#[derive(Debug, Serialize)]
struct RunResult {
    /// Always `"run_result"` — the discriminator a parser switches on.
    #[serde(rename = "type")]
    kind: &'static str,
    /// Which `--for` condition ended the run.
    outcome: &'static str,
    /// The session that ran.
    session_id: flyco_core::SessionId,
}
