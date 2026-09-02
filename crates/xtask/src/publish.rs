//! `cargo xtask publish-flycod`: the one path that puts `flycod` in R2.
//!
//! Cross-compiles the daemon for both Linux architectures against a pinned
//! glibc, writes the checksum files `crates/api/src/releases.rs` serves,
//! uploads all four objects to the channel's prefix, and reads back what the
//! control plane actually serves. It refuses to publish a daemon speaking a
//! wire protocol the deployed Worker does not, because that combination
//! refuses every machine provisioned from it at `Hello`.

use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail, ensure};
use askama::Template as _;
use cargo_metadata::MetadataCommand;
use clap::Parser;
use serde::Deserialize;
use tokio::fs;
use tracing::info;
use zenwave::ResponseExt as _;

use crate::release::{
    ARCHITECTURE_COUNT, ARCHITECTURES, ArchitectureArtifacts, Channel, Invocation, Release,
    WireCheck,
};

/// An external program the publish runs, and how to install it.
#[derive(Debug, Clone, Copy)]
struct RequiredTool {
    /// Executable looked up on `PATH`.
    program: &'static str,
    /// The command that installs it, named in the failure.
    install: &'static str,
}

/// What cross-compiling needs. `cargo-zigbuild` drives the build and `zig`
/// is the C toolchain it links with; either one missing is a hard stop
/// rather than a fallback to the host toolchain, which cannot produce a
/// glibc-pinned Linux binary at all.
const BUILD_TOOLS: [RequiredTool; 2] = [
    RequiredTool {
        program: "cargo-zigbuild",
        install: "cargo install cargo-zigbuild",
    },
    RequiredTool {
        program: "zig",
        install: "brew install zig (or https://ziglang.org/download/)",
    },
];

/// What uploading needs.
const UPLOAD_TOOLS: [RequiredTool; 1] = [RequiredTool {
    program: "wrangler",
    install: "npm install --global wrangler",
}];

/// Builds and publishes the `flycod` release artifacts.
#[derive(Debug, Parser)]
pub struct PublishFlycod {
    /// Release channel to publish to: the `releases/<channel>/` prefix in the
    /// bucket, and the deployment whose wire protocol version must match.
    #[arg(long, default_value = "dev")]
    channel: Channel,
    /// Build and checksum the artifacts without uploading anything.
    #[arg(long)]
    dry_run: bool,
    /// Publish even though the channel's control plane speaks a different
    /// wire protocol version.
    #[arg(long)]
    allow_wire_mismatch: bool,
}

/// The health document `/v1/healthz` answers with.
#[derive(Debug, Deserialize)]
struct Health {
    /// Wire protocol version that deployment speaks to daemons.
    wire_protocol_version: u32,
}

/// The workspace paths the publish works in.
#[derive(Debug, Clone)]
struct Workspace {
    /// Directory the build is run from.
    root: PathBuf,
    /// Directory cargo writes build output to.
    target: PathBuf,
}

impl PublishFlycod {
    /// Runs the publish.
    ///
    /// # Errors
    ///
    /// Returns an error when a required tool is missing, the wire protocol
    /// versions disagree, the cross-compile fails, an upload fails, or the
    /// published checksum does not match what was built.
    pub async fn run(self) -> Result<()> {
        let publishing = flyco_core::WIRE_PROTOCOL_VERSION;
        info!(
            channel = %self.channel,
            wire_protocol_version = publishing,
            dry_run = self.dry_run,
            "publishing flycod"
        );

        require(&BUILD_TOOLS)?;
        if !self.dry_run {
            require(&UPLOAD_TOOLS)?;
        }

        let deployed = deployed_wire_version(self.channel).await?;
        info!(
            origin = self.channel.origin,
            wire_protocol_version = deployed,
            "control plane"
        );
        WireCheck {
            channel: self.channel,
            publishing,
            deployed,
            allowed_mismatch: self.allow_wire_mismatch,
        }
        .approve()?;

        let workspace = workspace().await?;
        run_to_completion(&Invocation::zigbuild(), &workspace.root).await?;

        let release = stage(self.channel, &workspace.target).await?;
        for artifact in &release.artifacts {
            info!(
                object = artifact.binary.name,
                digest = artifact.line.digest,
                file = %artifact.binary.file.display(),
                "built"
            );
        }

        if self.dry_run {
            info!(
                staged = %release.artifacts[0].binary.file.display(),
                "dry run: four objects staged, nothing uploaded"
            );
            return Ok(());
        }

        for object in release.objects() {
            let upload = Invocation::upload(release.channel, object);
            run_to_completion(&upload, &workspace.root).await?;
            info!(key = release.channel.key(&object.name), "uploaded");
        }

        verify(&release).await
    }
}

