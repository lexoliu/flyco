//! The typed description of a `flycod` release.
//!
//! Everything the publish is allowed to do is spelled out here: which
//! architectures exist, where the objects they produce come from, the exact
//! bytes of a checksum file, and the exact argument vector of every external
//! program the publish runs. Nothing in this module performs I/O, so all of
//! it is assertable in unit tests — which is the point: the published names
//! must be the ones the control plane serves, and the commands must equal
//! the ones the deploy notes document.
//!
//! The names themselves are not declared here. They come from
//! [`flyco_core::release`], the one table `flyco_api::releases` allowlists
//! from as well, so a publisher that grew an object the control plane does
//! not serve cannot exist. What this module adds is how each object is
//! produced: two cross-compiles, two checksum lines, and three files copied
//! verbatim out of the repository.

use std::{
    borrow::Cow,
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
};

use askama::Template;
use flyco_core::release::{ASSET_COUNT, BINARY_COUNT, PublishedBinary, PublishedObject};
use sha2::{Digest as _, Sha256};

/// The R2 bucket the control plane serves `releases/` out of.
///
/// One bucket, declared once in `crates/api/Skyzen.toml` as the
/// `TRANSCRIPTS` binding: the Worker reads release objects through the same
/// storage service it writes transcripts to.
pub const BUCKET: &str = "flyco-transcripts";

/// glibc version the published binaries are pinned to.
///
/// 2.31 is Ubuntu 20.04's, the oldest base image flyco provisions, and
/// `cargo zigbuild` links against exactly that version rather than the build
/// machine's — which is what makes one binary run on every session VM.
const GLIBC: &str = "2.31";

/// Cargo package that owns the daemon binary.
const PACKAGE: &str = "flyco-daemon";

/// The binary target `cargo zigbuild` is asked for.
const BINARY: &str = "flycod";

/// Where the repository keeps the installer assets, relative to the workspace
/// root.
///
/// They live beside the code that publishes them because the publish is the
/// only thing that reads them. `/install/{artifact}` is answered from the
/// bucket — the embedded PWA never serves these two — so a second copy under
/// `frontend/public/` could only drift from the binaries it installs.
pub const ASSET_DIR: [&str; 3] = ["crates", "xtask", "install"];

/// One Linux CPU architecture `flycod` is published for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Architecture {
    /// The binary and checksum this architecture contributes, named by the
    /// same table the control plane serves from.
    pub published: PublishedBinary,
    /// Rust target triple, without the glibc pin.
    pub triple: &'static str,
}

/// Every architecture a release covers.
///
/// One entry per [`flyco_core::release::BINARIES`] entry, since an
/// architecture with no published binary has nothing to build and a binary
/// with no architecture here would never be produced.
pub const ARCHITECTURES: [Architecture; BINARY_COUNT] = [
    Architecture {
        published: flyco_core::release::X86_64,
        triple: "x86_64-unknown-linux-gnu",
    },
    Architecture {
        published: flyco_core::release::AARCH64,
        triple: "aarch64-unknown-linux-gnu",
    },
];

impl Architecture {
    /// The `--target` value passed to `cargo zigbuild`: the triple plus the
    /// glibc version to link against.
    #[must_use]
    pub fn zig_target(self) -> String {
        format!("{}.{GLIBC}", self.triple)
    }

    /// Where cargo writes the cross-compiled binary.
    ///
    /// zigbuild strips the glibc suffix before handing the target to cargo,
    /// so the output directory is named by the bare triple.
    #[must_use]
    pub fn built_binary(self, target_dir: &Path) -> PathBuf {
        target_dir.join(self.triple).join("release").join(BINARY)
    }
}

/// A release channel: one prefix in the bucket and the origin serving it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Channel {
    /// Prefix segment under `releases/` in the bucket.
    pub name: &'static str,
    /// Origin the installer downloads this channel's artifacts from, and the
    /// deployment whose wire protocol version the publish is coupled to.
    pub origin: &'static str,
}

