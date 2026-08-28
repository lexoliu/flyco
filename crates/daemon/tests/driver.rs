//! The Claude Code driver, end to end against a stand-in sidecar.
//!
//! `fake-sidecar.sh` speaks the flycod line protocol and nothing else, so
//! this exercises the parts of the driver a live session would exercise —
//! handshake, `start`, turn bookkeeping, normalization, approval routing,
//! and the `SessionStore` round trip — without Bun, the Agent SDK, or a
//! `claude` process. What it deliberately does not cover is whether the SDK
//! behaves as documented; that is what live verification is for.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use flyco_core::HarnessEvent;
use flyco_daemon::config::{ClaudeAuth, ClaudeConfig};
use flyco_daemon::harness::claude::protocol::PermissionMode;
use flyco_daemon::harness::claude::sidecar::SidecarConfig;
use flyco_daemon::harness::claude::store::JsonlTranscriptStore;
use flyco_daemon::harness::claude::{ClaudeCodeHarness, ClaudeSession};
use flyco_daemon::harness::{Harness as _, HarnessSession as _, SessionOutput, StartRequest};
use tokio::sync::mpsc;

/// A scratch directory that removes itself.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "flycod-driver-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the scratch directory");
        Self(path)
    }

    fn join(&self, tail: &str) -> PathBuf {
        self.0.join(tail)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A stand-in script from `tests/`, made executable wherever the crate was
/// checked out.
fn script(name: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(name);
    let mut permissions = std::fs::metadata(&path)
        .unwrap_or_else(|error| panic!("{} must exist: {error}", path.display()))
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).expect("make the script executable");
    path
}

/// Starts a session whose "bun" is the stand-in sidecar.
async fn start(scratch: &Scratch) -> (ClaudeSession, mpsc::Receiver<SessionOutput>) {
    let sidecar_dir = scratch.join("sidecar");
    // `prepare` skips `bun install` when the tree is already there, which
    // is what keeps this test free of a network and a Bun toolchain.
    std::fs::create_dir_all(sidecar_dir.join("node_modules")).expect("fake an install");

    let harness = ClaudeCodeHarness::new(
        ClaudeConfig {
            model: None,
            permission_mode: PermissionMode::Default,
            auth: ClaudeAuth::Inherit,
        },
        SidecarConfig {
            dir: sidecar_dir,
            bun: script("fake-sidecar.sh"),
        },
        JsonlTranscriptStore::new(scratch.join("transcripts")),
    );

    let started = harness
        .start(StartRequest {
            workdir: scratch.join("work"),
            resume_session_id: None,
        })
        .await
        .expect("the driver must start against the stand-in sidecar");
    (started.session, started.outputs)
}

