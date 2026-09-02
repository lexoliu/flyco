//! Flyco's workspace automation, in the cargo-xtask shape: one ordinary
//! binary in the workspace, reached through the `xtask` alias in
//! `.cargo/config.toml`, so a release procedure is Rust the compiler checks
//! rather than a shell script nobody tests.
//!
//! ```text
//! cargo xtask publish-flycod [--channel dev] [--dry-run] [--allow-wire-mismatch]
//! ```

mod publish;
mod release;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

/// Flyco workspace automation.
#[derive(Debug, Parser)]
#[command(name = "xtask", about = "Flyco workspace automation tasks")]
struct Cli {
    /// The task to run.
    #[command(subcommand)]
    command: Command,
}

/// Every task `cargo xtask` runs.
#[derive(Debug, Subcommand)]
enum Command {
    /// Cross-compile flycod for Linux and publish it to the release bucket.
    PublishFlycod(publish::PublishFlycod),
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .without_time()
        .with_target(false)
        .init();

    match Cli::parse().command {
        Command::PublishFlycod(task) => task.run().await,
    }
}