/// Fails unless every tool is on `PATH`, naming what installs the missing one.
fn require(tools: &[RequiredTool]) -> Result<()> {
    for tool in tools {
        if which::which(tool.program).is_err() {
            bail!(
                "{} is not on PATH; install it with: {}",
                tool.program,
                tool.install
            );
        }
    }
    Ok(())
}

/// Reads the wire protocol version the channel's control plane speaks.
async fn deployed_wire_version(channel: Channel) -> Result<u32> {
    let url = channel.health_url();
    let response = zenwave::get(url.as_str())
        .await
        .with_context(|| format!("GET {url}"))?;
    let health: Health = response
        .into_json()
        .await
        .with_context(|| format!("{url} did not answer a health document"))?;
    Ok(health.wire_protocol_version)
}

/// Locates the workspace, as cargo itself reports it.
async fn workspace() -> Result<Workspace> {
    let metadata = tokio::task::spawn_blocking(|| MetadataCommand::new().no_deps().exec())
        .await
        .context("reading cargo metadata")?
        .context("reading cargo metadata")?;
    Ok(Workspace {
        root: metadata.workspace_root.into_std_path_buf(),
        target: metadata.target_directory.into_std_path_buf(),
    })
}

/// Runs one invocation in `directory`, inheriting its output.
async fn run_to_completion(invocation: &Invocation, directory: &Path) -> Result<()> {
    info!(command = %invocation, "running");
    let mut command = invocation.command();
    command.current_dir(directory);
    let status = tokio::process::Command::from(command)
        .status()
        .await
        .with_context(|| format!("spawning {}", invocation.program))?;
    ensure!(status.success(), "`{invocation}` failed: {status}");
    Ok(())
}

/// Copies both freshly built binaries under their published names and writes
/// the checksum file that attests each one.
async fn stage(channel: Channel, target: &Path) -> Result<Release> {
    let stage_dir = target.join("flycod-release").join(channel.name);
    fs::create_dir_all(&stage_dir)
        .await
        .with_context(|| format!("creating {}", stage_dir.display()))?;

    let mut sources = Vec::with_capacity(ARCHITECTURE_COUNT);
    for architecture in ARCHITECTURES {
        let built = architecture.built_binary(target);
        sources.push(
            fs::read(&built)
                .await
                .with_context(|| format!("reading {}", built.display()))?,
        );
    }

    let mut sources = sources.into_iter();
    let staged = ARCHITECTURES.map(|architecture| {
        let bytes = sources
            .next()
            .expect("one built binary per published architecture");
        (
            ArchitectureArtifacts::stage(architecture, &stage_dir, &bytes),
            bytes,
        )
    });

    for (artifact, bytes) in &staged {
        fs::write(&artifact.binary.file, bytes)
            .await
            .with_context(|| format!("writing {}", artifact.binary.file.display()))?;
        fs::write(&artifact.checksum.file, artifact.line.render()?)
            .await
            .with_context(|| format!("writing {}", artifact.checksum.file.display()))?;
    }

    Ok(Release {
        channel,
        artifacts: staged.map(|(artifact, _)| artifact),
    })
}

/// Reads back what the control plane serves and fails on any difference.
///
/// This is the only check that covers the whole path — build, checksum,
/// upload, bucket, Worker allowlist — so a publish is not finished until it
/// passes.
async fn verify(release: &Release) -> Result<()> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("reading the clock")?
        .as_nanos();

    for artifact in &release.artifacts {
        let url = release
            .channel
            .checksum_verification_url(&artifact.binary.name, nonce);
        let served = zenwave::get(url.as_str())
            .await
            .with_context(|| format!("GET {url}"))?
            .into_string()
            .await
            .with_context(|| format!("{url} did not answer text"))?;
        let expected = artifact.line.render()?;
        ensure!(
            served.as_str() == expected,
            "{url} serves {served:?}, this publish built {expected:?}"
        );
        info!(object = artifact.checksum.name, "verified as served");
    }
    Ok(())
}