/// Every channel that exists.
///
/// One, for now: `crates/api/src/releases.rs` serves `releases/dev` and
/// nothing else, so publishing to a prefix the Worker does not read would
/// upload objects no machine can fetch.
pub const CHANNELS: [Channel; 1] = [Channel {
    name: "dev",
    origin: "https://dev.flyco.dev",
}];

impl Channel {
    /// Object key in the bucket.
    #[must_use]
    pub fn key(self, object: &str) -> String {
        format!("releases/{}/{object}", self.name)
    }

    /// Bucket-qualified key, the form `wrangler r2 object` addresses.
    #[must_use]
    pub fn bucket_key(self, object: &str) -> String {
        format!("{BUCKET}/{}", self.key(object))
    }

    /// Directory the publish stages this channel's objects in.
    ///
    /// Everything uploaded is uploaded from here, so what a `--dry-run`
    /// leaves behind is byte for byte what a real publish would send.
    #[must_use]
    pub fn stage_dir(self, target_dir: &Path) -> PathBuf {
        target_dir.join("flycod-release").join(self.name)
    }

    /// Public URL `object` is served from once published, and the one the
    /// installer downloads it from.
    #[must_use]
    pub fn url(self, object: &str) -> String {
        format!("{}/install/{object}", self.origin)
    }

    /// URL a just-published object is read back through.
    ///
    /// The Worker serves release artifacts with `max-age=60`, so a plain
    /// fetch straight after an upload can be answered out of Cloudflare's
    /// cache with the previous release's bytes. The nonce is part of the
    /// cache key, so this reads what the bucket now holds.
    #[must_use]
    pub fn verification_url(self, object: &str, nonce: u128) -> String {
        format!("{}?published={nonce}", self.url(object))
    }

    /// Public URL reporting the wire protocol version this channel speaks.
    #[must_use]
    pub fn health_url(self) -> String {
        format!("{}/v1/healthz", self.origin)
    }
}

impl fmt::Display for Channel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name)
    }
}

/// A `--channel` value naming no channel that exists.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown release channel {given:?}; flyco publishes {known}")]
pub struct UnknownChannel {
    /// What the operator asked for.
    given: String,
    /// The channels that do exist.
    known: String,
}

impl FromStr for Channel {
    type Err = UnknownChannel;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        CHANNELS
            .into_iter()
            .find(|channel| channel.name == value)
            .ok_or_else(|| UnknownChannel {
                given: value.to_owned(),
                known: CHANNELS
                    .iter()
                    .map(|channel| channel.name)
                    .collect::<Vec<_>>()
                    .join(", "),
            })
    }
}

/// A `shasum -a 256` line.
///
/// The installer checks the downloaded binary with `sha256sum --check`, which
/// reads exactly `<hex>  <name>\n` — two spaces, binary mode's single space
/// plus its mode flag being absent — so the bytes are rendered from a
/// template rather than assembled by hand.
#[derive(Debug, Clone, PartialEq, Eq, Template)]
#[template(path = "sha256.txt", whitespace = "preserve")]
pub struct Checksum {
    /// Lowercase hex SHA-256 digest.
    pub digest: String,
    /// The object name the digest belongs to, as the installer downloads it.
    pub name: &'static str,
}

impl Checksum {
    /// Digests `bytes` as the checksum of the object called `name`.
    #[must_use]
    pub fn of(name: &'static str, bytes: &[u8]) -> Self {
        Self {
            digest: hex::encode(Sha256::digest(bytes)),
            name,
        }
    }
}

/// One object of a release: its published identity, and the staged file
/// holding the bytes that will be uploaded under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseObject {
    /// What the control plane serves this object as.
    pub published: PublishedObject,
    /// Staged file uploaded as that object.
    pub file: PathBuf,
}

impl ReleaseObject {
    /// The staged form of `published`, under its published name in
    /// `stage_dir`.
    #[must_use]
    pub fn staged(published: PublishedObject, stage_dir: &Path) -> Self {
        Self {
            published,
            file: stage_dir.join(published.name),
        }
    }

    /// Published name, which is also the last segment of its key.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.published.name
    }
}

