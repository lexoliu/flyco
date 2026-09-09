//! The objects one `flycod` release publishes.
//!
//! A release crosses a boundary like any wire message does: `cargo xtask
//! publish-flycod` writes these objects into the deployment's R2 bucket and
//! `flyco_api::releases` serves exactly these names back to the machine
//! installer, refusing every other key. Both sides read this table, so the
//! publisher cannot produce an object the control plane would refuse and the
//! control plane cannot advertise one no publish writes.
//!
//! Nothing here says how an object is *built* — the target triples, the glibc
//! pin and the staging directory are the publisher's business and live in
//! `crates/xtask`.

/// One object of a release: what it is called, and what it is served as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishedObject {
    /// Name the object is published and served under, which is also the last
    /// segment of its key under `releases/<channel>/` and the last segment of
    /// the `/install/` URL the installer downloads it from.
    pub name: &'static str,
    /// Media type the control plane answers with.
    ///
    /// Fixed per object rather than sniffed or read back from storage: a
    /// bucket object whose stored type drifted would otherwise change how a
    /// machine treats a binary it is about to execute.
    pub content_type: &'static str,
}

/// Media type of an executable the installer downloads and runs.
const EXECUTABLE: &str = "application/octet-stream";

/// Media type of the plain-text objects: the checksum lines and the unit file.
const TEXT: &str = "text/plain; charset=utf-8";

/// One published architecture: the daemon binary, and the checksum attesting
/// it.
///
/// The two travel together because the installer downloads both and refuses
/// the machine if `sha256sum --check` disagrees, so a release that has one
/// without the other is half-published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishedBinary {
    /// What `uname -m` reports on a machine this binary runs on, which is how
    /// the installer picks between them.
    pub uname: &'static str,
    /// The `flycod-linux-<uname>` binary.
    pub binary: PublishedObject,
    /// The `flycod-linux-<uname>.sha256` line attesting it.
    pub checksum: PublishedObject,
}

/// Claude Code's managed-policy directory on Linux.
///
/// `managed-settings.json` and `managed-mcp.json` live here and outrank
/// every other settings source, which is what makes flyco's MCP allowlist a
/// fact about the filesystem rather than a request the agent can decline.
/// The CLI reads this exact path, so it is not ours to move.
///
/// It lives beside the release because two published things have to agree
/// about it: the provisioner writes it into a machine's configuration, and
/// the unit has to let the daemon write into it. The unit runs under
/// `ProtectSystem=strict`, which makes the whole hierarchy read-only, so a
/// path missing from `ReadWritePaths` is a daemon that cannot start.
pub const CLAUDE_MANAGED_DIR: &str = "/etc/claude-code";

/// The OCI image a session runs in on a container runtime — a machine the
/// user owns, or a managed container service (issue #235).
///
/// Published by `cargo xtask publish-flycod` with the daemon it carries,
/// tagged by channel (`:dev`), by wire protocol version (`:wire-9`), and
/// `:latest`; named here so the publisher and every driver that starts one
/// spell it the same way.
pub const SESSION_IMAGE: &str = "ghcr.io/lexoliu/flyco-session";

/// [`SESSION_IMAGE`] at its `latest` tag: what a host runs unless the user
/// names another image.
pub const SESSION_IMAGE_LATEST: &str = "ghcr.io/lexoliu/flyco-session:latest";

/// The 64-bit x86 build.
pub const X86_64: PublishedBinary = PublishedBinary {
    uname: "x86_64",
    binary: PublishedObject {
        name: "flycod-linux-x86_64",
        content_type: EXECUTABLE,
    },
    checksum: PublishedObject {
        name: "flycod-linux-x86_64.sha256",
        content_type: TEXT,
    },
};

/// The 64-bit Arm build.
pub const AARCH64: PublishedBinary = PublishedBinary {
    uname: "aarch64",
    binary: PublishedObject {
        name: "flycod-linux-aarch64",
        content_type: EXECUTABLE,
    },
    checksum: PublishedObject {
        name: "flycod-linux-aarch64.sha256",
        content_type: TEXT,
    },
};

/// Number of architectures a release covers.
pub const BINARY_COUNT: usize = 2;

