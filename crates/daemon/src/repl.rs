//! A line-oriented REPL for driving a session from a terminal.
//!
//! # What this is, and what it is not
//!
//! In the shipped execution plane, flycod's session loop is driven by the
//! control plane: an SSE command stream from the session's Durable Object
//! carrying [`flyco_core::wire::ControlToDaemon`] in, and `POST`ed batches
//! carrying [`flyco_core::wire::DaemonToControl`] out.
//!
//! This REPL is the M3a stand-in for it: the same
//! [`crate::harness::HarnessSession`] surface, driven from stdin instead of
//! from a control plane, emitting the same [`SessionOutput`] values as JSON lines
//! on stdout instead of as wire frames. It is how a Claude Code session is
//! verified end to end before there is a control plane to verify it
//! against, and it stays afterwards as the dev tool for reproducing a
//! harness bug without provisioning a VM.
//!
//! Because stdout carries structured output, all logging goes to stderr.
//!
//! # Line protocol
//!
//! | Input | Effect |
//! |---|---|
//! | any other text | a user message, opening a turn |
//! | `/interrupt` | end the turn with SIGINT semantics |
//! | `/approve [id]` | allow a pending tool call — the oldest, or `id` |
//! | `/deny [id] [reason]` | refuse one, with a reason for the model |
//! | `/quit` | shut the harness down and exit |

use std::collections::VecDeque;

use flyco_core::ApprovalId;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::sync::mpsc;

use crate::harness::{HarnessSession, SessionOutput, ToolApproval};

/// What the REPL tells the model when the operator gives no reason.
const DEFAULT_DENIAL: &str = "Denied by the flyco operator.";

/// The REPL could not read stdin or write stdout.
#[derive(Debug, thiserror::Error)]
#[error("the flycod REPL lost {stream}")]
pub struct ReplError {
    /// Which stream broke.
    stream: &'static str,
    /// The underlying cause.
    #[source]
    source: std::io::Error,
}

/// One line of the REPL's structured output.
#[derive(Debug, Serialize)]
#[serde(tag = "line", rename_all = "snake_case")]
enum ReplLine<'a> {
    /// Something the session emitted.
    Session {
        /// The session output.
        output: &'a SessionOutput,
    },
    /// The REPL could not act on what was typed.
    Error {
        /// What was wrong.
        message: String,
    },
}

/// One parsed input line.
#[derive(Debug, PartialEq)]
enum Input {
    /// Send this text to the model.
    Message(String),
    /// End the current turn.
    Interrupt,
    /// Allow a pending tool call.
    Approve(Option<ApprovalId>),
    /// Refuse a pending tool call.
    Deny(Option<ApprovalId>, String),
    /// Shut the session down.
    Quit,
    /// Not a command this REPL knows.
    Unknown(String),
}

impl Input {
    /// Parses one line. Anything not starting with `/` is a message,
    /// including the empty line, which is dropped by the caller.
    fn parse(line: &str) -> Self {
        let Some(rest) = line.strip_prefix('/') else {
            return Self::Message(line.to_owned());
        };
        let mut words = rest.split_whitespace();
        let Some(verb) = words.next() else {
            return Self::Unknown(line.to_owned());
        };
        let id = words.next().map(str::parse::<ApprovalId>);
        match (verb, id) {
            ("interrupt", None) => Self::Interrupt,
            ("quit", None) => Self::Quit,
            ("approve", None) => Self::Approve(None),
            ("approve", Some(Ok(id))) => Self::Approve(Some(id)),
            ("deny", None) => Self::Deny(None, DEFAULT_DENIAL.to_owned()),
            ("deny", Some(Ok(id))) => {
                let reason: Vec<&str> = words.collect();
                let message = if reason.is_empty() {
                    DEFAULT_DENIAL.to_owned()
                } else {
                    reason.join(" ")
                };
                Self::Deny(Some(id), message)
            }
            _ => Self::Unknown(line.to_owned()),
        }
    }
}

