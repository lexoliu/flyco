//! The configuration a provisioner writes is the configuration this daemon
//! reads.
//!
//! `flyco_provider::flycod` renders the `flycod` config that every driver
//! installs onto a session machine — Azure through cloud-init, byo-ssh
//! through the container's environment. Nothing else checks that the result
//! is a document *this* binary accepts: [`DaemonConfig`] is
//! `deny_unknown_fields` and every field is required, so a rename on either
//! side produces a machine that provisions cleanly, boots, and then refuses
//! its own configuration — a failure that only shows up on a real VM.
//!
//! So the two sides are pinned here. The provisioner renders; the daemon's
//! own loader parses; the assertions are on the fields the control plane
//! depends on being carried.

use flyco_core::{HarnessKind, PermissionMode, SessionId};
use flyco_daemon::config::{ClaudeAuth, DaemonConfig};
use flyco_provider::flycod::{self, CLAUDE_CONFIG_DIR, CLAUDE_PROJECT_DIR_NAME, WORKDIR};
use flyco_provider::{ClaudeCredential, DaemonBootstrap};

const CONTROL_PLANE: &str = "https://flyco.dev/";
const DAEMON_TOKEN: &str = "fd_a-token-from-the-control-plane";

fn bootstrap(claude_auth: ClaudeCredential) -> DaemonBootstrap {
    DaemonBootstrap {
        session: SessionId::generate(),
        control_plane_url: CONTROL_PLANE.to_owned(),
        daemon_token: DAEMON_TOKEN.to_owned(),
        harness: HarnessKind::ClaudeCode,
        permission_mode: PermissionMode::Default,
        claude_auth,
        resume_session_id: None,
    }
}

fn parse(bootstrap: &DaemonBootstrap) -> DaemonConfig {
    let rendered = flycod::render(bootstrap).expect("the provisioner renders a configuration");
    toml::from_str(&rendered).unwrap_or_else(|error| {
        panic!("flycod refused the configuration a provisioner writes: {error}\n{rendered}")
    })
}

#[test]
fn a_provisioned_configuration_is_one_this_daemon_accepts() {
    let bootstrap = bootstrap(ClaudeCredential::Inherit);
    let config = parse(&bootstrap);

    assert_eq!(config.session, bootstrap.session);
    assert_eq!(config.harness, HarnessKind::ClaudeCode);
    assert_eq!(config.workdir, std::path::PathBuf::from(WORKDIR));
    assert_eq!(config.claude.permission_mode, PermissionMode::Default);
}

#[test]
fn it_carries_the_control_plane_and_a_token_the_daemon_will_accept() {
    let bootstrap = bootstrap(ClaudeCredential::Inherit);
    let config = parse(&bootstrap);

    let control_plane = config
        .control_plane
        .expect("a provisioned machine always reports to a control plane");
    assert_eq!(control_plane.url.as_str(), CONTROL_PLANE);
    assert_eq!(control_plane.daemon_token, DAEMON_TOKEN);
    control_plane
        .validate()
        .expect("the provisioner writes a `fd_` daemon token");
}

#[test]
fn an_injected_claude_credential_arrives_with_its_isolated_config_tree() {
    let config = parse(&bootstrap(ClaudeCredential::OauthToken {
        token: "sk-ant-oat01-provisioned".to_owned(),
    }));

    let ClaudeAuth::OauthToken { token, isolation } = &config.claude.auth else {
        panic!("an oauth credential must parse back as one");
    };
    assert_eq!(token, "sk-ant-oat01-provisioned");
    assert_eq!(
        isolation.config_dir,
        std::path::PathBuf::from(CLAUDE_CONFIG_DIR)
    );
    assert_eq!(isolation.project_dir_name, CLAUDE_PROJECT_DIR_NAME);
}

#[test]
fn a_resuming_session_carries_its_harness_native_session_id() {
    let mut bootstrap = bootstrap(ClaudeCredential::Inherit);
    bootstrap.resume_session_id = Some("1f6d2c50-8a4b-4a2b-9f6d-2c508a4b4a2b".to_owned());

    assert_eq!(
        parse(&bootstrap).resume_session_id,
        bootstrap.resume_session_id
    );
}
