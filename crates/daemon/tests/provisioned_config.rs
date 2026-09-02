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

use flyco_core::{BillingMinimum, HarnessKind, MachineOrigin, PermissionMode, SessionId, Usd};
use flyco_daemon::config::{ClaudeAuth, CodexAuth, DaemonConfig};
use flyco_provider::flycod::{
    self, CLAUDE_CONFIG_DIR, CLAUDE_PROJECT_DIR_NAME, CODEX_HOME, WORKDIR,
};
use flyco_provider::{ClaudeCredential, DaemonBootstrap, GitIdentity, RepoCheckout};

const CONTROL_PLANE: &str = "https://flyco.dev/";
const DAEMON_TOKEN: &str = "fd_a-token-from-the-control-plane";
const REPO: &str = "lexoliu/flyco";
const BRANCH: &str = "dev";
const GITHUB_TOKEN: &str = "gho_a-user-access-token";
const COMMIT_EMAIL: &str = "4242+lexoliu@users.noreply.github.com";

fn bootstrap(claude_auth: ClaudeCredential) -> DaemonBootstrap {
    DaemonBootstrap {
        session: SessionId::generate(),
        control_plane_url: CONTROL_PLANE.to_owned(),
        daemon_token: DAEMON_TOKEN.to_owned(),
        harness: HarnessKind::ClaudeCode,
        permission_mode: PermissionMode::Auto,
        claude_auth,
        repo: RepoCheckout {
            slug: REPO.parse().expect("a valid repository slug"),
            branch: BRANCH.parse().expect("a valid branch name"),
            token: GITHUB_TOKEN.to_owned(),
            identity: GitIdentity {
                name: "lexoliu".to_owned(),
                email: COMMIT_EMAIL.to_owned(),
            },
        },
        machine_origin: MachineOrigin::User,
        machine: flyco_provider::testing::session_machine(),
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
    assert_eq!(
        config.claude.as_ref().expect("claude").permission_mode,
        PermissionMode::Auto
    );
}

#[test]
fn it_carries_the_machine_the_agent_is_told_about_and_who_chose_it() {
    let bootstrap = bootstrap(ClaudeCredential::Inherit);
    let config = parse(&bootstrap);

    assert_eq!(config.machine_origin, MachineOrigin::User);
    assert_eq!(config.machine, bootstrap.machine);
}

#[test]
fn a_license_bound_machine_arrives_with_the_minimum_it_billed() {
    let mut bootstrap = bootstrap(ClaudeCredential::Inherit);
    bootstrap.machine.machine_type = "mac2.metal".to_owned();
    bootstrap.machine.minimum = Some(BillingMinimum::new(24, Usd::from_cents(65)));
    let config = parse(&bootstrap);

    assert_eq!(
        config.machine.minimum,
        Some(BillingMinimum::new(24, Usd::from_cents(65)))
    );
    assert!(config.machine.is_license_bound());
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
fn it_carries_the_repository_the_machine_has_to_check_out() {
    // The one field on this path that a machine cannot recover for itself:
    // an agent in an empty /srv/flyco/work has no way to find out which
    // repository it was opened for.
    let config = parse(&bootstrap(ClaudeCredential::Inherit));

    let repo = config
        .repo
        .expect("a provisioned machine always knows what to check out");
    assert_eq!(repo.slug.to_string(), REPO);
    assert_eq!(repo.branch.to_string(), BRANCH);
    assert_eq!(repo.token, GITHUB_TOKEN);
    assert_eq!(repo.identity.email, COMMIT_EMAIL);
    assert_eq!(repo.remote_url(), "https://github.com/lexoliu/flyco.git");
    assert!(
        !repo.remote_url().contains(GITHUB_TOKEN),
        "the token must never be written into a URL git records on disk"
    );
    assert!(
        !format!("{repo:?}").contains(GITHUB_TOKEN),
        "the token must never survive a Debug rendering"
    );
}

#[test]
fn an_injected_claude_credential_arrives_with_its_isolated_config_tree() {
    let config = parse(&bootstrap(ClaudeCredential::OauthToken {
        token: "sk-ant-oat01-provisioned".to_owned(),
    }));

    let ClaudeAuth::OauthToken { token, isolation } = &config.claude.as_ref().expect("claude").auth
    else {
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

#[test]
fn a_provisioned_codex_configuration_is_one_this_daemon_accepts() {
    let mut bootstrap = bootstrap(ClaudeCredential::OauthToken {
        token: "chatgpt-access".to_owned(),
    });
    bootstrap.harness = HarnessKind::Codex;
    let config = parse(&bootstrap);

    assert_eq!(config.harness, HarnessKind::Codex);
    assert!(config.claude.is_none());
    let CodexAuth::OauthToken { token, isolation } = &config.codex.as_ref().expect("codex").auth
    else {
        panic!("an oauth credential must parse back as Codex oauth");
    };
    assert_eq!(token, "chatgpt-access");
    assert_eq!(isolation.home, std::path::PathBuf::from(CODEX_HOME));
}
