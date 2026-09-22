//! The generic ACP driver, end to end against a stand-in agent.
//!
//! `fake-acp-agent.py` speaks the ACP subset flycod uses, so this exercises
//! handshake, session open, a turn, permission routing, plan usage, and
//! interrupt without a real agent — and the `[acp]` table is the whole
//! configuration, which is what makes one test file cover every agent the
//! driver could run.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use flyco_core::{HarnessEvent, PermissionMode};
use flyco_daemon::config::{AcpConfig, AcpMethodCall, AcpMethods, AcpMode};
use flyco_daemon::harness::acp::AcpHarness;
use flyco_daemon::harness::{Harness as _, HarnessSession as _, SessionOutput, StartRequest};
use flyco_daemon::mount::{FlycoServer, Mount};
use tokio::sync::mpsc;

/// The MCP servers a test session is given.
///
/// Only flyco's own, and it is never launched: the stand-in agent reports
/// the mount rather than performing it. What the driver does with the
/// report is what these tests are about.
fn mount() -> Mount {
    Mount::new(
        FlycoServer::of(Path::new("/etc/flyco/flycod.toml")).expect("this test binary has a path"),
        Vec::new(),
        false,
    )
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "flycod-acp-{name}-{}-{:?}",
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

/// The `[acp]` table the tests run under, as a provisioner would write it:
/// every extension method named explicitly, so what is exercised is the
/// configuration-driven path rather than a special case.
fn config() -> AcpConfig {
    AcpConfig {
        agent: "fake".to_owned(),
        program: script("fake-acp-agent.py"),
        args: Vec::new(),
        env: BTreeMap::new(),
        files: Vec::new(),
        model: None,
        effort: None,
        model_option: "model".to_owned(),
        effort_option: Some("reasoning_effort".to_owned()),
        fused_effort_tails: Vec::new(),
        permission_mode: PermissionMode::Auto,
        modes: BTreeMap::from([(
            PermissionMode::Auto,
            AcpMode {
                set_mode: Some("agent".to_owned()),
                options: Vec::new(),
            },
        )]),
        methods: AcpMethods {
            compact: Some(AcpMethodCall {
                call: "thread/compact/start".to_owned(),
                params: serde_json::json!({"threadId": "<session>"}),
            }),
            usage: Some(AcpMethodCall {
                call: "account/rateLimits/read".to_owned(),
                params: serde_json::Value::Null,
            }),
            mcp_status: Some(AcpMethodCall {
                call: "mcpServerStatus/list".to_owned(),
                params: serde_json::Value::Null,
            }),
        },
        tui: None,
    }
}

async fn start(
    scratch: &Scratch,
) -> (
    flyco_daemon::harness::acp::AcpSession,
    mpsc::Receiver<SessionOutput>,
) {
    start_with(scratch, None).await
}

async fn start_with(
    scratch: &Scratch,
    resume_session_id: Option<String>,
) -> (
    flyco_daemon::harness::acp::AcpSession,
    mpsc::Receiver<SessionOutput>,
) {
    let harness = AcpHarness::new(config(), mount());
    let started = harness
        .start(StartRequest {
            workdir: scratch.join("work"),
            resume_session_id,
        })
        .await
        .expect("the driver must start against the stand-in agent");
    (started.session, started.outputs)
}

#[tokio::test]
async fn a_session_opened_without_flycos_tools_never_becomes_a_session() {
    // The scratch's name is what tells the stand-in agent to report a
    // flyco server with `machine_status` missing. The mount report is
    // asked for before the session is allowed to produce anything, so the
    // failure is `start` refusing, and no session exists to fail.
    let scratch = Scratch::new("unmounted");
    let harness = AcpHarness::new(config(), mount());
    let error = harness
        .start(StartRequest {
            workdir: scratch.join("work"),
            resume_session_id: None,
        })
        .await
        .expect_err("a session whose agent cannot read its budget must not open");
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

/// Steps past the announcements every session opens with.
///
/// A session is identified, its capabilities, models and commands are
/// announced, and its plan's limits are read, before any turn — so a test
/// about turns walks past all five rather than restating them.
/// `a_session_announces_itself_without_anyone_typing` is where their
/// content is pinned.
async fn announcements(outputs: &mut mpsc::Receiver<SessionOutput>) {
    let _ = next(outputs, "started").await;
    let _ = next(outputs, "capabilities").await;
    let _ = next(outputs, "models").await;
    let _ = next(outputs, "commands").await;
}

#[tokio::test]
async fn a_session_announces_itself_without_anyone_typing() {
    let scratch = Scratch::new("announce");
    let (session, mut outputs) = start(&scratch).await;

    assert_eq!(
        next(&mut outputs, "started").await,
        SessionOutput::Started {
            session_id: "fake-session".to_owned(),
        }
    );

    let SessionOutput::Capabilities { capabilities } = next(&mut outputs, "capabilities").await
    else {
        panic!("a session must announce what its agent negotiated");
    };
    for token in [
        "acp",
        "acp:load_session",
        "acp:resume",
        "acp:close",
        "acp:mcp_http",
    ] {
        assert!(
            capabilities.iter().any(|c| c == token),
            "{token} missing from {capabilities:?}"
        );
    }

    // And what it can run on, before a single turn: the composer's picker
    // has to be right for the first message.
    let SessionOutput::Models { models } = next(&mut outputs, "models").await else {
        panic!("a session must announce the models its harness offers");
    };
    assert_eq!(
        models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        ["gpt-5.6-terra", "gpt-5.6-luna"],
    );
    let default = models
        .iter()
        .find(|model| model.is_default)
        .expect("the agent names a current model");
    assert_eq!(default.id, "gpt-5.6-terra");
    assert_eq!(default.default_effort.as_deref(), Some("medium"));
    assert_eq!(default.efforts, ["low", "medium", "high"]);

    let SessionOutput::Commands { commands } = next(&mut outputs, "commands").await else {
        panic!("a session must announce the commands its harness offers");
    };
    assert_eq!(
        commands
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>(),
        ["cloudflare"]
    );
    assert_eq!(commands[0].argument_hint, None);
    assert!(
        commands[0]
            .description
            .starts_with("Comprehensive Cloudflare")
    );

    session.shutdown().await.expect("shut the session down");
}

#[tokio::test]
async fn a_resuming_session_recovers_its_native_id() {
    // The id the session was opened under is the one a later machine
    // resumes by; the driver names it on `session/resume` and the agent's
    // answer carries the mode and options state the open announced.
    let scratch = Scratch::new("resume");
    let (session, mut outputs) = start_with(&scratch, Some("a-native-session-id".to_owned())).await;

    assert_eq!(
        next(&mut outputs, "started").await,
        SessionOutput::Started {
            session_id: "a-native-session-id".to_owned(),
        }
    );

    session.shutdown().await.expect("shut the session down");
}

#[tokio::test]
async fn a_load_the_agent_rejects_opens_fresh_and_says_so() {
    // `session/load` is what a resumed machine asks for; when the agent
    // refuses it, the session is the restored workspace plus a fresh
    // conversation — not a startup failure — and the room is told the
    // context did not carry over.
    let scratch = Scratch::new("refuseload");
    let (session, mut outputs) = start_with(&scratch, Some("a-lost-session".to_owned())).await;

    assert_eq!(
        next(&mut outputs, "started").await,
        SessionOutput::Started {
            session_id: "fake-session".to_owned(),
        },
        "a fresh conversation carries its own id, not the rejected one"
    );
    let _ = next(&mut outputs, "capabilities").await;
    let _ = next(&mut outputs, "models").await;
    let _ = next(&mut outputs, "commands").await;
    let SessionOutput::Event {
        event: HarnessEvent::LocalCommandOutput { content },
    } = next(&mut outputs, "the restart notice").await
    else {
        panic!("a refused continuation must say so in the room");
    };
    assert!(content.contains("session/load"), "{content}");

    session.shutdown().await.expect("shut the session down");
}

#[tokio::test]
async fn an_agent_that_cannot_continue_starts_fresh_and_says_so() {
    // No `session/resume`, no `session/load`: the conversation cannot be
    // continued, and an unresumable session is still not a dead one — the
    // work in the checkout is the part that mattered.
    let scratch = Scratch::new("continuless");
    let (session, mut outputs) = start_with(&scratch, Some("a-lost-session".to_owned())).await;

    assert_eq!(
        next(&mut outputs, "started").await,
        SessionOutput::Started {
            session_id: "fake-session".to_owned(),
        }
    );
    let _ = next(&mut outputs, "capabilities").await;
    let _ = next(&mut outputs, "models").await;
    let _ = next(&mut outputs, "commands").await;
    let SessionOutput::Event {
        event: HarnessEvent::LocalCommandOutput { content },
    } = next(&mut outputs, "the restart notice").await
    else {
        panic!("an uncontinuable resume must say so in the room");
    };
    assert!(content.contains("neither resume nor load"), "{content}");

    session.shutdown().await.expect("shut the session down");
}

#[tokio::test]
async fn a_palette_update_mid_session_re_lists_the_commands() {
    // The scratch's name is what tells the stand-in agent to push an
    // `available_commands_update` when a turn opens: the palette the
    // browser holds has to be re-rendered rather than left stale.
    let scratch = Scratch::new("skillsreload");
    let (session, mut outputs) = start(&scratch).await;
    announcements(&mut outputs).await;

    session
        .send_user_message("hi".to_owned())
        .await
        .expect("send a user message");

    // The palette update races the turn's own frames, so this reads
    // forward to it rather than assuming it lands first.
    let mut relisted = None;
    while relisted.is_none() {
        if let SessionOutput::Commands { commands } =
            next(&mut outputs, "the new command list").await
        {
            relisted = Some(commands);
        }
    }
    let commands = relisted.expect("checked above");
    assert_eq!(
        commands
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>(),
        ["cloudflare", "release"],
        "the agent's new palette is the one announced"
    );
    assert_eq!(commands[1].argument_hint.as_deref(), Some("version"));

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
    assert_eq!(tool, "execute");

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
    assert_eq!(
        usage
            .context
            .expect("the agent reported a window")
            .size_tokens,
        200_000
    );
    assert_eq!(usage.context.expect("checked above").used_tokens, 18);

    // The window the turn left behind, reported unasked: a session whose
    // machine is suspended later still says where its context went,
    // instead of offering to wake one to find out.
    let SessionOutput::Event {
        event: HarnessEvent::ContextUsage { usage: context },
    } = next(&mut outputs, "context usage").await
    else {
        panic!("a finished turn must state what the window holds");
    };
    assert_eq!(
        context
            .window
            .expect("the agent reported a window")
            .size_tokens,
        200_000
    );

    // The turn spent the five-hour window. The reading itself is filed
    // nowhere — the control plane reads the plan from the vendor while it
    // draws it — and what the driver still owes the conversation is the
    // limit: the session is stopped until the window turns over, and the
    // transcript has to say which limit stopped it.
    let SessionOutput::Event {
        event: HarnessEvent::UsageLimited { window },
    } = next(&mut outputs, "usage limited").await
    else {
        panic!("a spent window must reach the conversation");
    };
    assert_eq!(window.label, "5-hour (primary)");
    assert_eq!(window.used_percent, 100);
    assert_eq!(window.resets_at_unix, Some(1_789_002_000));

    session.shutdown().await.expect("shut the session down");
}

#[tokio::test]
async fn a_message_sent_mid_turn_waits_and_opens_the_next_turn() {
    // ACP refuses a second `session/prompt` while a turn runs, so the
    // driver queues what arrives during one — a follow-up from the user,
    // an injected notice — and opens it when the running turn closes. A
    // refusal there used to be fatal: it killed the session.
    let scratch = Scratch::new("queued");
    let (session, mut outputs) = start(&scratch).await;
    announcements(&mut outputs).await;

    session
        .send_user_message("first".to_owned())
        .await
        .expect("send the first message");
    let SessionOutput::Event {
        event: HarnessEvent::TurnStarted { turn_id: first },
    } = next(&mut outputs, "the first turn").await
    else {
        panic!("the first message must open a turn");
    };

    // The approval request is the turn's still-open marker: the prompt has
    // not resolved, so this message lands mid-turn.
    let _ = next(&mut outputs, "assistant_delta").await;
    let SessionOutput::ApprovalRequest { id, .. } = next(&mut outputs, "approval_request").await
    else {
        panic!("the first turn must be waiting on its approval");
    };
    session
        .send_user_message("second".to_owned())
        .await
        .expect("a mid-turn message is accepted, not fatal");

    session
        .decide_approval(flyco_daemon::harness::ToolApproval::Allow {
            id,
            updated_input: None,
        })
        .await
        .expect("decide the first turn's approval");

    // The queued message opens a turn of its own once the first closes.
    // Plan usage and the second TurnStarted are emitted from different
    // tasks, so read past whichever lands first.
    let mut second_turn = None;
    for _ in 0..4 {
        if let SessionOutput::Event {
            event: HarnessEvent::TurnStarted { turn_id },
        } = next(&mut outputs, "the queued message's turn").await
        {
            second_turn = Some(turn_id);
            break;
        }
    }
    let second = second_turn.expect("the queued message must open its own turn");
    assert_ne!(first, second, "each message opens a distinct turn");

    session.shutdown().await.expect("shut the session down");
}

#[tokio::test]
async fn an_interrupted_turn_completes_as_cancelled() {
    // ACP's interrupt is `session/cancel` plus the prompt resolving with
    // `stopReason: "cancelled"` — a cancelled turn is a completed one in
    // the protocol's vocabulary, and the transcript closes it rather than
    // leaving it open.
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
        event: HarnessEvent::TurnCompleted { .. },
    } = next(&mut outputs, "turn_completed").await
    else {
        panic!("an interrupt must close the turn");
    };

    session.shutdown().await.expect("shut the session down");
}

#[tokio::test]
async fn manual_compaction_uses_the_configured_method_and_reports_completion() {
    let scratch = Scratch::new("compact");
    let (session, mut outputs) = start(&scratch).await;
    announcements(&mut outputs).await;

    session.compact().await.expect("compact the session");
    assert_eq!(
        next(&mut outputs, "context_compacted").await,
        SessionOutput::Event {
            event: HarnessEvent::ContextCompacted,
        }
    );

    session.shutdown().await.expect("shut the session down");
}