/// Drives `session` from stdin until end-of-input or `/quit`, printing
/// everything it emits to stdout as JSON lines.
///
/// # Errors
///
/// Returns [`ReplError`] if stdin or stdout breaks. A harness failure is
/// reported on stdout as a [`SessionOutput::Fatal`] line and ends the loop
/// rather than erroring here: the transcript of what happened is the
/// output, not the exit code.
pub async fn run<S: HarnessSession>(
    session: S,
    mut outputs: mpsc::Receiver<SessionOutput>,
) -> Result<(), ReplError> {
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    let mut pending: VecDeque<ApprovalId> = VecDeque::new();

    loop {
        tokio::select! {
            line = input.next_line() => {
                let line = line.map_err(|source| ReplError { stream: "stdin", source })?;
                let Some(line) = line else {
                    tracing::info!("stdin closed; shutting the session down");
                    break;
                };
                if line.trim().is_empty() {
                    continue;
                }
                if !act(&session, &mut pending, &mut stdout, Input::parse(&line)).await? {
                    break;
                }
            }
            output = outputs.recv() => {
                let Some(output) = output else {
                    tracing::info!("the session's output stream ended");
                    break;
                };
                if let SessionOutput::ApprovalRequest { id, .. } = &output {
                    pending.push_back(*id);
                }
                let terminal = matches!(output, SessionOutput::Fatal { .. });
                write(&mut stdout, &ReplLine::Session { output: &output }).await?;
                if terminal {
                    break;
                }
            }
        }
    }

    // Keep printing while the harness stops. The task driving it may still
    // be emitting, and a full output channel with nobody draining it would
    // deadlock against the acknowledgement this is waiting for.
    let mut stopping = std::pin::pin!(session.shutdown());
    let mut live = true;
    loop {
        tokio::select! {
            outcome = &mut stopping => {
                if let Err(error) = outcome {
                    tracing::warn!(%error, "the harness did not shut down cleanly");
                }
                break;
            }
            output = outputs.recv(), if live => {
                match output {
                    Some(output) => {
                        write(&mut stdout, &ReplLine::Session { output: &output }).await?;
                    }
                    None => live = false,
                }
            }
        }
    }

    // Whatever the harness produced on its way out.
    while let Some(output) = outputs.recv().await {
        write(&mut stdout, &ReplLine::Session { output: &output }).await?;
    }
    Ok(())
}

/// Acts on one parsed line; returns whether the REPL should keep running.
async fn act<S: HarnessSession>(
    session: &S,
    pending: &mut VecDeque<ApprovalId>,
    stdout: &mut tokio::io::Stdout,
    input: Input,
) -> Result<bool, ReplError> {
    let outcome = match input {
        Input::Quit => return Ok(false),
        Input::Message(text) => session.send_user_message(text).await,
        Input::Interrupt => session.interrupt().await,
        Input::Approve(id) => match resolve(pending, id) {
            Some(id) => {
                session
                    .decide_approval(ToolApproval::Allow {
                        id,
                        updated_input: None,
                    })
                    .await
            }
            None => return no_such_approval(stdout).await,
        },
        Input::Deny(id, message) => match resolve(pending, id) {
            Some(id) => {
                session
                    .decide_approval(ToolApproval::Deny { id, message })
                    .await
            }
            None => return no_such_approval(stdout).await,
        },
        Input::Unknown(line) => {
            write(
                stdout,
                &ReplLine::Error {
                    message: format!(
                        "unknown command {line:?}; try /interrupt, /approve [id], \
                         /deny [id] [reason], or /quit"
                    ),
                },
            )
            .await?;
            return Ok(true);
        }
    };

    if let Err(error) = outcome {
        write(
            stdout,
            &ReplLine::Error {
                message: error.to_string(),
            },
        )
        .await?;
        return Ok(false);
    }
    Ok(true)
}