/// Where the repository keeps the source of `asset`, the file a publish
/// copies into the staging directory unchanged.
#[must_use]
pub fn asset_source(workspace_root: &Path, asset: PublishedObject) -> PathBuf {
    let mut path = workspace_root.to_path_buf();
    path.extend(ASSET_DIR);
    path.push(asset.name);
    path
}

/// What one architecture contributes to a release: a binary and its checksum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchitectureArtifacts {
    /// The architecture these were built for.
    pub architecture: Architecture,
    /// The `flycod-linux-<arch>` binary.
    pub binary: ReleaseObject,
    /// The `flycod-linux-<arch>.sha256` file.
    pub checksum: ReleaseObject,
    /// The line that checksum file contains.
    pub line: Checksum,
}

impl ArchitectureArtifacts {
    /// Names the two objects `architecture` contributes and where they are
    /// staged, given the binary's `bytes`.
    #[must_use]
    pub fn stage(architecture: Architecture, stage_dir: &Path, bytes: &[u8]) -> Self {
        let published = architecture.published;
        Self {
            architecture,
            binary: ReleaseObject::staged(published.binary, stage_dir),
            checksum: ReleaseObject::staged(published.checksum, stage_dir),
            line: Checksum::of(published.binary.name, bytes),
        }
    }

    /// Both objects, in upload order: the binary before the checksum that
    /// attests it, so a half-finished publish never advertises a digest for
    /// bytes that are not there yet.
    pub fn objects(&self) -> impl Iterator<Item = &ReleaseObject> {
        [&self.binary, &self.checksum].into_iter()
    }
}

/// The complete set of objects one publish produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Channel the objects belong to.
    pub channel: Channel,
    /// One pair of objects per architecture — four objects, by construction.
    pub artifacts: [ArchitectureArtifacts; BINARY_COUNT],
    /// The installer and the two systemd units, copied out of the repository
    /// — three more objects, by construction, for seven in all.
    pub assets: [ReleaseObject; ASSET_COUNT],
}

impl Release {
    /// Every object of the release, in upload order: each architecture's
    /// binary and checksum, then the two unit files, then the installer.
    ///
    /// The installer is last because it is the entry point cloud-init
    /// fetches: the object that makes a release live is the object published
    /// once everything it downloads is already there.
    pub fn objects(&self) -> impl Iterator<Item = &ReleaseObject> {
        self.artifacts
            .iter()
            .flat_map(ArchitectureArtifacts::objects)
            .chain(&self.assets)
    }

    /// The objects a finished publish reads back through `/install/`.
    ///
    /// Everything but the binaries themselves, which are tens of megabytes
    /// and are attested by the checksum objects that are read back.
    pub fn verified_objects(&self) -> impl Iterator<Item = &ReleaseObject> {
        self.artifacts
            .iter()
            .map(|artifact| &artifact.checksum)
            .chain(&self.assets)
    }
}

/// An external program and its argument vector.
///
/// Every argument is one element and the program is executed directly: no
/// shell parses any of this, so a staging path containing a space is one
/// argument and a bucket key can never be re-split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// Program to execute, resolved on `PATH`.
    pub program: &'static str,
    /// Arguments, already split.
    pub args: Vec<String>,
}

impl Invocation {
    /// `cargo zigbuild --release -p flyco-daemon --bin flycod --target <a> --target <b>`
    ///
    /// One invocation for both architectures: zigbuild accepts repeated
    /// `--target`, and cargo then shares one dependency graph between them.
    #[must_use]
    pub fn zigbuild() -> Self {
        let mut args = vec![
            "zigbuild".to_owned(),
            "--release".to_owned(),
            "-p".to_owned(),
            PACKAGE.to_owned(),
            "--bin".to_owned(),
            BINARY.to_owned(),
        ];
        for architecture in ARCHITECTURES {
            args.push("--target".to_owned());
            args.push(architecture.zig_target());
        }
        Self {
            program: "cargo",
            args,
        }
    }

