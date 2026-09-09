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

use flyco_core::{
    BillingMinimum, HarnessKind, MachineOrigin, PermissionMode, Runtime, SessionId, Usd,
};
use flyco_daemon::config::{ClaudeAuth, CodexAuth, DaemonConfig};
use flyco_provider::flycod::{
    self, CLAUDE_CONFIG_DIR, CLAUDE_MANAGED_DIR, CLAUDE_PROJECT_DIR_NAME, CODEX_HOME, WORKDIR,
};
use flyco_provider::{
    ClaudeCredential, CodexCredential, DaemonBootstrap, GitIdentity, HarnessCredential,
    RepoCheckout,
};

const CONTROL_PLANE: &str = "https://flyco.dev/";
const DAEMON_TOKEN: &str = "fd_a-token-from-the-control-plane";
const REPO: &str = "lexoliu/flyco";
const BRANCH: &str = "dev";
const GITHUB_TOKEN: &str = "gho_a-user-access-token";
const COMMIT_EMAIL: &str = "4242+lexoliu@users.noreply.github.com";

fn bootstrap(auth: HarnessCredential) -> DaemonBootstrap {
    DaemonBootstrap {
        session: SessionId::generate(),
        provider: flyco_core::CloudProviderKind::Azure,
        runtime: flyco_core::Runtime::Vm,
        control_plane_url: CONTROL_PLANE.to_owned(),
        daemon_token: DAEMON_TOKEN.to_owned(),
        permission_mode: PermissionMode::Auto,
        auth,
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
        model: flyco_provider::testing::session_model(),
        mcp_servers: flyco_provider::testing::mcp_servers(),
    }
}

fn claude(credential: ClaudeCredential) -> DaemonBootstrap {
    bootstrap(HarnessCredential::ClaudeCode(credential))
}

fn codex(credential: CodexCredential) -> DaemonBootstrap {
    bootstrap(HarnessCredential::Codex(credential))
}

fn parse(bootstrap: &DaemonBootstrap) -> DaemonConfig {
    let rendered = flycod::render(bootstrap).expect("the provisioner renders a configuration");
    toml::from_str(&rendered).unwrap_or_else(|error| {
        panic!("flycod refused the configuration a provisioner writes: {error}\n{rendered}")
    })
}

