//! What the machine says about itself.
//!
//! Reported at enrollment and again on every attach, because the facts
//! change: memory is added, a disk fills, Podman is upgraded. The control
//! plane schedules against them and has no other way to learn them — nothing
//! at flyco can reach this machine to look.
//!
//! # Readings, then facts
//!
//! Taking the readings needs a Linux machine; turning them into
//! [`HostFacts`] needs only the text they produced. So the two are separate:
//! [`Readings`] is what `uname`, `/proc/meminfo`, `statvfs` and `podman
//! --version` said, and [`Readings::into_facts`] is the parsing, which is
//! where every mistake a machine can make lives and is therefore where the
//! tests are.

use std::path::{Path, PathBuf};

use flyco_core::host::HostFacts;
use flyco_core::machine::CpuArchitecture;
use tokio::process::Command;

/// What `podman --version` prints before the version itself.
const PODMAN_VERSION_PREFIX: &str = "podman version ";

/// The line of `/proc/meminfo` carrying total memory.
const MEM_TOTAL: &str = "MemTotal:";

/// Where a Linux kernel publishes memory.
const MEMINFO: &str = "/proc/meminfo";

/// What a machine without Podman is told to run.
///
/// Named in the refusal rather than left to the reader: an enrollment that
/// fails is somebody at a terminal on their own machine, and the next thing
/// they need is the command.
pub const PODMAN_INSTALL_COMMAND: &str = "sudo apt-get install -y podman uidmap dbus-user-session";

/// The machine could not describe itself.
#[derive(Debug, thiserror::Error)]
pub enum FactsError {
    /// Podman is not installed, which is not something flyco works around:
    /// a session on somebody's own machine runs in a container or it does
    /// not run.
    #[error(
        "podman is not installed on this machine, and a flyco session runs in a container. \
         Install it with `{PODMAN_INSTALL_COMMAND}` and enrol again"
    )]
    NoPodman,
    /// A program the readings need could not be run.
    #[error("could not run `{program}` to describe this machine")]
    Command {
        /// The program that was tried.
        program: &'static str,
        /// The underlying cause.
        #[source]
        source: std::io::Error,
    },
    /// A program ran and failed.
    #[error("`{program}` failed: {output}")]
    Failed {
        /// The program that failed.
        program: &'static str,
        /// What it said.
        output: String,
    },
    /// A file the readings need could not be read.
    #[error("could not read {path} to describe this machine")]
    Read {
        /// The path that was tried.
        path: PathBuf,
        /// The underlying cause.
        #[source]
        source: std::io::Error,
    },
    /// The filesystem holding Podman's storage could not be measured.
    #[error("could not measure the free space at {path}")]
    Disk {
        /// The directory that was measured.
        path: PathBuf,
        /// The underlying cause.
        #[source]
        source: rustix::io::Errno,
    },
    /// The kernel reports an instruction set flyco publishes no image for.
    #[error("flyco has no session image for the {0} architecture")]
    UnknownArchitecture(String),
    /// A reading was not in the shape its producer documents.
    #[error("{reading} did not report {wanted}: {got:?}")]
    Unreadable {
        /// Where the reading came from.
        reading: &'static str,
        /// What was being looked for.
        wanted: &'static str,
        /// What was there instead.
        got: String,
    },
    /// The machine has no usable CPU count, which is not a machine a
    /// session can be scheduled onto.
    #[error("could not read this machine's CPU count")]
    NoCpus(#[source] std::io::Error),
}

/// Everything one machine measured about itself, before any of it is parsed.
///
/// Public so a test can state a machine that does not exist — an arm64 host
/// with 400 GiB free and Podman 5.4 — and assert what flyco makes of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Readings {
    /// `uname -m`.
    pub machine: String,
    /// `uname -r`.
    pub kernel: String,
    /// `uname -n`.
    pub hostname: String,
    /// The whole of `/proc/meminfo`.
    pub meminfo: String,
    /// How many CPUs the kernel offers this process.
    pub vcpus: u32,
    /// Free bytes on the filesystem holding Podman's storage.
    pub disk_free_bytes: u64,
    /// `podman --version`.
    pub podman_version: String,
}

