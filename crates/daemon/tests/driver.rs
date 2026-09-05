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
use flyco_daemon::mount::{FlycoServer, Mount};
use tokio::sync::mpsc;

/// The MCP servers a test session is given.
///
/// Only flyco's own, and it is never launched: the stand-in sidecar reports
/// the mount rather than performing it. What the driver does with the
/// report is what these tests are about.
fn mount() -> Mount {
    Mount::new(
        FlycoServer::of(Path::new("/etc/flyco/flycod.toml")).expect("this test binary has a path"),
        Vec::new(),
    )
}

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
            managed_dir: None,
            auth: ClaudeAuth::Inherit,
        },
        SidecarConfig {
            dir: sidecar_dir,
            bun: script("fake-sidecar.sh"),
        },
        mount(),
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
async fn a_harness_that_came_up_without_flycos_tools_fails_the_session() {
    // The scratch's name is what tells the stand-in sidecar to report a
    // flyco server with `machine_status` missing.
    let scratch = Scratch::new("unmounted");
    let (_session, mut outputs) = start(&scratch).await;

    // Identity still arrives first: the CLI is warm before its MCP servers
    // have finished connecting, and the refusal is the next thing said.
    assert!(matches!(
        next(&mut outputs, "started").await,
        SessionOutput::Started { .. }
    ));

    let SessionOutput::Fatal { error } = next(&mut outputs, "the mount refusal").await else {
        panic!("a session whose agent cannot read its budget must not run");
    };
    assert!(
        error.contains("machine_status"),
        "the refusal must name the missing tool: {error}"
    );
    assert!(
        error.contains("does not expose"),
        "the refusal must say the server answered but not with the tools: {error}"
    );
    assert!(
        outputs.recv().await.is_none(),
        "a fatal ends the session rather than pausing it"
    );
}

#[tokio::test]
async fn a_turn_runs_from_user_message_to_completion() {
    let scratch = Scratch::new("turn");
    let (session, mut outputs) = start(&scratch).await;

    // The session announces itself before anyone types: this is the whole
    // point of the warm-up, and it carries identity only.
    let SessionOutput::Started { session_id } = next(&mut outputs, "started").await else {
        panic!("the first output must be `started`");
    };
    assert_eq!(session_id, "fake-session");

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

    // Capabilities ride the first turn's `system/init`, so they cannot
    // arrive before this point — feature gating has to tolerate that.
    assert_eq!(
        next(&mut outputs, "capabilities").await,
        SessionOutput::Capabilities {
            capabilities: vec!["interrupt_receipt_v1".to_owned()],
        }
    );

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
async fn a_session_announces_itself_without_anyone_typing() {
    // The defect this guards: `started` used to be derived from the Agent
    // SDK's `system/init`, which the CLI emits at the start of a turn — so
    // a session that nobody had messaged yet never announced itself at all.
    // Identity now comes from the sidecar, which knows it at construction.
    let scratch = Scratch::new("announce");
    let (session, mut outputs) = start(&scratch).await;

    let announced = next(&mut outputs, "started").await;
    assert_eq!(
        announced,
        SessionOutput::Started {
            session_id: "fake-session".to_owned(),
        },
        "the session must announce itself before any user message"
    );

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
    let _ = next(&mut outputs, "capabilities").await;
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
    let _ = next(&mut outputs, "capabilities").await;
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
async fn manual_compaction_reaches_the_sidecar_and_reports_completion() {
    let scratch = Scratch::new("compact");
    let (session, mut outputs) = start(&scratch).await;
    let _ = next(&mut outputs, "started").await;

    session.compact().await.expect("compact the session");
    assert_eq!(
        next(&mut outputs, "context_compacted").await,
        SessionOutput::Event {
            event: HarnessEvent::ContextCompacted,
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
            managed_dir: None,
            auth: ClaudeAuth::Inherit,
        },
        SidecarConfig {
            dir: scratch.join("sidecar"),
            bun: PathBuf::from("definitely-not-a-real-bun"),
        },
        mount(),
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

#[tokio::test]
async fn a_sidecar_that_dies_mid_turn_reports_what_it_said_before_it_went() {
    // The scratch's name is what tells the stand-in sidecar to write to
    // stderr and exit instead of answering the turn.
    let scratch = Scratch::new("dies");
    let (session, mut outputs) = start(&scratch).await;

    // The write may well succeed: a pipe buffers, and the child is dying
    // rather than dead. What matters is what the driver does next.
    let _ = session.send_user_message("go".to_owned()).await;

    let fatal = loop {
        if let SessionOutput::Fatal { error } = next(&mut outputs, "the agent's death notice").await
        {
            break error;
        }
    };

    // Both halves, because neither alone is diagnosable: a process killed
    // by the OOM killer and one whose credential was refused both just
    // close their stdout.
    assert!(
        fatal.contains("exit") && fatal.contains('3'),
        "the notice must name the exit status, not just the fact: {fatal}"
    );
    assert!(
        fatal.contains("the credential was refused"),
        "the notice must quote what the process last said: {fatal}"
    );
    assert!(
        fatal.contains("sidecar.ts:42"),
        "the tail is more than one line, so a stack trace survives: {fatal}"
    );
}
