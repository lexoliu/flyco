//! The `flycod` binary.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use flyco_core::{DriverKind, ProvisioningStage};
use flyco_daemon::config::{ControlPlaneConfig, DaemonConfig, EXAMPLE};
use flyco_daemon::control::{
    ControlApi, HttpControlApi, RemoteTranscriptStore, SessionRelay, wire,
};
use flyco_daemon::harness::acp::AcpHarness;
use flyco_daemon::harness::claude::ClaudeCodeHarness;
use flyco_daemon::harness::claude::store::{JsonlTranscriptStore, TranscriptStore};
use flyco_daemon::harness::{Harness as _, HarnessSession, StartRequest, Started};
use flyco_daemon::host;
use flyco_daemon::mcp::FlycoTools;
use flyco_daemon::mount::{FlycoServer, Mount};
use flyco_daemon::repl;
use flyco_daemon::skills;
use rmcp::ServiceExt as _;
use rmcp::transport::stdio;
use tokio::io::AsyncWriteExt as _;
use tracing_subscriber::EnvFilter;
use url::Url;

/// flyco's execution-plane daemon.
#[derive(Debug, Parser)]
#[command(name = "flycod", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the daemon: start the harness and drive it.
    Run {
        /// Path to the TOML configuration.
        ///
        /// Kept after loading rather than dropped: it is what the harness
        /// is told to pass to the `flycod mcp` it launches, so the second
        /// process reads the same session and the same daemon token.
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
    },
    /// Serve flyco's tools to the harness over stdio (MCP).
    ///
    /// Launched by the coding harness, not by the machine: it is a second
    /// process beside `flycod run`, sharing only the configuration file and
    /// the session's daemon token.
    Mcp {
        /// Path to the TOML configuration.
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
    },
    /// Print a complete, valid configuration to stdout.
    ExampleConfig,
    /// Boot a GitHub codespace as a flyco session machine.
    ///
    /// The `postStartCommand` of the environment repository's devcontainer:
    /// fetches this session's daemon configuration from the control plane —
    /// authenticated by the `CODESPACE_NAME`/`GITHUB_TOKEN` pair GitHub
    /// injects — writes it, and starts `flycod run` detached.
    Codespace,
    /// Act as a machine the user owns, rather than as a session VM.
    ///
    /// The same binary in its other role: `flycod host run` holds the
    /// command stream this machine's room serves and runs the session
    /// containers the control plane sends it (docs/host-enrollment.md).
    Host {
        #[command(subcommand)]
        command: HostCommand,
    },
}

#[derive(Debug, Subcommand)]
enum HostCommand {
    /// Register this machine with a control plane and write its
    /// configuration.
    ///
    /// Run once, by the installer, with the single-use token the enrollment
    /// wizard printed. It measures the machine, spends the token, and writes
    /// the long-lived one root-only.
    Enroll {
        /// The single-use `fh_` enrollment token.
        #[arg(long, value_name = "TOKEN")]
        token: String,
        /// The control plane that minted it.
        #[arg(long, value_name = "URL")]
        control_plane: Url,
        /// Where rootless Podman keeps this machine's containers and
        /// volumes. Its free space is what the control plane schedules
        /// against.
        #[arg(long, value_name = "PATH", default_value = host::config::DEFAULT_VOLUME_ROOT)]
        volume_root: PathBuf,
        /// Where to write the configuration.
        #[arg(long, value_name = "PATH", default_value = host::config::DEFAULT_PATH)]
        config: PathBuf,
    },
    /// Hold this machine's relay and run the container jobs it is sent.
    Run {
        /// Path to the configuration `flycod host enroll` wrote.
        #[arg(long, value_name = "PATH", default_value = host::config::DEFAULT_PATH)]
        config: PathBuf,
    },
}