impl Readings {
    /// Turns the readings into the facts the control plane schedules on.
    ///
    /// # Errors
    ///
    /// Returns [`FactsError`] if the architecture is one flyco publishes no
    /// image for, or if a reading is not in the shape its producer
    /// documents — both of which are refusals rather than guesses: a host
    /// enrolled with invented facts is a session scheduled onto a machine
    /// that cannot run it.
    pub fn into_facts(self) -> Result<HostFacts, FactsError> {
        Ok(HostFacts {
            architecture: architecture(self.machine.trim())?,
            vcpus: self.vcpus,
            memory_mib: total_memory_mib(&self.meminfo)?,
            disk_free_gib: gibibytes(self.disk_free_bytes),
            podman_version: podman_version(&self.podman_version)?,
            kernel: self.kernel.trim().to_owned(),
            hostname: self.hostname.trim().to_owned(),
        })
    }
}

/// Measures this machine, with Podman's storage at `volume_root`.
///
/// # Errors
///
/// Returns [`FactsError::NoPodman`] when Podman is not installed, and
/// [`FactsError`] for any reading that could not be taken or parsed.
pub async fn gather(volume_root: &Path) -> Result<HostFacts, FactsError> {
    Readings {
        machine: uname("-m").await?,
        kernel: uname("-r").await?,
        hostname: uname("-n").await?,
        meminfo: tokio::fs::read_to_string(MEMINFO)
            .await
            .map_err(|source| FactsError::Read {
                path: PathBuf::from(MEMINFO),
                source,
            })?,
        vcpus: vcpus()?,
        disk_free_bytes: free_bytes(volume_root)?,
        podman_version: podman().await?,
    }
    .into_facts()
}

/// One `uname` reading.
async fn uname(flag: &'static str) -> Result<String, FactsError> {
    output("uname", &[flag]).await
}

