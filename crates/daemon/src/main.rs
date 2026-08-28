//! The `flycod` binary.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use flyco_core::HarnessKind;
use flyco_daemon::config::{DaemonConfig, EXAMPLE};
use flyco_daemon::harness::claude::ClaudeCodeHarness;
use flyco_daemon::harness::claude::store::JsonlTranscriptStore;
use flyco_daemon::harness::{Harness as _, StartRequest, Started};
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
    Repl(#[from] repl::ReplError),
    #[error("could not write to stdout")]
    Stdout(#[source] std::io::Error),
    /// The Codex driver is a later milestone; refusing beats pretending.
    #[error(
        "this build of flycod drives Claude Code only — the Codex app-server driver is not \
         implemented yet"
    )]
    UnsupportedHarness,
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
                HarnessKind::Codex => Err(Failure::UnsupportedHarness),
            }
        }
    }
}

async fn drive_claude_code(config: DaemonConfig) -> Result<(), Failure> {
    let harness = ClaudeCodeHarness::new(
        config.claude,
        config.sidecar,
        JsonlTranscriptStore::new(config.transcript_dir),
    );
    let Started { session, outputs } = harness
        .start(StartRequest {
            workdir: config.workdir,
            resume_session_id: config.resume_session_id,
        })
        .await?;
    repl::run(session, outputs).await?;
    Ok(())
}