/// Every architecture a release covers.
///
/// An architecture absent here has no binary, and the installer refuses the
/// machine rather than downloading one built for another instruction set.
pub const BINARIES: [PublishedBinary; BINARY_COUNT] = [X86_64, AARCH64];

/// The systemd unit that runs the daemon on a session VM, installed by
/// [`INSTALLER`].
pub const UNIT: PublishedObject = PublishedObject {
    name: "flycod.service",
    content_type: TEXT,
};

/// The systemd unit that runs `flycod host` on a machine the user owns.
///
/// A second unit rather than a second mode of the first: the two run
/// different commands, as different users, against different configuration
/// files, and a host is enrolled by a person pasting one line into a root
/// shell rather than by cloud-init building an image. The installer's `host
/// enroll` path is what puts this one on a machine
/// (docs/host-enrollment.md).
pub const HOST_UNIT: PublishedObject = PublishedObject {
    name: "flycod-host.service",
    content_type: TEXT,
};

/// The machine installer, which cloud-init fetches and runs as root.
///
/// Served as a shell script rather than as plain text because it is the one
/// object a machine executes straight off the wire.
pub const INSTALLER: PublishedObject = PublishedObject {
    name: "flycod.sh",
    content_type: "text/x-shellscript; charset=utf-8",
};

/// Number of objects a release copies verbatim out of the repository.
pub const ASSET_COUNT: usize = 3;

/// The objects a release copies verbatim out of the repository, in publish
/// order.
///
/// The two units come first and the installer last: the installer is the
/// entry point cloud-init fetches and the one line a user pastes into a root
/// shell, so it is the last thing a publish makes current, and a publish
/// interrupted midway never points a machine at a unit file that is not
/// there yet.
pub const ASSETS: [PublishedObject; ASSET_COUNT] = [UNIT, HOST_UNIT, INSTALLER];

/// Number of objects one release publishes.
pub const OBJECT_COUNT: usize = BINARY_COUNT * 2 + ASSET_COUNT;

/// Every object one release publishes, and the control plane serves.
pub const OBJECTS: [PublishedObject; OBJECT_COUNT] = [
    X86_64.binary,
    X86_64.checksum,
    AARCH64.binary,
    AARCH64.checksum,
    UNIT,
    HOST_UNIT,
    INSTALLER,
];

/// The published object called `name`, if a release publishes one.
///
/// This is the whole of the control plane's allowlist: a name absent here
/// never becomes a storage key, so `/install/` cannot be walked into the rest
/// of the bucket.
#[must_use]
pub fn object(name: &str) -> Option<PublishedObject> {
    OBJECTS.into_iter().find(|object| object.name == name)
}

#[cfg(test)]
mod tests {
    use super::{ASSETS, BINARIES, HOST_UNIT, INSTALLER, OBJECT_COUNT, OBJECTS, object};

    #[test]
    fn every_published_object_is_named_once() {
        let mut names = OBJECTS.map(|object| object.name).to_vec();
        names.sort_unstable();
        names.dedup();

        assert_eq!(names.len(), OBJECT_COUNT);
    }

    #[test]
    fn the_objects_are_the_binaries_their_checksums_and_the_assets() {
        let mut expected = BINARIES
            .into_iter()
            .flat_map(|architecture| [architecture.binary, architecture.checksum])
            .chain(ASSETS)
            .map(|object| object.name)
            .collect::<Vec<_>>();
        let mut published = OBJECTS.map(|object| object.name).to_vec();
        expected.sort_unstable();
        published.sort_unstable();

        assert_eq!(published, expected);
    }

    #[test]
    fn a_checksum_is_named_after_the_binary_it_attests() {
        for architecture in BINARIES {
            assert_eq!(
                architecture.checksum.name,
                format!("{}.sha256", architecture.binary.name)
            );
            assert!(architecture.binary.name.ends_with(architecture.uname));
        }
    }

    #[test]
    fn a_lookup_answers_only_for_published_names() {
        assert_eq!(object("flycod.sh"), Some(INSTALLER));
        assert_eq!(object("flycod-host.service"), Some(HOST_UNIT));
        assert_eq!(object("../transcripts/private"), None);
        assert_eq!(object("flycod-linux-riscv64"), None);
    }
}
