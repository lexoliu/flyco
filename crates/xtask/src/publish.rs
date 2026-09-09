//! `cargo xtask publish-flycod`: the one path that puts `flycod` in R2.
//!
//! Cross-compiles the daemon for both Linux architectures against a pinned
//! glibc, writes the checksum files `crates/api/src/releases.rs` serves,
//! stages the installer and both systemd units out of `crates/xtask/install/`,
//! uploads every object to the channel's prefix, and reads back what the
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
use flyco_core::release::{ASSETS, BINARY_COUNT};
use serde::Deserialize;
use tokio::fs;
use tracing::info;
use zenwave::{ResponseExt as _, header::CONTENT_TYPE};

use crate::release::{
    ARCHITECTURES, ArchitectureArtifacts, CONTAINERFILE, Channel, ENTRYPOINT, IMAGE_DIR,
    Invocation, Release, ReleaseObject, WireCheck, asset_source,
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

/// What building the session image needs: a Docker CLI with buildx, which
/// every current Docker Desktop, `OrbStack` and the Ubuntu runner carry.
const IMAGE_TOOLS: [RequiredTool; 1] = [RequiredTool {
    program: "docker",
    install: "https://docs.docker.com/get-docker/ (OrbStack on a Mac)",
}];

/// What uploading needs.
const UPLOAD_TOOLS: [RequiredTool; 1] = [RequiredTool {
    program: "wrangler",
    install: "npm install --global wrangler",
}];

/// How the registry is logged into when a push is refused: the same GitHub
/// account the repository lives under, with the one scope a package push
/// needs.
const REGISTRY_LOGIN: &str = "gh auth refresh --hostname github.com --scopes write:packages && gh auth token | docker login ghcr.io --username lexoliu --password-stdin";

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
    /// Directory the build is run from, and the one the installer sources are
    /// read out of.
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
    /// versions disagree, the cross-compile fails, an installer source is
    /// missing, an upload fails, or what the control plane serves afterwards
    /// is not what was staged.
    pub async fn run(self) -> Result<()> {
        let publishing = flyco_core::WIRE_PROTOCOL_VERSION;
        info!(
            channel = %self.channel,
            wire_protocol_version = publishing,
            dry_run = self.dry_run,
            "publishing flycod"
        );

        require(&BUILD_TOOLS)?;
        require(&IMAGE_TOOLS)?;
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

        let release = stage(self.channel, &workspace).await?;
        for artifact in &release.artifacts {
            info!(
                object = artifact.binary.name(),
                digest = artifact.line.digest,
                file = %artifact.binary.file.display(),
                "built"
            );
        }
        for asset in &release.assets {
            info!(
                object = asset.name(),
                file = %asset.file.display(),
                "copied from the repository"
            );
        }

        // The image before the bucket: a push the registry refuses (no
        // login, no `write:packages`) stops the publish before a single
        // object has changed under the machines already installing from it.
        let stage_dir = self.channel.stage_dir(&workspace.target);
        let tags = self.channel.image_tags(publishing);
        let build = Invocation::image_build(self.channel, &stage_dir, publishing, !self.dry_run);
        run_to_completion(&build, &workspace.root)
            .await
            .with_context(|| {
                if self.dry_run {
                    "building the session image".to_owned()
                } else {
                    format!("building and pushing the session image; if the registry refused the push, log in with: {REGISTRY_LOGIN}")
                }
            })?;
        if self.dry_run {
            run_to_completion(&Invocation::image_smoke(&tags[0]), &workspace.root).await?;
            info!(
                objects = release.objects().count(),
                image = %tags[0],
                directory = %stage_dir.display(),
                "dry run: staged and the image built, nothing uploaded"
            );
            return Ok(());
        }
        for tag in &tags {
            run_to_completion(&Invocation::image_inspect(tag), &workspace.root).await?;
            info!(image = %tag, "pushed and read back");
        }

        for object in release.objects() {
            let upload = Invocation::upload(release.channel, object);
            run_to_completion(&upload, &workspace.root).await?;
            info!(key = release.channel.key(object.name()), "uploaded");
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

/// Puts every object in the staging directory under its published name: both
/// freshly built binaries, the checksum file attesting each one, and the
/// installer and both units copied verbatim out of the repository.
///
/// Everything is uploaded from here, so a `--dry-run` leaves behind exactly
/// the bytes a real publish would have sent.
async fn stage(channel: Channel, workspace: &Workspace) -> Result<Release> {
    let stage_dir = channel.stage_dir(&workspace.target);
    fs::create_dir_all(&stage_dir)
        .await
        .with_context(|| format!("creating {}", stage_dir.display()))?;

    let mut sources = Vec::with_capacity(BINARY_COUNT);
    for architecture in ARCHITECTURES {
        let built = architecture.built_binary(&workspace.target);
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

    let mut assets = Vec::with_capacity(ASSETS.len());
    for asset in ASSETS {
        let staged = ReleaseObject::staged(asset, &stage_dir);
        let source = asset_source(&workspace.root, asset);
        let bytes = fs::read(&source)
            .await
            .with_context(|| format!("reading {}", source.display()))?;
        fs::write(&staged.file, &bytes)
            .await
            .with_context(|| format!("writing {}", staged.file.display()))?;
        assets.push(staged);
    }
    let assets = assets
        .try_into()
        .expect("one staged object per installer asset");

    // The image's own two sources go beside the objects, since the staging
    // directory is the image's build context and the installer it runs is
    // the one just staged above.
    let image_dir = IMAGE_DIR
        .iter()
        .fold(workspace.root.clone(), |path, segment| path.join(segment));
    for source in [CONTAINERFILE, ENTRYPOINT] {
        let from = image_dir.join(source);
        let bytes = fs::read(&from)
            .await
            .with_context(|| format!("reading {}", from.display()))?;
        fs::write(stage_dir.join(source), &bytes)
            .await
            .with_context(|| format!("writing {}", stage_dir.join(source).display()))?;
    }

    Ok(Release {
        channel,
        artifacts: staged.map(|(artifact, _)| artifact),
        assets,
    })
}

/// Reads back what the control plane serves and fails on any difference.
///
/// This is the only check that covers the whole path — build, checksum,
/// upload, bucket, Worker allowlist, media type — so a publish is not
/// finished until it passes. The binaries themselves are not downloaded
/// again: they are tens of megabytes, and the checksum objects read back here
/// are what a machine trusts them by.
async fn verify(release: &Release) -> Result<()> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("reading the clock")?
        .as_nanos();

    for object in release.verified_objects() {
        let expected = fs::read_to_string(&object.file)
            .await
            .with_context(|| format!("reading {}", object.file.display()))?;
        let url = release.channel.verification_url(object.name(), nonce);
        let response = zenwave::get(url.as_str())
            .await
            .with_context(|| format!("GET {url}"))?;
        let served_type = response
            .headers()
            .get(CONTENT_TYPE)
            .with_context(|| format!("{url} answered without a content type"))?
            .to_str()
            .with_context(|| format!("{url} answered a content type that is not text"))?
            .to_owned();
        ensure!(
            served_type == object.published.content_type,
            "{url} is served as {served_type:?}, this publish uploaded {:?}",
            object.published.content_type
        );
        let served = response
            .into_string()
            .await
            .with_context(|| format!("{url} did not answer text"))?;
        ensure!(
            served.as_str() == expected,
            "{url} serves {served:?}, this publish staged {expected:?}"
        );
        info!(object = object.name(), "verified as served");
    }
    Ok(())
}