/// Picks the approval a command refers to, removing it from the queue.
fn resolve(pending: &mut VecDeque<ApprovalId>, id: Option<ApprovalId>) -> Option<ApprovalId> {
    match id {
        None => pending.pop_front(),
        Some(id) => {
            let at = pending.iter().position(|waiting| *waiting == id)?;
            pending.remove(at)
        }
    }
}

async fn no_such_approval(stdout: &mut tokio::io::Stdout) -> Result<bool, ReplError> {
    write(
        stdout,
        &ReplLine::Error {
            message: "no such pending approval".to_owned(),
        },
    )
    .await?;
    Ok(true)
}

async fn write(stdout: &mut tokio::io::Stdout, line: &ReplLine<'_>) -> Result<(), ReplError> {
    let mut rendered = serde_json::to_string(line).expect("every ReplLine serializes to JSON");
    rendered.push('\n');
    stdout
        .write_all(rendered.as_bytes())
        .await
        .map_err(|source| ReplError {
            stream: "stdout",
            source,
        })?;
    stdout.flush().await.map_err(|source| ReplError {
        stream: "stdout",
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_DENIAL, Input, ReplLine, resolve};
    use crate::harness::SessionOutput;
    use flyco_core::{ApprovalId, HarnessEvent};
    use std::collections::VecDeque;

    #[test]
    fn plain_text_is_a_user_message() {
        assert_eq!(
            Input::parse("what does this crate do?"),
            Input::Message("what does this crate do?".to_owned())
        );
    }

    #[test]
    fn a_message_that_merely_mentions_a_slash_is_still_a_message() {
        assert_eq!(
            Input::parse("run cargo/clippy"),
            Input::Message("run cargo/clippy".to_owned())
        );
    }

    #[test]
    fn commands_parse() {
        assert_eq!(Input::parse("/interrupt"), Input::Interrupt);
        assert_eq!(Input::parse("/quit"), Input::Quit);
        assert_eq!(Input::parse("/approve"), Input::Approve(None));
        assert_eq!(
            Input::parse("/deny"),
            Input::Deny(None, DEFAULT_DENIAL.to_owned())
        );
    }

    #[test]
    fn an_approval_id_selects_one_pending_request() {
        let id = ApprovalId::generate();
        assert_eq!(
            Input::parse(&format!("/approve {id}")),
            Input::Approve(Some(id))
        );
        assert_eq!(
            Input::parse(&format!("/deny {id} touches production")),
            Input::Deny(Some(id), "touches production".to_owned())
        );
    }

    #[test]
    fn a_malformed_command_is_reported_rather_than_guessed_at() {
        assert_eq!(
            Input::parse("/approve not-a-uuid"),
            Input::Unknown("/approve not-a-uuid".to_owned())
        );
        assert_eq!(
            Input::parse("/resume"),
            Input::Unknown("/resume".to_owned())
        );
    }

    #[test]
    fn resolving_takes_the_oldest_or_the_named_approval() {
        let (first, second) = (ApprovalId::generate(), ApprovalId::generate());
        let mut pending: VecDeque<ApprovalId> = [first, second].into_iter().collect();

        assert_eq!(resolve(&mut pending, Some(second)), Some(second));
        assert_eq!(resolve(&mut pending, None), Some(first));
        assert_eq!(resolve(&mut pending, None), None);
        assert_eq!(resolve(&mut pending, Some(first)), None);
    }

    #[test]
    fn output_lines_are_tagged_so_a_reader_can_route_them() {
        let output = SessionOutput::Event {
            event: HarnessEvent::AssistantDelta {
                turn_id: "turn-1".to_owned(),
                text: "hi".to_owned(),
            },
        };
        let json = serde_json::to_value(ReplLine::Session { output: &output }).expect("serialize");
        assert_eq!(json["line"], "session");
        assert_eq!(json["output"]["output"], "event");
        assert_eq!(json["output"]["event"]["type"], "assistant_delta");

        let json = serde_json::to_value(ReplLine::Error {
            message: "nope".to_owned(),
        })
        .expect("serialize");
        assert_eq!(json["line"], "error");
    }
}
