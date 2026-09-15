//! The harness-TUI launch spec built from a provisioned configuration.
//!
//! `HarnessTui` is how a `TerminalHarness` command becomes a process: the
//! tests here pin the argv and environment the daemon's own configuration
//! produces, because the wire carries only `resume` — whatever the TUI is
//! launched with is decided entirely on this side.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use flyco_core::{DriverKind, MachineOrigin, PermissionMode, SessionId};
use flyco_daemon::config::DaemonConfig;
use flyco_daemon::tui::HarnessTui;
use flyco_provider::flycod::{self, CLAUDE_CONFIG_DIR, CODEX_HOME};
use flyco_provider::{
    CheckoutSpec, ClaudeCredential, CodexCredential, DaemonBootstrap, GitAccess, GitIdentity,
    HarnessCredential,
};
use portable_pty::CommandBuilder;

const CONTROL_PLANE: &str = "https://flyco.dev/";
const DAEMON_TOKEN: &str = "fd_a-token-from-the-control-plane";
const REPO: &str = "lexoliu/flyco";
const BRANCH: &str = "dev";
const GITHUB_TOKEN: &str = "gho_a-user-access-token";
const COMMIT_EMAIL: &str = "4242+lexoliu@users.noreply.github.com";
const OAUTH_TOKEN: &str = "sk-ant-oat01-a-subscription-token";
const OPENAI_KEY: &str = "sk-proj-an-openai-key";

fn bootstrap(auth: HarnessCredential) -> DaemonBootstrap {
    DaemonBootstrap {
        session: SessionId::generate(),
        provider: flyco_core::CloudProviderKind::Azure,
        runtime: flyco_core::Runtime::Vm,
        control_plane_url: CONTROL_PLANE.to_owned(),
        daemon_token: DAEMON_TOKEN.to_owned(),
        permission_mode: PermissionMode::Auto,
        auth,
        repos: vec![CheckoutSpec {
            slug: REPO.parse().expect("a valid repository slug"),
            branch: BRANCH.parse().expect("a valid branch name"),
            dir: "flyco".to_owned(),
        }],
        github: GitAccess {
            token: GITHUB_TOKEN.to_owned(),
            identity: GitIdentity {
                name: "lexoliu".to_owned(),
                email: COMMIT_EMAIL.to_owned(),
            },
        },
        machine_origin: MachineOrigin::User,
        machine: flyco_provider::testing::session_machine(),
        resume_session_id: None,
        model: flyco_provider::testing::session_model(),
        computer_use: true,
        mcp_servers: flyco_provider::testing::mcp_servers(),
    }
}

fn parse(bootstrap: &DaemonBootstrap) -> DaemonConfig {
    let rendered = flycod::render(bootstrap).expect("the provisioner renders a configuration");
    toml::from_str(&rendered).unwrap_or_else(|error| {
        panic!("flycod refused the configuration a provisioner writes: {error}\n{rendered}")
    })
}

/// A scratch dir holding a fake SDK package tree for `find_sdk_claude`.
fn sidecar_with_claude(root: &Path) -> PathBuf {
    let package = root.join("node_modules/@anthropic-ai/claude-agent-sdk-darwin-arm64");
    std::fs::create_dir_all(&package).expect("sdk package dir");
    let binary = package.join("claude");
    std::fs::write(&binary, "#!/bin/sh\n").expect("the bundled binary");
    root.to_path_buf()
}

