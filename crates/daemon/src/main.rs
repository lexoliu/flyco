//! The `flycod` binary.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use flyco_core::HarnessKind;
use flyco_daemon::config::{ControlPlaneConfig, DaemonConfig, EXAMPLE};
use flyco_daemon::control::{Endpoint, HttpControlApi, RemoteTranscriptStore, wire};
use flyco_daemon::harness::claude::ClaudeCodeHarness;
use flyco_daemon::harness::claude::store::{JsonlTranscriptStore, TranscriptStore};
use flyco_daemon::harness::codex::CodexHarness;
use flyco_daemon::harness::{Harness as _, HarnessSession, StartRequest, Started};
use flyco_daemon::repl;
use tokio::io::AsyncWriteExt as _;
use tracing_subscriber::EnvFilter;

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
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
    },
    /// Print a complete, valid configuration to stdout.
    ExampleConfig,
}

/// Anything that stops `flycod` before it finishes.
#[derive(Debug, thiserror::Error)]
enum Failure {
    #[error(transparent)]
    Config(#[from] flyco_daemon::config::ConfigError),
    #[error(transparent)]
    Claude(#[from] flyco_daemon::harness::claude::ClaudeError),
    #[error(transparent)]
    Codex(#[from] flyco_daemon::harness::codex::CodexError),
    #[error(transparent)]
    Repl(#[from] repl::ReplError),
    #[error(transparent)]
    Wire(#[from] flyco_daemon::control::WireError),
    #[error("could not write to stdout")]
    Stdout(#[source] std::io::Error),
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

    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            tracing::error!(error = %failure, "flycod stopped");
            let mut source = std::error::Error::source(&failure);
            while let Some(cause) = source {
                tracing::error!(%cause, "caused by");
                source = cause.source();
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
        Command::Run { config } => {
            let config = DaemonConfig::load(&config)?;
            tracing::info!(
                session = %config.session,
                harness = ?config.harness,
                workdir = ?config.workdir,
                "starting flycod"
            );
            match config.harness {
                HarnessKind::ClaudeCode => drive_claude_code(config).await,
                HarnessKind::Codex => drive_codex(config).await,
            }
        }
    }
}

/// Drives a Claude Code session, reporting to a control plane if the
/// configuration names one and to the terminal otherwise.
///
/// The two paths differ in more than their output: a session that reports to
/// a control plane keeps its transcript there, which is what lets it resume
/// onto another machine. Which one a run took is logged, because "why is
/// nothing reaching the browser" has exactly one cheap answer.
async fn drive_claude_code(config: DaemonConfig) -> Result<(), Failure> {
    let Some(control_plane) = config.control_plane.clone() else {
        tracing::info!(
            "no [control_plane] in the config: driving this session from stdin. \
             Transcripts stay in `transcript_dir` and no browser can reach the session."
        );
        let store = JsonlTranscriptStore::new(config.transcript_dir.clone());
        let started = start(&config, store).await?;
        repl::run(started.session, started.outputs).await?;
        return Ok(());
    };

    tracing::info!(
        url = %control_plane.url,
        "reporting to the control plane over the session relay"
    );
    let ControlPlaneConfig { url, daemon_token } = control_plane;
    let api = HttpControlApi::new(url.clone(), config.session, daemon_token.clone());
    let endpoint = Endpoint::from_base(&url, config.session, daemon_token)?;

    let started = start(&config, RemoteTranscriptStore::new(api.clone())).await?;
    wire::run(endpoint, started.session, started.outputs, api).await?;
    Ok(())
}

/// Drives a Codex session, reporting to a control plane if the
/// configuration names one and to the terminal otherwise.
async fn drive_codex(config: DaemonConfig) -> Result<(), Failure> {
    let harness = CodexHarness::new(config.codex().clone());
    let started = harness
        .start(StartRequest {
            workdir: config.workdir.clone(),
            resume_session_id: config.resume_session_id.clone(),
        })
        .await?;
    report(config, started).await
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
    let api = HttpControlApi::new(url.clone(), config.session, daemon_token.clone());
    let endpoint = Endpoint::from_base(&url, config.session, daemon_token)?;
    wire::run(endpoint, started.session, started.outputs, api).await?;
    Ok(())
}

/// Launches the Claude Code harness against a transcript store.
async fn start<S: TranscriptStore>(
    config: &DaemonConfig,
    store: S,
) -> Result<Started<impl flyco_daemon::harness::HarnessSession + use<S>>, Failure> {
    let harness = ClaudeCodeHarness::new(config.claude().clone(), config.sidecar().clone(), store);
    Ok(harness
        .start(StartRequest {
            workdir: config.workdir.clone(),
            resume_session_id: config.resume_session_id.clone(),
        })
        .await?)
}