#[test]
fn a_provisioned_configuration_is_one_this_daemon_accepts() {
    let bootstrap = claude(ClaudeCredential::Inherit);
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
fn it_carries_the_model_the_session_was_opened_on_into_the_table_that_harness_reads() {
    // A rename on either side of this — `effort` becoming `reasoning_effort`,
    // the pair moving under one key — produces a machine that provisions,
    // boots, and then refuses its own configuration. Both harnesses are
    // checked because each reads its own table.
    let claude = parse(&claude(ClaudeCredential::Inherit));
    let claude = claude.claude.expect("claude");
    assert_eq!(claude.model.as_deref(), Some("sonnet"));
    assert_eq!(claude.effort.as_deref(), Some("high"));

    let codex = parse(&codex(CodexCredential::Inherit));
    let codex = codex.codex.expect("codex");
    assert_eq!(codex.model.as_deref(), Some("sonnet"));
    assert_eq!(codex.effort.as_deref(), Some("high"));
}

#[test]
fn a_session_that_chose_no_effort_arrives_with_none_rather_than_an_empty_one() {
    let mut bootstrap = claude(ClaudeCredential::Inherit);
    bootstrap.model.effort = None;
    let config = parse(&bootstrap);
    let claude = config.claude.expect("claude");
    assert_eq!(claude.model.as_deref(), Some("sonnet"));
    assert_eq!(
        claude.effort, None,
        "the harness's own default for the model is the answer, not a level flyco invented"
    );
}

#[test]
fn it_carries_the_machine_the_agent_is_told_about_and_who_chose_it() {
    let bootstrap = claude(ClaudeCredential::Inherit);
    let config = parse(&bootstrap);

    assert_eq!(config.machine_origin, MachineOrigin::User);
    assert_eq!(config.machine, bootstrap.machine);
}

#[test]
fn it_carries_whether_the_disk_survives_a_stop() {
    // The one fact nothing on the machine can discover for itself, and the
    // one that decides what SIGTERM means there: on a container the daemon
    // spends the platform's grace period writing the working tree out,
    // because nothing else will survive.
    let vm = parse(&claude(ClaudeCredential::Inherit));
    assert_eq!(vm.runtime, Runtime::Vm);

    let container = parse(&DaemonBootstrap {
        runtime: Runtime::Container,
        ..claude(ClaudeCredential::Inherit)
    });
    assert_eq!(container.runtime, Runtime::Container);
    assert!(!container.runtime.keeps_disk());
}

#[test]
fn a_spot_machine_arrives_knowing_whose_metadata_announces_its_reclamation() {
    // The daemon polls one endpoint out of three and probes none of them:
    // which one is a fact the provisioner holds and the machine cannot
    // recover for itself.
    let config = parse(&claude(ClaudeCredential::Inherit));
    assert_eq!(
        config.spot_provider,
        Some(flyco_core::CloudProviderKind::Azure)
    );

    let mut on_demand = claude(ClaudeCredential::Inherit);
    on_demand.machine.spot = false;
    assert_eq!(
        parse(&on_demand).spot_provider,
        None,
        "capacity nobody can reclaim is capacity with nothing to watch for"
    );
}

#[test]
fn a_license_bound_machine_arrives_with_the_minimum_it_billed() {
    let mut bootstrap = claude(ClaudeCredential::Inherit);
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
    let bootstrap = claude(ClaudeCredential::Inherit);
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
    let config = parse(&claude(ClaudeCredential::Inherit));

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
    let config = parse(&claude(ClaudeCredential::OauthToken {
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
fn it_carries_the_mcp_registry_and_the_directory_that_makes_it_binding() {
    // Both halves of the takeover, and neither is recoverable on the
    // machine: the servers are the user's registry, and the managed-policy
    // directory is what makes that registry an allowlist rather than a
    // suggestion.
    let bootstrap = claude(ClaudeCredential::Inherit);
    let config = parse(&bootstrap);

    assert_eq!(config.mcp_servers, bootstrap.mcp_servers);
    assert_eq!(
        config
            .mcp_servers
            .iter()
            .map(|server| server.name.as_str())
            .collect::<Vec<_>>(),
        ["deepwiki", "git"]
    );
    assert_eq!(
        config.claude.as_ref().expect("claude").managed_dir,
        Some(std::path::PathBuf::from(CLAUDE_MANAGED_DIR))
    );

    // A user with an empty registry still gets flyco's own server, and the
    // absent table is a document the daemon accepts rather than a required
    // key nobody could write.
    let mut none = claude(ClaudeCredential::Inherit);
    none.mcp_servers.clear();
    assert!(parse(&none).mcp_servers.is_empty());
}

#[test]
fn a_resuming_session_carries_its_harness_native_session_id() {
    let mut bootstrap = claude(ClaudeCredential::Inherit);
    bootstrap.resume_session_id = Some("1f6d2c50-8a4b-4a2b-9f6d-2c508a4b4a2b".to_owned());

    assert_eq!(
        parse(&bootstrap).resume_session_id,
        bootstrap.resume_session_id
    );
}

#[test]
fn a_provisioned_codex_configuration_is_one_this_daemon_accepts() {
    let config = parse(&codex(CodexCredential::ChatGpt {
        id_token: "header.payload.signature".to_owned(),
        access_token: "chatgpt-access".to_owned(),
        refresh_token: "chatgpt-refresh".to_owned(),
        account_id: "acc_01JD".to_owned(),
    }));

    assert_eq!(config.harness, HarnessKind::Codex);
    assert!(config.claude.is_none());
    let CodexAuth::ChatGpt {
        id_token,
        access_token,
        refresh_token,
        account_id,
        isolation,
    } = &config.codex.as_ref().expect("codex").auth
    else {
        panic!("a ChatGPT grant must parse back as one");
    };
    assert_eq!(id_token, "header.payload.signature");
    assert_eq!(access_token, "chatgpt-access");
    assert_eq!(refresh_token, "chatgpt-refresh");
    assert_eq!(account_id, "acc_01JD");
    assert_eq!(isolation.home, std::path::PathBuf::from(CODEX_HOME));
}

#[test]
fn a_provisioned_codex_api_key_arrives_with_its_isolated_home() {
    let config = parse(&codex(CodexCredential::ApiKey {
        key: "sk-proj-provisioned".to_owned(),
    }));

    let CodexAuth::ApiKey { key, isolation } = &config.codex.as_ref().expect("codex").auth else {
        panic!("an API key must parse back as one");
    };
    assert_eq!(key, "sk-proj-provisioned");
    assert_eq!(isolation.home, std::path::PathBuf::from(CODEX_HOME));
}