/// Anything that stops `flycod` before it finishes.
#[derive(Debug, thiserror::Error)]
enum Failure {
    #[error(transparent)]
    Config(#[from] flyco_daemon::config::ConfigError),
    #[error(transparent)]
    Claude(#[from] flyco_daemon::harness::claude::ClaudeError),
    #[error(transparent)]
    Acp(#[from] flyco_daemon::harness::acp::AcpError),
    #[error(transparent)]
    Repl(#[from] repl::ReplError),
    #[error(transparent)]
    Wire(#[from] flyco_daemon::control::WireError),
    #[error(transparent)]
    Terminal(#[from] flyco_daemon::terminal::TerminalError),
    #[error(transparent)]
    Git(#[from] flyco_daemon::git::GitError),
    #[error(transparent)]
    Codespace(#[from] flyco_daemon::codespace::CodespaceError),
    #[error(transparent)]
    Mount(#[from] flyco_daemon::mount::MountError),
    #[error(transparent)]
    Skills(#[from] skills::SkillError),
    /// Boxed: `HostError` is wide enough that carrying it inline would make
    /// every `Result` in this binary a hundred-plus bytes.
    #[error(transparent)]
    Host(Box<host::HostError>),
    #[error("could not write to stdout")]
    Stdout(#[source] std::io::Error),
    /// `flycod mcp` was pointed at a configuration with no control plane.
    #[error(
        "`flycod mcp` needs a [control_plane] in its config: every tool it serves \
         reads or changes this session in the control plane"
    )]
    NoControlPlane,
    /// The MCP client never completed the handshake.
    ///
    /// Boxed because the SDK's initialization error carries the whole
    /// handshake, which is several hundred bytes and would make every
    /// `Result` in this binary that size.
    #[error("the harness did not initialize flyco's MCP server")]
    Mcp(#[source] Box<rmcp::service::ServerInitializeError>),
    /// The MCP server stopped on an error rather than on a closed pipe.
    #[error("flyco's MCP server stopped")]
    McpStopped(#[source] tokio::task::JoinError),
    /// The daemon's singleton lock could not be taken or made.
    ///
    /// A failure here is fatal rather than a stand-down: contention is
    /// `Ok(None)`, and what reaches this variant is a filesystem that
    /// could not say so — running anyway would run unlocked.
    #[error("the daemon lock could not be taken")]
    Lock(#[source] std::io::Error),
}

impl From<host::HostError> for Failure {
    fn from(error: host::HostError) -> Self {
        Self::Host(Box::new(error))
    }
}

impl Failure {
    /// The wait the control plane named, when what stopped `flycod` was a
    /// refusal carrying `Retry-After`.
    fn retry_after(&self) -> Option<core::time::Duration> {
        match self {
            Self::Wire(wire) => wire.retry_after(),
            Self::Host(host) => host.retry_after(),
            Self::Skills(skills) => skills.retry_after(),
            _ => None,
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    // stdout carries the REPL's structured output, so every diagnostic goes
    // to stderr.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    match Box::pin(run(Cli::parse())).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            tracing::error!(error = %failure, "flycod stopped");
            let mut source = std::error::Error::source(&failure);
            while let Some(cause) = source {
                tracing::error!(%cause, "caused by");
                source = cause.source();
            }
            // A refusal that named a wait is honoured before the unit is
            // allowed to restart this process: `Restart=on-failure` with
            // five seconds between attempts would otherwise spend the
            // request budget the control plane just said is spent, five
            // seconds at a time, until UTC midnight (issue #342).
            if let Some(wait) = failure.retry_after() {
                tracing::warn!(
                    ?wait,
                    "the control plane asked this daemon to wait; sleeping before the unit restarts it"
                );
                tokio::time::sleep(wait).await;
            }
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), Failure> {
    match cli.command {
        Command::ExampleConfig => {
            let mut stdout = tokio::io::stdout();
            stdout
                .write_all(EXAMPLE.as_bytes())
                .await
                .map_err(Failure::Stdout)?;
            stdout.flush().await.map_err(Failure::Stdout)
        }
        Command::Codespace => Ok(flyco_daemon::codespace::bootstrap().await?),
        Command::Host { command } => run_host(command).await,
        Command::Mcp { config } => serve_mcp(DaemonConfig::load(&config)?).await,
        Command::Run { config: path } => {
            let mut config = DaemonConfig::load(&path)?;
            // One daemon per session per machine — every spawn path
            // included. `postStart` fires on each codespace start and a
            // unit restarts on failure, and two daemons for one session
            // ping-pong the room's attach epoch until the request budget
            // is gone (issue #336). The losing process stands down
            // cleanly rather than racing its own relay.
            let Some(_daemon_lock) =
                flyco_daemon::lock::acquire(&flyco_daemon::lock::session(&config))
                    .map_err(Failure::Lock)?
            else {
                tracing::warn!(
                    session = %config.session,
                    "another flycod already holds this session's lock; standing down"
                );
                return Ok(());
            };
            tracing::info!(
                session = %config.session,
                harness = ?config.harness,
                workdir = ?config.workdir,
                "starting flycod"
            );
            // The checkout is what the harness is started *in*, so it lands
            // before either driver runs rather than inside one of them: an
            // agent that came up in an empty directory would spend its first
            // turn discovering the repository is not there.
            let api = config.control_plane.as_ref().map(|control_plane| {
                HttpControlApi::new(
                    control_plane.url.clone(),
                    config.session,
                    control_plane.daemon_token.clone(),
                )
            });
            if let Some(api) = api.as_ref() {
                Box::pin(conversation_to_continue(&mut config, api)).await;
            }
            report_failure_to(
                api.as_ref(),
                Box::pin(check_out(&config, api.as_ref())).await,
            )
            .await?;
            // The owner's skills land before the harness is built: a
            // harness reads its global skills directory once, at launch,
            // and a running one must never watch it change mid-turn. A
            // developer machine has no control plane and installs nothing.
            if let Some(api) = api.as_ref() {
                report_failure_to(Some(api), install_skills(&config, api).await).await?;
            }
            // Every server this session may reach: flyco's own, launched as
            // a second `flycod mcp` against this same file, and the ones
            // the user registered. Built once here because both harnesses
            // are given the identical set.
            let mount = Mount::new(
                FlycoServer::of(&path)?,
                core::mem::take(&mut config.mcp_servers),
                config.computer.enabled,
            );
            let driven = match config.harness {
                DriverKind::ClaudeCode => Box::pin(drive_claude_code(config, mount)).await,
                DriverKind::Acp => Box::pin(drive_acp(config, mount)).await,
            };
            report_failure_to(api.as_ref(), driven).await
        }
    }
}

/// Tells the control plane why this daemon is stopping, and passes the
/// failure on.
///
/// `flycod` is restarted on failure, so a machine whose daemon cannot start
/// says nothing at all otherwise: the relay is never opened, and the page
/// waits on a timeline that will not advance. The report is best effort —
/// a control plane that cannot be reached is exactly the kind of failure
/// being reported, and shouting about it twice would replace the real
/// reason with a network error.
async fn report_failure_to<T>(
    api: Option<&HttpControlApi>,
    outcome: Result<T, Failure>,
) -> Result<T, Failure> {
    let Err(failure) = outcome else {
        return outcome;
    };
    if let Some(api) = api
        && let Err(error) = api.report_startup_failure(failure.to_string()).await
    {
        tracing::warn!(%error, "could not tell the control plane why flycod is stopping");
    }
    Err(failure)
}

/// Installs the session owner's skills into the harness's global skills
/// directory.
///
/// Called only with a control plane — the developer-machine REPL has no
/// registry to ask and installs nothing — and only for a harness that has
/// such a directory, which [`skills::target`] decides: Claude's config
/// tree gets the `Claude` scope, Codex's `CODEX_HOME` gets the `Codex`
/// one, and any other ACP agent gets nothing.
async fn install_skills(config: &DaemonConfig, api: &HttpControlApi) -> Result<(), Failure> {
    let Some(target) = skills::target(config) else {
        return Ok(());
    };
    let mounts = api.list_skills().await.map_err(skills::SkillError::from)?;
    skills::install(api, &mounts, &target).await?;
    Ok(())
}

/// Which harness conversation this daemon must continue.
///
/// The control plane is asked rather than the configuration file trusted,
/// because the file is not current and cannot be: it was written when the
/// machine was *created*, and the machine that recovers from a spot
/// reclamation is the same machine, booting the same disk and therefore the
/// same file. The control plane recorded the harness's identity the moment
/// the previous daemon announced it, so its answer is the conversation the
/// user is watching.
///
/// A control plane that cannot be reached leaves the configured value in
/// place. That is the honest fallback rather than a papered-over failure:
/// on a first boot it is `None` and a fresh session is right, and on a
/// rebuilt machine the provisioner wrote the id into the file itself.
async fn conversation_to_continue(config: &mut DaemonConfig, api: &HttpControlApi) {
    let view = match api.harness_session().await {
        Ok(view) => view,
        Err(error) => {
            tracing::warn!(
                %error,
                "could not read the harness conversation to continue; using the configured one"
            );
            return;
        }
    };

    // The model and the mode as well as the conversation, and for the same
    // reason: the file on this disk was written when the machine was
    // *created*, so a session whose model or mode the user changed while it
    // ran would come back on the old ones after a reclamation.
    tracing::info!(
        model = %view.model.model,
        effort = ?view.model.effort,
        mode = ?view.permission_mode,
        "running this session on the model and mode the control plane recorded"
    );
    if let Some(claude) = config.claude.as_mut() {
        claude.model = Some(view.model.model.clone());
        claude.effort.clone_from(&view.model.effort);
        claude.permission_mode = view.permission_mode;
    }
    if let Some(acp) = config.acp.as_mut() {
        acp.model = Some(view.model.model.clone());
        acp.effort.clone_from(&view.model.effort);
        acp.permission_mode = view.permission_mode;
    }

    match view.harness_session_id {
        Some(recorded) => {
            if config.resume_session_id.as_deref() != Some(recorded.as_str()) {
                tracing::info!(
                    session = %recorded,
                    "continuing the harness conversation the control plane recorded"
                );
            }
            config.resume_session_id = Some(recorded);
        }
        None => {
            tracing::info!("this session has no harness conversation yet; starting one");
        }
    }
}

/// Puts the session's repositories in the workdir, before anything is
/// started in it.
///
/// Three steps in this order, and the order is the feature:
///
/// 1. The [`Cloning`](ProvisioningStage::Cloning) stage is announced, so the
///    timeline says what the minute before the agent appears is being spent
///    on (docs/ux.md §9.2).
/// 2. Each `[[repos]]` entry is cloned at the branch the session names,
///    into `workdir/<dir>` — or adopted, when the disk already holds the
///    checkout: a machine booting a disk it already worked on keeps every
///    tree exactly as it was, uncommitted work included, and clones
///    nothing.
/// 3. Any work a previous machine stored for a checkout — committed or
///    not — is applied back on top of it. A session resuming onto a new
///    machine is a fresh clone plus that patch — which is why the patch is
///    applied *after* the clone and *before* the harness, rather than onto
///    whatever the last machine left. On a
///    [`Runtime::Container`](flyco_core::Runtime::Container) this is every
///    start, not only the ones that follow an archive: the filesystem went
///    with the last execution, so the clone is always fresh and the patch
///    is always where the work is.
///
/// A daemon with no `[[repos]]` is a developer machine pointed at a
/// checkout that already exists, and clones nothing.
async fn check_out(config: &DaemonConfig, api: Option<&HttpControlApi>) -> Result<(), Failure> {
    if config.repos.is_empty() {
        tracing::info!(
            workdir = %config.workdir.display(),
            "no [[repos]] in the config: working in the directory this daemon was pointed at"
        );
        return Ok(());
    }
    let Some(github) = &config.github else {
        // `validate_repos` refuses this shape at load; reaching it here is
        // a config written by a build that did not.
        tracing::warn!("[[repos]] configured without [github]; nothing can be cloned");
        return Ok(());
    };

    if let Some(api) = api {
        // A stage that does not reach the room costs the user a line of the
        // timeline. Failing the clone over it would cost them the session.
        if let Err(error) = api.report_stage(ProvisioningStage::Cloning).await {
            tracing::warn!(%error, "the cloning stage did not reach the session room");
        }
    }

    // Asked once, before the loop: a handoff's provenance describes the
    // session's first checkout — the local repository it was taken from —
    // and every other checkout's patch lookup is the ordinary kind.
    let handoff = if let Some(api) = api {
        api.get_handoff()
            .await
            .map_err(flyco_daemon::control::WireError::from)?
    } else {
        None
    };

    for (index, repo) in config.repos.iter().enumerate() {
        let checkout = repo.checkout_path(&config.workdir);
        if flyco_daemon::git::has_checkout(&checkout).await {
            // The machine is booting a disk it already worked on: this
            // checkout — with whatever the agent had not committed —
            // survived exactly as it was. Replaying a stored patch onto it
            // would apply the same work twice.
            tracing::info!(
                dir = %repo.dir,
                "the checkout is already on this disk; keeping it as it is"
            );
            continue;
        }
        flyco_daemon::git::clone_into(repo, github, &checkout).await?;
        if let Some(api) = api {
            apply_stored_patch(
                api,
                Some(&repo.dir),
                &checkout,
                if index == 0 { handoff.as_ref() } else { None },
            )
            .await?;
        }
    }
    if let Some(api) = api
        && let Some(view) = &handoff
        && view.has_transcript
    {
        materialize_transcript(api).await?;
    }
    Ok(())
}

/// Runs `flycod host`: this machine, rather than a session on one.
///
/// Enrolling and running are one command apart because they happen at
/// different times and with different credentials — the installer enrols
/// once with a token the user pasted, and the unit runs for as long as the
/// machine is flyco's to schedule onto.
async fn run_host(command: HostCommand) -> Result<(), Failure> {
    match command {
        HostCommand::Enroll {
            token,
            control_plane,
            volume_root,
            config,
        } => {
            host::enroll(host::Enrollment {
                token,
                control_plane,
                volume_root,
                config,
            })
            .await?;
            Ok(())
        }
        HostCommand::Run { config } => Ok(host::run(&config).await?),
    }
}

/// Serves flyco's tools to the harness until it closes the pipe.
///
/// Everything this answers comes from the control plane, so a configuration
/// without one has nothing to serve: on a developer machine the REPL is how
/// a session is driven, and there is no session in a control plane for the
/// tools to act on. Refusing here says so once, rather than answering every
/// tool call with the same failure.
async fn serve_mcp(config: DaemonConfig) -> Result<(), Failure> {
    let Some(control_plane) = config.control_plane.clone() else {
        return Err(Failure::NoControlPlane);
    };
    let api = HttpControlApi::new(
        control_plane.url,
        config.session,
        control_plane.daemon_token,
    );
    let tools = FlycoTools::new(
        api,
        flyco_daemon::mcp::RepoTrees::over(&config.workdir, &config.repos),
        config.machine_origin,
        // The desktop's agent socket exists only while the session's
        // flag is on, which is also the condition the `computer_*` tools
        // are listed under.
        config
            .computer
            .enabled
            .then(|| flyco_daemon::desktop::ipc::socket_path(config.session)),
    );

    tracing::info!(session = %config.session, "serving flyco's MCP tools over stdio");
    let service = tools
        .serve(stdio())
        .await
        .map_err(|error| Failure::Mcp(Box::new(error)))?;
    service.waiting().await.map_err(Failure::McpStopped)?;
    Ok(())
}

/// Drives a Claude Code session, reporting to a control plane if the
/// configuration names one and to the terminal otherwise.
///
/// The two paths differ in more than their output: a session that reports to
/// a control plane keeps its transcript there, which is what lets it resume
/// onto another machine. Which one a run took is logged, because "why is
/// nothing reaching the browser" has exactly one cheap answer.
async fn drive_claude_code(config: DaemonConfig, mount: Mount) -> Result<(), Failure> {
    let Some(control_plane) = config.control_plane.clone() else {
        tracing::info!(
            "no [control_plane] in the config: driving this session from stdin. \
             Transcripts stay in `transcript_dir` and no browser can reach the session."
        );
        let store = JsonlTranscriptStore::new(config.transcript_dir.clone());
        let started = start(&config, mount, store).await?;
        repl::run(started.session, started.outputs).await?;
        return Ok(());
    };

    tracing::info!(
        url = %control_plane.url,
        "reporting to the control plane over the session relay"
    );
    let ControlPlaneConfig { url, daemon_token } = control_plane;
    let api = HttpControlApi::new(url, config.session, daemon_token);

    let started = start(&config, mount, RemoteTranscriptStore::new(api.clone())).await?;
    let (terminal, terminal_out) =
        flyco_daemon::terminal::Terminal::spawn(&config.terminal.shell, &config.workdir)?;
    let (desktop, desktop_out) = flyco_daemon::desktop::spawn(&config.computer, config.session);
    Box::pin(wire::run(SessionRelay {
        session_id: config.session,
        deadlines: wire::Deadlines::default(),
        session: started.session,
        outputs: started.outputs,
        api,
        terminal,
        terminal_out,
        desktop,
        desktop_out,
        tui: flyco_daemon::tui::HarnessTui::resolve(&config).await,
        // The composer's `!` commands run in the same workspace the agent
        // works in, as the same user this daemon runs as.
        shell: config.shell.runner(config.workdir.clone()),
        repos: repos_of(&config).await,
        workspace: workspace_of(&config),
        github: config.github.clone(),
        disk: flyco_daemon::spot::HostDisk,
        // Watched from here rather than from inside the relay: which
        // endpoint carries a notice is a fact about the machine, and the
        // relay's business is the session on it.
        spot: flyco_daemon::spot::watch(config.spot_provider),
        // And on exactly the machines whose filesystem goes with them:
        // saving a session takes the whole grace period, and on a VM there
        // is nothing at risk to spend it on (see [`flyco_daemon::stop`]).
        stops: flyco_daemon::stop::watch(config.runtime),
        machine: config.machine.clone(),
        machine_origin: config.machine_origin,
    }))
    .await?;
    Ok(())
}

/// Drives an ACP session — Codex, Devin, or any other agent the `[acp]`
/// table names — reporting to a control plane if the configuration names
/// one and to the terminal otherwise.
async fn drive_acp(config: DaemonConfig, mount: Mount) -> Result<(), Failure> {
    let harness = AcpHarness::new(config.acp().clone(), mount);
    let started = harness
        .start(StartRequest {
            workdir: config.workdir.clone(),
            resume_session_id: config.resume_session_id.clone(),
        })
        .await?;
    Box::pin(report(config, started)).await
}

/// Hands a started session to the control plane or the REPL.
async fn report<S: HarnessSession + 'static>(
    config: DaemonConfig,
    started: Started<S>,
) -> Result<(), Failure> {
    let Some(control_plane) = config.control_plane.clone() else {
        tracing::info!(
            "no [control_plane] in the config: driving this session from stdin. \
             Transcripts stay in `transcript_dir` and no browser can reach the session."
        );
        repl::run(started.session, started.outputs).await?;
        return Ok(());
    };

    tracing::info!(
        url = %control_plane.url,
        "reporting to the control plane over the session relay"
    );
    let ControlPlaneConfig { url, daemon_token } = control_plane;
    let api = HttpControlApi::new(url, config.session, daemon_token);
    let (terminal, terminal_out) =
        flyco_daemon::terminal::Terminal::spawn(&config.terminal.shell, &config.workdir)?;
    let (desktop, desktop_out) = flyco_daemon::desktop::spawn(&config.computer, config.session);
    Box::pin(wire::run(SessionRelay {
        session_id: config.session,
        deadlines: wire::Deadlines::default(),
        session: started.session,
        outputs: started.outputs,
        api,
        terminal,
        terminal_out,
        desktop,
        desktop_out,
        tui: flyco_daemon::tui::HarnessTui::resolve(&config).await,
        // The composer's `!` commands run in the same workspace the agent
        // works in, as the same user this daemon runs as.
        shell: config.shell.runner(config.workdir.clone()),
        repos: repos_of(&config).await,
        workspace: workspace_of(&config),
        github: config.github.clone(),
        disk: flyco_daemon::spot::HostDisk,
        // Watched from here rather than from inside the relay: which
        // endpoint carries a notice is a fact about the machine, and the
        // relay's business is the session on it.
        spot: flyco_daemon::spot::watch(config.spot_provider),
        // And on exactly the machines whose filesystem goes with them:
        // saving a session takes the whole grace period, and on a VM there
        // is nothing at risk to spend it on (see [`flyco_daemon::stop`]).
        stops: flyco_daemon::stop::watch(config.runtime),
        machine: config.machine.clone(),
        machine_origin: config.machine_origin,
    }))
    .await?;
    Ok(())
}

/// The read-only view of the workspace the `Files` and `Diff` tabs read.
///
/// The base a checkout's diff is taken against is the *remote-tracking*
/// ref of the branch the session opened it on, not the local branch: the
/// agent commits onto the local one, and a diff against it would go empty
/// the moment the agent committed — which is precisely when the user wants
/// to see what it did. A daemon with no `[[repos]]` was pointed at a
/// directory rather than given clones, so it has no branches the session
/// began at and says so rather than inventing one.
fn workspace_of(config: &DaemonConfig) -> flyco_daemon::workdir::Workspace {
    if config.repos.is_empty() {
        flyco_daemon::workdir::Workspace::new(config.workdir.clone(), None)
    } else {
        flyco_daemon::workdir::Workspace::provisioned(
            config.workdir.clone(),
            config
                .repos
                .iter()
                .map(|repo| (repo.dir.clone(), repo.branch.to_string())),
        )
    }
}

/// The session's checkouts as the relay's working set.
///
/// One watched `GitWorkdir` per `[[repos]]` directory under the workspace — or,
/// on a developer machine whose root is itself the checkout, the root
/// under the `None` key the wire uses for it. A developer machine pointed
/// at a directory that is not a checkout watches nothing: `git status`
/// would only say so forever.
async fn repos_of(config: &DaemonConfig) -> flyco_daemon::git::GitRepos {
    let mut repos = flyco_daemon::git::GitRepos::new(config.workdir.clone());
    if config.repos.is_empty() {
        if flyco_daemon::git::has_checkout(&config.workdir).await {
            repos.watch(None, config.workdir.clone());
        }
    } else {
        for repo in &config.repos {
            repos.watch(Some(repo.dir.clone()), repo.checkout_path(&config.workdir));
        }
    }
    repos
}

/// Replays the work a previous machine stored for one checkout, if any.
///
/// Two things store a patch, and from this side they are the same fact —
/// this checkout is fresh and the session's work is not in it. An
/// automatic archive writes a patch before it releases the disk, and so
/// does every stop of a [`Runtime::Container`](flyco_core::Runtime::Container)
/// session, whose filesystem goes with its execution
/// ([`flyco_daemon::stop`]). A `flyco handoff` uploads one for the local
/// tree it is moving here — always the session's first checkout, which is
/// what `handoff` is `Some` for. `dir` is the checkout's workspace
/// directory — the key the patch was stored under — and `workdir` where it
/// lives on this machine.
///
/// A patch is not necessarily diffed against the tip this clone landed
/// on, so the checkout is rewound first: a handoff names its sender's
/// merge-base in the manifest, and a snapshot the daemon itself stored
/// carries its base as the object's first line.
///
/// A patch that will not apply is **fatal**, and the git error travels with
/// it into the startup failure the control plane records. The alternative
/// is an agent that comes up on a clean tree and carries on, which is the
/// user's work silently discarded and a session that looks fine until
/// they read the diff.
async fn apply_stored_patch(
    api: &HttpControlApi,
    dir: Option<&str>,
    workdir: &std::path::Path,
    handoff: Option<&flyco_core::HandoffView>,
) -> Result<(), flyco_daemon::control::WireError> {
    use flyco_daemon::git::WorkingTree as _;

    let Some(stored) = api.get_workdir_patch(dir).await? else {
        tracing::info!("no stored patch: this session's work is all in the clone");
        return Ok(());
    };
    // A handoff patch is verified before it is applied: the manifest's
    // checksum is what `complete` proved against the uploaded object, so a
    // corrupted or swapped one is a clear startup failure rather than a
    // `git apply` syntax error.
    if let Some(view) = handoff {
        use sha2::Digest as _;
        let actual = hex::encode(sha2::Sha256::digest(&stored));
        if actual != view.patch_sha256 {
            return Err(flyco_daemon::control::WireError::Handoff(format!(
                "the stored patch hashes to {actual}, not the {} its manifest recorded",
                view.patch_sha256
            )));
        }
    }
    let (envelope_base, patch) = flyco_daemon::git::decode_snapshot(&stored);
    // A handoff's base is the manifest's provenance; a daemon snapshot's
    // is the envelope's. A bare patch stored before bases existed applies
    // onto the clone tip, exactly as it always did.
    let rewind = handoff
        .map(|view| view.base_commit.as_str())
        .or(envelope_base);
    if let Some(commit) = rewind {
        flyco_daemon::git::reset_to(workdir, commit).await?;
    }
    let bytes = patch.len();
    flyco_daemon::git::GitWorkdir::new(workdir.to_path_buf())
        .apply(patch)
        .await?;
    tracing::info!(
        bytes,
        workdir = %workdir.display(),
        "replayed the work the previous machine stored"
    );
    Ok(())
}

/// Writes a handoff's uploaded transcript where the handoff prompt tells
/// the agent it is: outside the workdir, at the fixed path the prompt and
/// [`flyco_core::HANDOFF_TRANSCRIPT_PATH`] agree on.
async fn materialize_transcript(
    api: &HttpControlApi,
) -> Result<(), flyco_daemon::control::WireError> {
    use flyco_daemon::control::WireError;

    let Some(transcript) = api.get_handoff_transcript().await? else {
        return Err(WireError::Handoff(
            "the manifest announces a transcript that is not stored".to_owned(),
        ));
    };
    let path = std::path::Path::new(flyco_core::HANDOFF_TRANSCRIPT_PATH);
    if let Some(directory) = path.parent() {
        tokio::fs::create_dir_all(directory)
            .await
            .map_err(|error| WireError::Handoff(error.to_string()))?;
    }
    tokio::fs::write(path, &transcript)
        .await
        .map_err(|error| WireError::Handoff(error.to_string()))?;
    tracing::info!(
        bytes = transcript.len(),
        path = %path.display(),
        "landed the handed-off transcript"
    );
    Ok(())
}

/// Launches the Claude Code harness against a transcript store.
async fn start<S: TranscriptStore>(
    config: &DaemonConfig,
    mount: Mount,
    store: S,
) -> Result<Started<impl flyco_daemon::harness::HarnessSession + use<S>>, Failure> {
    let harness = ClaudeCodeHarness::new(
        config.claude().clone(),
        config.sidecar().clone(),
        mount,
        store,
    );
    Ok(harness
        .start(StartRequest {
            workdir: config.workdir.clone(),
            resume_session_id: config.resume_session_id.clone(),
        })
        .await?)
}
