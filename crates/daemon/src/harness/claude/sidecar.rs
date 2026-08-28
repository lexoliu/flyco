//! Materializing and launching the Bun sidecar.
//!
//! The sidecar's sources are embedded in the `flycod` binary and written to
//! a working directory at startup, so a session VM needs Bun and nothing
//! else — no npm registry layout, no separately shipped script directory.
//!
//! `rust-embed` is used rather than `include_dir!` for one reason: the same
//! directory that holds the sources also holds the `node_modules/` tree
//! that `bun install` creates in-tree, and `include_dir!` has no way to
//! exclude it. Embedding it would put an entire dependency tree inside the
//! binary. [`SidecarAssets`] excludes it, and a test asserts that it does.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use rust_embed::Embed;
use tokio::process::{Child, Command};

/// The sidecar's TypeScript sources, lockfile, and manifest.
///
/// Two exclusions, both load-bearing: `node_modules/` is created in-tree by
/// `bun install` and would put a whole dependency tree in the binary, and
/// the test files reference `crates/daemon/fixtures/`, which does not exist
/// beside a materialized sidecar. What ships is exactly what runs.
#[derive(Debug, Embed)]
#[folder = "sidecar/"]
#[exclude = "node_modules/**"]
#[exclude = "*.test.ts"]
struct SidecarAssets;

/// The sidecar could not be prepared or launched.
#[derive(Debug, thiserror::Error)]
pub enum SidecarError {
    /// Writing the embedded sources to the working directory failed.
    #[error("could not materialize the sidecar at {path}")]
    Materialize {
        /// The path being written.
        path: PathBuf,
        /// The underlying cause.
        #[source]
        source: std::io::Error,
    },
    /// The `bun` executable is not where the config says it is.
    #[error(
        "bun was not found at {executable} — flycod drives Claude Code through a Bun sidecar and \
         cannot run without it"
    )]
    BunMissing {
        /// The executable that could not be spawned.
        executable: PathBuf,
    },
    /// Spawning a Bun process failed for some other reason.
    #[error("could not run {executable}")]
    Spawn {
        /// The executable being run.
        executable: PathBuf,
        /// The underlying cause.
        #[source]
        source: std::io::Error,
    },
    /// `bun install` returned a non-zero status.
    #[error("`bun install --frozen-lockfile` failed in {directory} with {status}")]
    Install {
        /// The sidecar working directory.
        directory: PathBuf,
        /// How Bun exited.
        status: std::process::ExitStatus,
    },
}

/// Where flycod keeps the materialized sidecar and how it runs Bun.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SidecarConfig {
    /// Directory the embedded sources are written to. Created if missing.
    pub dir: PathBuf,
    /// The `bun` executable. A bare name is resolved through `PATH`.
    #[serde(default = "default_bun")]
    pub bun: PathBuf,
}

fn default_bun() -> PathBuf {
    PathBuf::from("bun")
}

/// Writes the embedded sources into `config.dir` and installs dependencies
/// if they are missing.
///
/// Idempotent: files whose bytes already match are left alone, and
/// `bun install` runs only when `node_modules` is absent.
///
/// # Errors
///
/// Returns [`SidecarError`] if the directory cannot be written, Bun is
/// missing, or the install fails.
pub async fn prepare(config: &SidecarConfig) -> Result<(), SidecarError> {
    for name in SidecarAssets::iter() {
        let asset = SidecarAssets::get(&name).ok_or_else(|| SidecarError::Materialize {
            path: config.dir.join(name.as_ref()),
            source: std::io::Error::new(
                ErrorKind::NotFound,
                "an embedded asset vanished between iteration and lookup",
            ),
        })?;
        write_if_changed(&config.dir.join(name.as_ref()), &asset.data).await?;
    }

    if tokio::fs::try_exists(config.dir.join("node_modules"))
        .await
        .map_err(|source| SidecarError::Materialize {
            path: config.dir.join("node_modules"),
            source,
        })?
    {
        tracing::debug!(dir = ?config.dir, "sidecar dependencies are already installed");
        return Ok(());
    }

    tracing::info!(dir = ?config.dir, "installing sidecar dependencies with bun");
    // Captured rather than inherited: flycod's stdout carries structured
    // output, and bun's install progress would land in the middle of it.
    let installed = Command::new(&config.bun)
        .arg("install")
        .arg("--frozen-lockfile")
        .current_dir(&config.dir)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|source| classify_spawn(&config.bun, source))?;
    for (stream, bytes) in [("stdout", &installed.stdout), ("stderr", &installed.stderr)] {
        let text = String::from_utf8_lossy(bytes);
        let text = text.trim();
        if !text.is_empty() {
            tracing::debug!(target: "flycod::bun", stream, "{text}");
        }
    }
    if !installed.status.success() {
        return Err(SidecarError::Install {
            directory: config.dir.clone(),
            status: installed.status,
        });
    }
    Ok(())
}

/// Spawns the sidecar with its stdio piped.
///
/// # Errors
///
/// Returns [`SidecarError`] if Bun cannot be started.
pub fn spawn(config: &SidecarConfig) -> Result<Child, SidecarError> {
    tracing::info!(dir = ?config.dir, "starting the Claude Code sidecar");
    Command::new(&config.bun)
        .arg("run")
        .arg("sidecar.ts")
        .current_dir(&config.dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| classify_spawn(&config.bun, source))
}

fn classify_spawn(executable: &Path, source: std::io::Error) -> SidecarError {
    if source.kind() == ErrorKind::NotFound {
        SidecarError::BunMissing {
            executable: executable.to_owned(),
        }
    } else {
        SidecarError::Spawn {
            executable: executable.to_owned(),
            source,
        }
    }
}

async fn write_if_changed(path: &Path, contents: &[u8]) -> Result<(), SidecarError> {
    if let Ok(existing) = tokio::fs::read(path).await
        && existing == contents
    {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|source| SidecarError::Materialize {
                path: parent.to_owned(),
                source,
            })?;
    }
    tokio::fs::write(path, contents)
        .await
        .map_err(|source| SidecarError::Materialize {
            path: path.to_owned(),
            source,
        })?;
    tracing::info!(?path, "wrote sidecar source");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::SidecarAssets;

    #[test]
    fn only_what_runs_is_embedded() {
        for name in SidecarAssets::iter() {
            assert!(
                !name.contains("node_modules"),
                "{name} would put an installed dependency tree inside flycod"
            );
            assert!(
                !name.ends_with(".test.ts"),
                "{name} needs fixtures that do not exist beside a materialized sidecar"
            );
        }
    }

    #[test]
    fn the_sidecar_ships_everything_bun_needs_to_run_it() {
        let embedded: Vec<String> = SidecarAssets::iter().map(|name| name.to_string()).collect();
        for required in [
            "package.json",
            "bun.lock",
            "tsconfig.json",
            "protocol.ts",
            "sidecar.ts",
        ] {
            assert!(
                embedded.iter().any(|name| name == required),
                "{required} is missing from the embedded sidecar: {embedded:?}"
            );
        }
    }
}