/// The next output, or a panic naming what was being waited for.
async fn next(outputs: &mut mpsc::Receiver<SessionOutput>, what: &str) -> SessionOutput {
    tokio::time::timeout(std::time::Duration::from_secs(10), outputs.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
        .unwrap_or_else(|| panic!("the session ended before {what}"))
}

#[tokio::test]
async fn a_turn_runs_from_user_message_to_completion() {
    let scratch = Scratch::new("turn");
    let (session, mut outputs) = start(&scratch).await;

    // The sidecar's `started` becomes the session identity flyco resumes by.
    let SessionOutput::Started {
        session_id,
        capabilities,
    } = next(&mut outputs, "started").await
    else {
        panic!("the first output must be `started`");
    };
    assert_eq!(session_id, "fake-session");
    assert_eq!(capabilities, vec!["interrupt_receipt_v1".to_owned()]);

    session
        .send_user_message("hi".to_owned())
        .await
        .expect("send a user message");

    // A turn id is flyco's own: minted here, stamped on everything until
    // the harness's terminal `result`.
    let SessionOutput::Event {
        event: HarnessEvent::TurnStarted { turn_id },
    } = next(&mut outputs, "turn_started").await
    else {
        panic!("a user message must open a turn");
    };

    assert_eq!(
        next(&mut outputs, "assistant_delta").await,
        SessionOutput::Event {
            event: HarnessEvent::AssistantDelta {
                turn_id: turn_id.clone(),
                text: "hello".to_owned(),
            },
        }
    );

    let SessionOutput::ApprovalRequest { id, tool, .. } =
        next(&mut outputs, "approval_request").await
    else {
        panic!("the tool call must reach flyco's approval UI");
    };
    assert_eq!(tool, "Bash");

    session
        .decide_approval(flyco_daemon::harness::ToolApproval::Allow {
            id,
            updated_input: None,
        })
        .await
        .expect("decide the approval");

    let SessionOutput::Event {
        event: HarnessEvent::TurnCompleted {
            turn_id: done,
            usage,
        },
    } = next(&mut outputs, "turn_completed").await
    else {
        panic!("the result must close the turn");
    };
    assert_eq!(done, turn_id);
    assert_eq!(usage.input_tokens, 7);
    assert_eq!(usage.output_tokens, 11);
    // 0.0025 USD is 2500 microdollars exactly.
    assert_eq!(usage.estimated_cost.expect("a cost").micros(), 2_500);
    // The Agent SDK named no window size, so there is no gauge to show.
    assert!(usage.context.is_none());

    session.shutdown().await.expect("shut the session down");
}

#[tokio::test]
async fn the_transcript_store_answers_the_sdk_and_keeps_what_it_is_given() {
    let scratch = Scratch::new("store");
    let (session, mut outputs) = start(&scratch).await;
    let _ = next(&mut outputs, "started").await;

    // The stand-in issues a load and then an append; both are answered by
    // the driver before it will accept anything else, so a completed turn
    // proves both round trips closed.
    session
        .send_user_message("hi".to_owned())
        .await
        .expect("send a user message");
    let _ = next(&mut outputs, "turn_started").await;
    let _ = next(&mut outputs, "assistant_delta").await;
    let SessionOutput::ApprovalRequest { id, .. } = next(&mut outputs, "approval_request").await
    else {
        panic!("expected an approval request");
    };
    session
        .decide_approval(flyco_daemon::harness::ToolApproval::Deny {
            id,
            message: "not in a test".to_owned(),
        })
        .await
        .expect("decide the approval");
    let _ = next(&mut outputs, "turn_completed").await;

    let stored = scratch.join("transcripts/fake-project/fake-session/entries.jsonl");
    let text = std::fs::read_to_string(&stored)
        .unwrap_or_else(|error| panic!("read {}: {error}", stored.display()));
    let line: serde_json::Value =
        serde_json::from_str(text.trim()).expect("the stored line is one JSON entry");
    assert_eq!(line["uuid"], "11111111-1111-4111-8111-111111111111");
    assert_eq!(line["text"], "mirrored");

    session.shutdown().await.expect("shut the session down");
}

#[tokio::test]
async fn an_interrupted_turn_fails_rather_than_completing() {
    let scratch = Scratch::new("interrupt");
    let (session, mut outputs) = start(&scratch).await;
    let _ = next(&mut outputs, "started").await;

    session
        .send_user_message("hi".to_owned())
        .await
        .expect("send a user message");
    let SessionOutput::Event {
        event: HarnessEvent::TurnStarted { turn_id },
    } = next(&mut outputs, "turn_started").await
    else {
        panic!("a user message must open a turn");
    };
    let _ = next(&mut outputs, "assistant_delta").await;
    let _ = next(&mut outputs, "approval_request").await;

    session.interrupt().await.expect("interrupt the turn");

    assert_eq!(
        next(&mut outputs, "turn_failed").await,
        SessionOutput::Event {
            event: HarnessEvent::TurnFailed {
                turn_id,
                error: "interrupted".to_owned(),
            },
        }
    );

    session.shutdown().await.expect("shut the session down");
}

#[tokio::test]
async fn a_missing_bun_names_itself_instead_of_failing_obscurely() {
    let scratch = Scratch::new("no-bun");
    let harness = ClaudeCodeHarness::new(
        ClaudeConfig {
            model: None,
            permission_mode: PermissionMode::Default,
            auth: ClaudeAuth::Inherit,
        },
        SidecarConfig {
            dir: scratch.join("sidecar"),
            bun: PathBuf::from("definitely-not-a-real-bun"),
        },
        JsonlTranscriptStore::new(scratch.join("transcripts")),
    );

    let error = harness
        .start(StartRequest {
            workdir: scratch.join("work"),
            resume_session_id: None,
        })
        .await
        .expect_err("a missing bun must fail the daemon at startup");
    assert!(
        error.to_string().contains("bun was not found"),
        "the error must name bun: {error}"
    );
}

#[test]
fn nothing_bun_prints_reaches_the_structured_output_stream() {
    // flycod's stdout is JSON lines and nothing else, so `bun install`'s
    // progress has to be captured rather than inherited. A loud, failing
    // stand-in proves it: the failure must be reported, and stdout must
    // still be empty.
    let scratch = Scratch::new("noisy-bun");
    let config_path = scratch.join("flycod.toml");
    let config = flyco_daemon::config::EXAMPLE
        .replace(
            "/tmp/flycod-dev",
            scratch.0.to_str().expect("a UTF-8 scratch path"),
        )
        .replace(
            "bun = \"bun\"",
            &format!(
                "bun = {:?}",
                script("noisy-bun.sh").to_str().expect("a UTF-8 path")
            ),
        );
    std::fs::write(&config_path, config).expect("write the config");

    let run = std::process::Command::new(env!("CARGO_BIN_EXE_flycod"))
        .arg("run")
        .arg("--config")
        .arg(&config_path)
        .output()
        .expect("run flycod");

    assert!(
        !run.status.success(),
        "a failed install must fail the daemon"
    );
    assert_eq!(
        String::from_utf8_lossy(&run.stdout),
        "",
        "bun's chatter must not reach the structured output stream"
    );
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("frozen-lockfile"),
        "the failure must name the install: {stderr}"
    );
}