    /// `wrangler r2 object put <bucket>/<key> --file <path> --content-type <type> --remote`
    ///
    /// `--remote` is required: without it wrangler writes to the local
    /// simulator's bucket and the publish would report success having
    /// uploaded nothing. `--content-type` stores the same media type the
    /// control plane serves the object with, so the bucket and the Worker
    /// describe an object the same way.
    #[must_use]
    pub fn upload(channel: Channel, object: &ReleaseObject) -> Self {
        Self {
            program: "wrangler",
            args: vec![
                "r2".to_owned(),
                "object".to_owned(),
                "put".to_owned(),
                channel.bucket_key(object.name()),
                "--file".to_owned(),
                object.file.display().to_string(),
                "--content-type".to_owned(),
                object.published.content_type.to_owned(),
                "--remote".to_owned(),
            ],
        }
    }

    /// The command to spawn, arguments passed individually.
    #[must_use]
    pub fn command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(self.program);
        command.args(&self.args);
        command
    }
}

impl fmt::Display for Invocation {
    /// Renders the invocation the way an operator would type it, for logs.
    /// This is never executed — [`Invocation::command`] is.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.program)?;
        for argument in &self.args {
            write!(
                formatter,
                " {}",
                shell_escape::escape(Cow::Borrowed(argument.as_str()))
            )?;
        }
        Ok(())
    }
}

/// The wire-protocol coupling between the daemon being published and the
/// control plane already deployed on the channel.
///
/// A daemon speaking a version the Worker refuses is rejected at `Hello`, so
/// every machine provisioned after such a publish is dead on arrival. The
/// check is made before the build, not after, so a mismatch costs no
/// cross-compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireCheck {
    /// Channel whose deployment was asked.
    pub channel: Channel,
    /// `flyco_core::WIRE_PROTOCOL_VERSION` this build speaks.
    pub publishing: u32,
    /// What the channel's `/v1/healthz` reports.
    pub deployed: u32,
    /// Whether the operator deliberately accepted a mismatch.
    pub allowed_mismatch: bool,
}

/// A publish refused because the two planes would speak different protocols.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{origin} speaks wire protocol {deployed} and this build speaks {publishing}: \
     every machine provisioned from it would be refused at Hello. \
     Deploy the control plane first, or pass --allow-wire-mismatch."
)]
pub struct WireMismatch {
    /// Origin that was asked.
    origin: &'static str,
    /// Version it reported.
    deployed: u32,
    /// Version this build speaks.
    publishing: u32,
}