/// What `podman --version` printed, or [`FactsError::NoPodman`].
async fn podman() -> Result<String, FactsError> {
    match output("podman", &["--version"]).await {
        Err(FactsError::Command { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            Err(FactsError::NoPodman)
        }
        other => other,
    }
}

/// Runs one program and returns its stdout.
async fn output(program: &'static str, args: &[&str]) -> Result<String, FactsError> {
    let output = Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(|source| FactsError::Command { program, source })?;

    if !output.status.success() {
        return Err(FactsError::Failed {
            program,
            output: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// How many CPUs this machine offers.
fn vcpus() -> Result<u32, FactsError> {
    let parallelism = std::thread::available_parallelism().map_err(FactsError::NoCpus)?;
    Ok(u32::try_from(parallelism.get()).unwrap_or(u32::MAX))
}

/// Free bytes on the filesystem holding `path`.
///
/// The *available* blocks rather than the free ones: the reserve a
/// filesystem keeps for root is not space a rootless container can use.
fn free_bytes(path: &Path) -> Result<u64, FactsError> {
    let stats = rustix::fs::statvfs(path).map_err(|source| FactsError::Disk {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(stats.f_bavail.saturating_mul(stats.f_frsize))
}

/// The architecture a `uname -m` names.
fn architecture(machine: &str) -> Result<CpuArchitecture, FactsError> {
    match machine {
        "x86_64" | "amd64" => Ok(CpuArchitecture::X8664),
        "aarch64" | "arm64" => Ok(CpuArchitecture::Arm64),
        other => Err(FactsError::UnknownArchitecture(other.to_owned())),
    }
}

/// Total memory in MiB, from `/proc/meminfo`.
///
/// The kernel publishes it in kibibytes and says so on the line, which is
/// checked rather than assumed: a unit that changed would otherwise become a
/// host advertising a thousand times the memory it has.
fn total_memory_mib(meminfo: &str) -> Result<u64, FactsError> {
    let unreadable = |got: &str| FactsError::Unreadable {
        reading: MEMINFO,
        wanted: "a `MemTotal: <n> kB` line",
        got: got.to_owned(),
    };

    let line = meminfo
        .lines()
        .find(|line| line.starts_with(MEM_TOTAL))
        .ok_or_else(|| unreadable(meminfo))?;
    let mut fields = line[MEM_TOTAL.len()..].split_whitespace();
    let kibibytes: u64 = fields
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| unreadable(line))?;
    if fields.next() != Some("kB") {
        return Err(unreadable(line));
    }
    Ok(kibibytes / 1024)
}

/// The version `podman --version` reported.
fn podman_version(reported: &str) -> Result<String, FactsError> {
    reported
        .trim()
        .strip_prefix(PODMAN_VERSION_PREFIX)
        .map(|version| version.trim().to_owned())
        .ok_or_else(|| FactsError::Unreadable {
            reading: "podman --version",
            wanted: "a `podman version <n>` line",
            got: reported.to_owned(),
        })
}

/// Whole gibibytes in `bytes`, saturating: a host with more free space than
/// a `u32` of gibibytes can hold is reported at the largest size flyco can
/// state, which is four exbibytes and not a machine anybody owns.
fn gibibytes(bytes: u64) -> u32 {
    u32::try_from(bytes / (1024 * 1024 * 1024)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use flyco_core::machine::CpuArchitecture;

    use super::{FactsError, Readings, gather};

    /// A real `/proc/meminfo`, from a machine with 32 GiB.
    const MEMINFO: &str = include_str!("../../fixtures/host/meminfo");

    fn readings() -> Readings {
        Readings {
            machine: "aarch64\n".to_owned(),
            kernel: "6.11.0-19-generic\n".to_owned(),
            hostname: "build.lexo.cool\n".to_owned(),
            meminfo: MEMINFO.to_owned(),
            vcpus: 10,
            disk_free_bytes: 400 * 1024 * 1024 * 1024,
            podman_version: "podman version 5.4.0\n".to_owned(),
        }
    }

    #[test]
    fn a_machine_describes_itself_from_what_it_read() {
        let facts = readings().into_facts().expect("parse");

        assert_eq!(facts.architecture, CpuArchitecture::Arm64);
        assert_eq!(facts.vcpus, 10);
        assert_eq!(facts.memory_mib, 32 * 1024);
        assert_eq!(facts.disk_free_gib, 400);
        assert_eq!(facts.podman_version, "5.4.0");
        assert_eq!(facts.kernel, "6.11.0-19-generic");
        assert_eq!(facts.hostname, "build.lexo.cool");
    }

    #[test]
    fn both_spellings_of_each_architecture_are_the_same_machine() {
        for (machine, expected) in [
            ("x86_64", CpuArchitecture::X8664),
            ("amd64", CpuArchitecture::X8664),
            ("aarch64", CpuArchitecture::Arm64),
            ("arm64", CpuArchitecture::Arm64),
        ] {
            let facts = Readings {
                machine: machine.to_owned(),
                ..readings()
            }
            .into_facts()
            .expect("parse");
            assert_eq!(facts.architecture, expected);
        }
    }

    #[test]
    fn an_architecture_with_no_session_image_is_refused() {
        let error = Readings {
            machine: "riscv64".to_owned(),
            ..readings()
        }
        .into_facts()
        .expect_err("flyco publishes no riscv64 image");

        assert!(matches!(error, FactsError::UnknownArchitecture(named) if named == "riscv64"));
    }

    #[test]
    fn memory_is_read_in_the_unit_the_kernel_states() {
        let error = Readings {
            meminfo: "MemTotal:       32780480 MB\n".to_owned(),
            ..readings()
        }
        .into_facts()
        .expect_err("a unit that changed must not be guessed at");

        assert!(matches!(error, FactsError::Unreadable { .. }));
    }

    #[test]
    fn a_meminfo_without_a_total_is_refused_rather_than_defaulted() {
        let error = Readings {
            meminfo: "SwapTotal:      0 kB\n".to_owned(),
            ..readings()
        }
        .into_facts()
        .expect_err("a machine with no stated memory is not a machine to schedule onto");

        assert!(matches!(error, FactsError::Unreadable { .. }));
    }

    #[test]
    fn a_podman_that_does_not_say_its_version_is_refused() {
        let error = Readings {
            podman_version: "command not found".to_owned(),
            ..readings()
        }
        .into_facts()
        .expect_err("the version is what the control plane records");

        assert!(matches!(error, FactsError::Unreadable { .. }));
    }

    #[test]
    fn free_space_is_reported_in_whole_gibibytes() {
        let facts = Readings {
            // Just short of 12 GiB: a host reports what fits, not what
            // rounds up.
            disk_free_bytes: 12 * 1024 * 1024 * 1024 - 1,
            ..readings()
        }
        .into_facts()
        .expect("parse");

        assert_eq!(facts.disk_free_gib, 11);
    }

    /// The refusal a machine without Podman gets, with the command on it.
    #[tokio::test]
    async fn a_machine_without_podman_is_refused_by_name() {
        // `gather` reads this machine, and every machine a test runs on has
        // a `uname`; what it may not have is Podman. Both answers are
        // acceptable — what is asserted is that the *only* refusal that
        // mentions Podman is the one naming the install command.
        if let Err(error) = gather(std::env::temp_dir().as_path()).await {
            let message = error.to_string();
            assert!(
                !message.contains("podman") || message.contains("apt-get install -y podman"),
                "a machine without podman must be told how to get one: {message}"
            );
        }
    }
}
