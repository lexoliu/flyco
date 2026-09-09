//! The Codex driver, end to end against a stand-in app-server.
//!
//! `fake-app-server.py` speaks the JSON-RPC subset flycod uses, so this
//! exercises handshake, `Started`, a turn, approval routing, usage, and
//! interrupt without a `codex` binary.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use flyco_core::HarnessEvent;
use flyco_daemon::config::{CodexApprovalPolicy, CodexAuth, CodexConfig, CodexSandbox};
use flyco_daemon::harness::codex::CodexHarness;
use flyco_daemon::harness::{Harness as _, HarnessSession as _, SessionOutput, StartRequest};
use flyco_daemon::mount::{FlycoServer, Mount};
use tokio::sync::mpsc;

/// The MCP servers a test session is given.
///
/// Only flyco's own, and it is never launched: the stand-in app-server
/// reports the mount rather than performing it. What the driver does with
/// the report is what these tests are about.
fn mount() -> Mount {
    Mount::new(
        FlycoServer::of(Path::new("/etc/flyco/flycod.toml")).expect("this test binary has a path"),
        Vec::new(),
    )
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "flycod-app-server-{name}-{}-{:?}",
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

async fn start(
    scratch: &Scratch,
) -> (
    flyco_daemon::harness::codex::CodexSession,
    mpsc::Receiver<SessionOutput>,
) {
    let harness = CodexHarness::new(
        CodexConfig {
            bin: script("fake-app-server.py"),
            model: None,
            effort: None,
            approval_policy: CodexApprovalPolicy::OnRequest,
            sandbox: CodexSandbox::WorkspaceWrite,
            auth: CodexAuth::Inherit,
        },
        mount(),
    );
    let started = harness
        .start(StartRequest {
            workdir: scratch.join("work"),
            resume_session_id: None,
        })
        .await
        .expect("the driver must start against the stand-in app-server");
    (started.session, started.outputs)
}

#[tokio::test]
async fn a_thread_that_opened_without_flycos_tools_never_becomes_a_session() {
    // The scratch's name is what tells the stand-in app-server to report a
    // flyco server with `machine_status` missing. Unlike the Claude driver,
    // which learns this after the session is identified, Codex is asked
    // during the handshake — so the failure is `start` refusing, and no
    // session exists to fail.
    let scratch = Scratch::new("unmounted");
    let harness = CodexHarness::new(
        CodexConfig {
            bin: script("fake-app-server.py"),
            model: None,
            effort: None,
            approval_policy: CodexApprovalPolicy::OnRequest,
            sandbox: CodexSandbox::WorkspaceWrite,
            auth: CodexAuth::Inherit,
        },
        mount(),
    );
    let error = harness
        .start(StartRequest {
            workdir: scratch.join("work"),
            resume_session_id: None,
        })
        .await
        .expect_err("a thread whose agent cannot read its budget must not open");
    let said = error.to_string();
    assert!(
        said.contains("machine_status"),
        "the refusal must name the missing tool: {said}"
    );
}

async fn next(outputs: &mut mpsc::Receiver<SessionOutput>, what: &str) -> SessionOutput {
    tokio::time::timeout(std::time::Duration::from_secs(10), outputs.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
        .unwrap_or_else(|| panic!("the session ended before {what}"))
}

/// Steps past the two announcements every session opens with.
///
/// A thread is identified and its models are listed before any turn, in
/// that order, so a test about turns walks past both rather than restating
/// them — `a_session_announces_itself_without_anyone_typing` is where their
/// content is pinned.
async fn announcements(outputs: &mut mpsc::Receiver<SessionOutput>) {
    let _ = next(outputs, "started").await;
    let _ = next(outputs, "models").await;
}

#[tokio::test]
async fn a_session_announces_itself_without_anyone_typing() {
    let scratch = Scratch::new("announce");
    let (session, mut outputs) = start(&scratch).await;

    assert_eq!(
        next(&mut outputs, "started").await,
        SessionOutput::Started {
            session_id: "fake-thread".to_owned(),
        }
    );

    // And what it can run on, before a single turn: the composer's picker
    // has to be right for the first message. A row the app-server marks
    // hidden is not one a user may choose, so it is not in the answer.
    let SessionOutput::Models { models } = next(&mut outputs, "models").await else {
        panic!("a session must announce the models its harness offers");
    };
    assert_eq!(
        models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        ["gpt-5.6-terra", "gpt-5.6-luna"],
        "a hidden model is dropped rather than offered"
    );
    let default = models
        .iter()
        .find(|model| model.is_default)
        .expect("the app-server names a default");
    assert_eq!(default.id, "gpt-5.6-terra");
    assert_eq!(default.default_effort.as_deref(), Some("medium"));
    assert_eq!(default.efforts, ["low", "medium", "high"]);

    session.shutdown().await.expect("shut the session down");
}

#[tokio::test]
async fn a_turn_runs_from_user_message_to_completion() {
    let scratch = Scratch::new("turn");
    let (session, mut outputs) = start(&scratch).await;
    announcements(&mut outputs).await;

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
    assert_eq!(turn_id, "fake-turn");

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
    assert_eq!(tool, "commandExecution");

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
    assert_eq!(
        usage.context.expect("codex reports a window").size_tokens,
        200_000
    );

    session.shutdown().await.expect("shut the session down");
}

#[tokio::test]
async fn an_interrupted_turn_fails_rather_than_completing() {
    let scratch = Scratch::new("interrupt");
    let (session, mut outputs) = start(&scratch).await;
    announcements(&mut outputs).await;

    session
        .send_user_message("hi".to_owned())
        .await
        .expect("send a user message");
    let _ = next(&mut outputs, "turn_started").await;
    let _ = next(&mut outputs, "assistant_delta").await;
    let _ = next(&mut outputs, "approval_request").await;

    session.interrupt().await.expect("interrupt the turn");

    let SessionOutput::Event {
        event: HarnessEvent::TurnFailed { error, .. },
    } = next(&mut outputs, "turn_failed").await
    else {
        panic!("an interrupt must fail the turn");
    };
    assert_eq!(error, "interrupted");

    session.shutdown().await.expect("shut the session down");
}

#[tokio::test]
async fn manual_compaction_uses_the_native_thread_method_and_reports_completion() {
    let scratch = Scratch::new("compact");
    let (session, mut outputs) = start(&scratch).await;
    announcements(&mut outputs).await;

    session.compact().await.expect("compact the thread");
    assert_eq!(
        next(&mut outputs, "context_compacted").await,
        SessionOutput::Event {
            event: HarnessEvent::ContextCompacted,
        }
    );

    session.shutdown().await.expect("shut the session down");
}