fn args_of(command: &CommandBuilder) -> Vec<String> {
    command
        .get_argv()
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

#[tokio::test]
async fn a_claude_launch_names_the_sdk_binary_the_model_and_the_mode() {
    let root = std::env::temp_dir().join(format!("flycod-tui-{}", std::process::id()));
    let mut config = parse(&bootstrap(HarnessCredential::ClaudeCode(
        ClaudeCredential::OauthToken {
            token: OAUTH_TOKEN.to_owned(),
        },
    )));
    config.sidecar.as_mut().expect("sidecar").dir = sidecar_with_claude(&root);

    let tui = HarnessTui::resolve(&config).await;
    let command = tui.command(false).expect("the launch resolves");

    let argv = args_of(&command);
    assert_eq!(
        argv[0],
        root.join("node_modules/@anthropic-ai/claude-agent-sdk-darwin-arm64/claude")
            .to_string_lossy(),
        "the SDK's bundled binary, not a PATH lookup: {argv:?}"
    );
    assert!(
        argv.windows(2).any(|w| w == ["--model", "sonnet"]),
        "{argv:?}"
    );
    assert!(
        argv.windows(2).any(|w| w == ["--permission-mode", "auto"]),
        "{argv:?}"
    );
    assert_eq!(
        command.get_env("CLAUDE_CONFIG_DIR"),
        Some(OsStr::new(CLAUDE_CONFIG_DIR)),
        "the isolated config tree the credential belongs to"
    );
    assert_eq!(
        command.get_env("CLAUDE_CODE_OAUTH_TOKEN"),
        Some(OsStr::new(OAUTH_TOKEN)),
        "the credential reaches the TUI through the environment"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_claude_resume_continues_or_names_the_session() {
    let root = std::env::temp_dir().join(format!("flycod-tui-resume-{}", std::process::id()));
    let mut bootstrap = bootstrap(HarnessCredential::ClaudeCode(ClaudeCredential::Inherit));
    let mut config = parse(&bootstrap);
    config.sidecar.as_mut().expect("sidecar").dir = sidecar_with_claude(&root);

    // No harness-native id recorded: re-entry is `--continue`, the CLI's
    // own "latest conversation" spelling.
    let tui = HarnessTui::resolve(&config).await;
    let argv = args_of(&tui.command(true).expect("the resume resolves"));
    assert!(argv.contains(&"--continue".to_owned()), "{argv:?}");
    assert!(!argv.contains(&"--resume".to_owned()), "{argv:?}");

    // With one recorded the re-entry names it — cross-machine history is
    // the whole reason the field exists.
    bootstrap.resume_session_id = Some("a-harness-session-id".to_owned());
    let mut config = parse(&bootstrap);
    config.sidecar.as_mut().expect("sidecar").dir = sidecar_with_claude(&root);
    let tui = HarnessTui::resolve(&config).await;
    let argv = args_of(&tui.command(true).expect("the resume resolves"));
    assert!(
        argv.windows(2)
            .any(|w| w == ["--resume", "a-harness-session-id"]),
        "{argv:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_missing_claude_binary_is_an_error_not_a_guess() {
    let root = std::env::temp_dir().join(format!("flycod-tui-missing-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("an empty sidecar dir");
    let mut config = parse(&bootstrap(HarnessCredential::ClaudeCode(
        ClaudeCredential::Inherit,
    )));
    config.sidecar.as_mut().expect("sidecar").dir = root.clone();

    let tui = HarnessTui::resolve(&config).await;
    let error = tui.command(false).expect_err("no binary, no launch");
    assert!(
        error.to_string().contains("no claude binary"),
        "the error names what is missing: {error}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_codex_launch_names_the_configured_tui_under_the_agents_environment() {
    let mut bootstrap = bootstrap(HarnessCredential::Codex(CodexCredential::ApiKey {
        key: OPENAI_KEY.to_owned(),
    }));
    bootstrap.permission_mode = PermissionMode::AcceptEdits;
    let config = parse(&bootstrap);
    assert_eq!(config.harness, DriverKind::Acp);

    let tui = HarnessTui::resolve(&config).await;
    let command = tui.command(false).expect("the launch resolves");
    let argv = args_of(&command);

    // The TUI is the agent's own — `codex`, fresh — with the session's
    // model and mode already applied to the session it re-enters, so the
    // launch itself takes no overrides.
    assert_eq!(argv, ["codex"], "the [acp.tui] program and args: {argv:?}");
    assert_eq!(
        command.get_env("CODEX_HOME"),
        Some(OsStr::new(CODEX_HOME)),
        "the isolated home holding auth.json reaches the TUI"
    );
    // The key itself never appears: it lives in `auth.json` under
    // CODEX_HOME, written at provisioning.
    assert!(
        !argv.iter().any(|arg| arg.contains(OPENAI_KEY)),
        "no credential on argv: {argv:?}"
    );
}

#[tokio::test]
async fn a_codex_resume_names_the_thread_or_the_picker() {
    let mut bootstrap = bootstrap(HarnessCredential::Codex(CodexCredential::Inherit));
    let config = parse(&bootstrap);

    // Nothing recorded: the `{session}` placeholder is dropped, and bare
    // `codex resume` lands on the agent's own conversation picker.
    let tui = HarnessTui::resolve(&config).await;
    let argv = args_of(&tui.command(true).expect("the resume resolves"));
    assert_eq!(argv, ["codex", "resume"], "{argv:?}");

    bootstrap.resume_session_id = Some("a-codex-thread-id".to_owned());
    let config = parse(&bootstrap);
    let tui = HarnessTui::resolve(&config).await;
    let argv = args_of(&tui.command(true).expect("the resume resolves"));
    assert_eq!(argv, ["codex", "resume", "a-codex-thread-id"], "{argv:?}");
}