impl WireCheck {
    /// Whether this publish may proceed.
    ///
    /// # Errors
    ///
    /// Returns [`WireMismatch`] when the versions differ and the operator did
    /// not pass `--allow-wire-mismatch`.
    pub const fn approve(self) -> Result<(), WireMismatch> {
        if self.publishing == self.deployed || self.allowed_mismatch {
            return Ok(());
        }
        Err(WireMismatch {
            origin: self.channel.origin,
            deployed: self.deployed,
            publishing: self.publishing,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use askama::Template as _;
    use flyco_core::release::{ASSETS, OBJECT_COUNT, OBJECTS};

    use super::{
        ARCHITECTURES, ArchitectureArtifacts, CHANNELS, Channel, Checksum, Invocation, Release,
        ReleaseObject, WireCheck, asset_source,
    };

    /// The one channel that exists, as `--channel dev` resolves it.
    fn dev() -> Channel {
        "dev".parse().expect("dev is a channel")
    }

    #[test]
    fn a_checksum_line_is_exactly_what_sha256sum_check_reads() {
        let checksum = Checksum::of("flycod-linux-x86_64", b"");

        assert_eq!(
            checksum.render().expect("render"),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  flycod-linux-x86_64\n"
        );
    }

    #[test]
    fn a_checksum_line_separates_digest_and_name_with_two_spaces() {
        let line = Checksum::of("flycod-linux-aarch64", b"payload")
            .render()
            .expect("render");
        let (digest, rest) = line.split_at(64);

        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(rest, "  flycod-linux-aarch64\n");
    }

    #[test]
    fn a_release_publishes_the_seven_objects_in_upload_order() {
        let release = release();

        assert_eq!(
            release
                .objects()
                .map(ReleaseObject::name)
                .collect::<Vec<_>>(),
            [
                "flycod-linux-x86_64",
                "flycod-linux-x86_64.sha256",
                "flycod-linux-aarch64",
                "flycod-linux-aarch64.sha256",
                "flycod.service",
                "flycod-host.service",
                "flycod.sh",
            ]
        );
    }

    /// The publisher and the control plane are one allowlist or they are a
    /// broken release: an object nobody serves is a wasted upload, and a name
    /// the Worker serves that no publish writes is a permanent 404.
    #[test]
    fn a_release_publishes_exactly_the_objects_the_control_plane_serves() {
        let mut published = release()
            .objects()
            .map(|object| object.published)
            .collect::<Vec<_>>();
        let mut allowlisted = OBJECTS.to_vec();
        published.sort_by_key(|object| object.name);
        allowlisted.sort_by_key(|object| object.name);

        assert_eq!(published, allowlisted);
        assert_eq!(published.len(), OBJECT_COUNT);
    }

    #[test]
    fn every_object_is_uploaded_with_the_media_type_it_is_served_as() {
        for object in release().objects() {
            let upload = Invocation::upload(dev(), object);
            let content_type = upload
                .args
                .iter()
                .skip_while(|argument| argument.as_str() != "--content-type")
                .nth(1)
                .expect("the upload declares a content type");

            assert_eq!(content_type, object.published.content_type);
        }
    }

    #[test]
    fn the_installer_and_both_units_are_read_from_the_repository() {
        let root = Path::new("/w");

        assert_eq!(
            ASSETS.map(|asset| asset_source(root, asset)),
            [
                PathBuf::from("/w/crates/xtask/install/flycod.service"),
                PathBuf::from("/w/crates/xtask/install/flycod-host.service"),
                PathBuf::from("/w/crates/xtask/install/flycod.sh"),
            ]
        );
    }

    #[test]
    fn the_binaries_are_attested_rather_than_downloaded_back() {
        let release = release();

        assert_eq!(
            release
                .verified_objects()
                .map(ReleaseObject::name)
                .collect::<Vec<_>>(),
            [
                "flycod-linux-x86_64.sha256",
                "flycod-linux-aarch64.sha256",
                "flycod.service",
                "flycod-host.service",
                "flycod.sh",
            ]
        );
    }

    #[test]
    fn an_architecture_names_its_target_its_object_and_its_build_output() {
        let [x86_64, aarch64] = ARCHITECTURES;

        assert_eq!(x86_64.zig_target(), "x86_64-unknown-linux-gnu.2.31");
        assert_eq!(aarch64.zig_target(), "aarch64-unknown-linux-gnu.2.31");
        assert_eq!(x86_64.published.binary.name, "flycod-linux-x86_64");
        assert_eq!(
            aarch64.built_binary(Path::new("/w/target")),
            PathBuf::from("/w/target/aarch64-unknown-linux-gnu/release/flycod")
        );
    }

    #[test]
    fn a_channel_addresses_one_bucket_prefix_one_stage_and_one_origin() {
        let channel = dev();

        assert_eq!(
            channel.bucket_key("flycod-linux-x86_64"),
            "flyco-transcripts/releases/dev/flycod-linux-x86_64"
        );
        assert_eq!(
            channel.stage_dir(Path::new("/w/target")),
            PathBuf::from("/w/target/flycod-release/dev")
        );
        assert_eq!(
            channel.url("flycod-linux-x86_64.sha256"),
            "https://dev.flyco.dev/install/flycod-linux-x86_64.sha256"
        );
        assert_eq!(
            channel.url("flycod.sh"),
            "https://dev.flyco.dev/install/flycod.sh"
        );
        assert_eq!(channel.health_url(), "https://dev.flyco.dev/v1/healthz");
    }

    #[test]
    fn verification_reads_past_the_edge_cache() {
        assert_eq!(
            dev().verification_url("flycod-linux-aarch64.sha256", 17),
            "https://dev.flyco.dev/install/flycod-linux-aarch64.sha256?published=17"
        );
        assert_eq!(
            dev().verification_url("flycod.sh", 17),
            "https://dev.flyco.dev/install/flycod.sh?published=17"
        );
    }

    #[test]
    fn an_unknown_channel_names_the_ones_that_exist() {
        let error = "nightly".parse::<Channel>().expect_err("no such channel");

        assert_eq!(
            error.to_string(),
            "unknown release channel \"nightly\"; flyco publishes dev"
        );
        assert_eq!(CHANNELS.len(), 1);
    }

    #[test]
    fn the_build_command_is_exactly_the_documented_zigbuild() {
        let invocation = Invocation::zigbuild();

        assert_eq!(invocation.program, "cargo");
        assert_eq!(
            invocation.args,
            [
                "zigbuild",
                "--release",
                "-p",
                "flyco-daemon",
                "--bin",
                "flycod",
                "--target",
                "x86_64-unknown-linux-gnu.2.31",
                "--target",
                "aarch64-unknown-linux-gnu.2.31",
            ]
        );
    }

    #[test]
    fn the_upload_command_is_exactly_the_documented_wrangler_put() {
        let invocation = Invocation::upload(
            dev(),
            &ReleaseObject::staged(
                flyco_core::release::X86_64.binary,
                Path::new("/w/target/flycod-release/dev"),
            ),
        );

        assert_eq!(invocation.program, "wrangler");
        assert_eq!(
            invocation.args,
            [
                "r2",
                "object",
                "put",
                "flyco-transcripts/releases/dev/flycod-linux-x86_64",
                "--file",
                "/w/target/flycod-release/dev/flycod-linux-x86_64",
                "--content-type",
                "application/octet-stream",
                "--remote",
            ]
        );
    }

    #[test]
    fn no_invocation_goes_through_a_shell() {
        let release = release();
        let uploads = release
            .objects()
            .map(|object| Invocation::upload(dev(), object))
            .collect::<Vec<_>>();

        for invocation in std::iter::once(Invocation::zigbuild()).chain(uploads) {
            assert!(
                !["sh", "bash", "fish", "zsh", "cmd", "powershell"].contains(&invocation.program),
                "{} is a shell",
                invocation.program
            );
            let command = invocation.command();
            assert_eq!(command.get_program(), invocation.program);
            assert_eq!(
                command
                    .get_args()
                    .map(|argument| argument.to_string_lossy().into_owned())
                    .collect::<Vec<_>>(),
                invocation.args
            );
        }
    }

    #[test]
    fn matching_wire_versions_publish() {
        assert_eq!(
            WireCheck {
                channel: dev(),
                publishing: 4,
                deployed: 4,
                allowed_mismatch: false,
            }
            .approve(),
            Ok(())
        );
    }

    #[test]
    fn a_wire_mismatch_refuses_and_names_both_versions() {
        let error = WireCheck {
            channel: dev(),
            publishing: 4,
            deployed: 3,
            allowed_mismatch: false,
        }
        .approve()
        .expect_err("refused");

        assert!(error.to_string().contains("speaks wire protocol 3"));
        assert!(error.to_string().contains("this build speaks 4"));
        assert!(error.to_string().contains("--allow-wire-mismatch"));
    }

    #[test]
    fn a_wire_mismatch_is_publishable_when_the_operator_accepts_it() {
        assert_eq!(
            WireCheck {
                channel: dev(),
                publishing: 4,
                deployed: 3,
                allowed_mismatch: true,
            }
            .approve(),
            Ok(())
        );
    }

    /// A release staged from placeholder bytes, for the naming assertions.
    fn release() -> Release {
        let stage = Path::new("/w/target/flycod-release/dev");
        let [x86_64, aarch64] = ARCHITECTURES;
        Release {
            channel: dev(),
            artifacts: [
                ArchitectureArtifacts::stage(x86_64, stage, b"x86_64"),
                ArchitectureArtifacts::stage(aarch64, stage, b"aarch64"),
            ],
            assets: ASSETS.map(|asset| ReleaseObject::staged(asset, stage)),
        }
    }
}
