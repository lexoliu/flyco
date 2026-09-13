//! Hosts: a Linux machine the user owns, enrolled rather than dialled.
//!
//! The control plane runs on Cloudflare Workers and has no TCP sockets, so
//! it can never open a connection to somebody's machine. A host is therefore
//! **enrolled**: `flycod host` is installed with a single-use enrollment
//! token, registers itself, and holds one outbound command stream to its
//! [`HostRoom`] for as long as it is up. Everything the control plane wants
//! done on that machine travels down that stream as container work.
//!
//! What lives here is the part both planes agree on: what a host says about
//! itself ([`HostFacts`]), where it is in its life ([`HostState`]), and the
//! documents the enrollment routes exchange. The container jobs themselves
//! are `flyco_provider::host`, because they carry a daemon bootstrap.
//!
//! [`HostRoom`]: https://github.com/lexoliu/flyco/blob/dev/docs/host-enrollment.md

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::id::{EnrollmentTokenId, HostId, MachineId};
use crate::machine::{CpuArchitecture, MachineCapacity, MachineLineage};

/// Prefix every host credential carries.
///
/// One prefix for both the enrollment token and the host token, because
/// they are the same kind of secret at two ages: a leaked one is
/// recognisable as flyco's, and neither is ever a user credential. Which of
/// the two a presented string is follows from the route it was presented
/// to, exactly as `fd_` says "a session's daemon" and nothing more.
pub const HOST_TOKEN_PREFIX: &str = "fh_";

/// How long a minted enrollment token may sit unspent, in seconds.
///
/// Ten minutes: long enough to paste one command into a terminal on another
/// machine, short enough that a token left in a shell history is worthless
/// by the time anybody reads it.
pub const ENROLLMENT_TOKEN_TTL_SECONDS: u64 = 600;

/// The [`MachineLineage::family`] every host's catalog entry carries.
///
/// A family of one. It exists because curation groups by family before it
/// compares generations, and a host that named no family at all would be
/// grouped with every other unnumbered entry in its account — of which
/// there are none, since a host is its own account, which is precisely why
/// one honest constant is enough.
pub const HOST_FAMILY: &str = "host";

/// What a host says about itself when it greets the control plane.
///
/// Reported by the machine rather than configured by the user, and
/// refreshed on every attach: a host that gained memory, filled its disk,
/// or was upgraded to another Podman is a different machine to schedule
/// onto, and flyco has no other way to learn it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HostFacts {
    /// The instruction set it runs.
    pub architecture: CpuArchitecture,
    /// Virtual CPU count, as the kernel reports it.
    pub vcpus: u32,
    /// Total memory in MiB.
    pub memory_mib: u64,
    /// Free space in GiB where Podman keeps its containers and volumes.
    ///
    /// Free rather than total, because it is the number that decides
    /// whether another session fits.
    pub disk_free_gib: u32,
    /// Podman's own version string, e.g. `5.4.0`.
    pub podman_version: String,
    /// Kernel release, as `uname -r` prints it.
    pub kernel: String,
    /// The machine's hostname.
    ///
    /// Also how the host names itself in the machine catalog: it is its own
    /// region and its own machine type, and there is nothing else to call
    /// it that a person would recognise.
    pub hostname: String,
}

impl HostFacts {
    /// How much compute this host offers, as a catalog entry states it.
    #[must_use]
    pub const fn capacity(&self) -> MachineCapacity {
        MachineCapacity {
            vcpus: self.vcpus,
            memory_mib: self.memory_mib,
        }
    }

    /// Where this host sits in a line-up of one.
    ///
    /// The architecture is the load-bearing half — an arm64 host cannot run
    /// an x86-64 session image, and curation never compares across it — and
    /// the family is [`HOST_FAMILY`] with no generation, because a machine
    /// somebody owns is not a numbered member of a product line.
    #[must_use]
    pub fn lineage(&self) -> MachineLineage {
        MachineLineage {
            architecture: self.architecture,
            family: HOST_FAMILY.to_owned(),
            generation: None,
        }
    }
}

/// Where a host is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "sql", derive(skyzen::Column))]
pub enum HostState {
    /// Its daemon is attached to the control plane, and it can be
    /// scheduled onto.
    Online,
    /// Nothing is connected. Containers already on it keep running; a
    /// session there shows `Interrupted · host offline` until the daemon
    /// comes back.
    Offline,
    /// The user asked for it to go, and flyco is stopping what runs on it.
    ///
    /// A durable state rather than a moment inside one request: a removal
    /// stops containers and revokes a token, and a host caught halfway
    /// between those must not read as one still available to schedule onto.
    Draining,
    /// Drained and revoked. It keeps its row so the sessions that ran there
    /// still name something.
    Removed,
}

impl HostState {
    /// Whether the control plane may plan new work onto this host.
    #[must_use]
    pub const fn is_schedulable(self) -> bool {
        matches!(self, Self::Online)
    }
}

/// One row of `GET /v1/hosts`, and the body of `GET /v1/hosts/{id}`.
///
/// Carries no credential: a host's token is minted once, stored hashed, and
/// never read back, exactly like a daemon token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HostView {
    /// Identifier every route and every container job names it by.
    pub id: HostId,
    /// What the user calls it. Opens as the hostname it enrolled with.
    pub label: String,
    /// Where it is in its life.
    pub state: HostState,
    /// What it last said about itself.
    ///
    /// Absent only between the row being written and the host's first
    /// attach, which is a window the enrollment route does not leave open —
    /// enrolling carries the facts.
    pub facts: HostFacts,
    /// When its daemon was last heard from, seconds since the Unix epoch.
    pub last_seen_unix: Option<u64>,
    /// When it was enrolled, seconds since the Unix epoch.
    pub created_at_unix: u64,
}

/// Answer of `POST /v1/hosts/enrollment-tokens`.
///
/// The token is returned exactly once and stored only as a hash. The
/// command is rendered by the control plane rather than assembled by the
/// wizard, because it names *this deployment's* origin: a command built in
/// the browser would point wherever the page happened to be served from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct EnrollmentToken {
    /// Identifier the wizard polls while it waits for the machine.
    pub id: EnrollmentTokenId,
    /// The single-use token, `fh_…`.
    pub token: String,
    /// When it stops being accepted, seconds since the Unix epoch.
    pub expires_at_unix: u64,
    /// The one line to run on the machine, ready to copy.
    pub command: String,
}

/// Answer of `GET /v1/hosts/enrollment-tokens/{id}`.
///
/// What the wizard polls: either nothing has happened yet, or the machine
/// arrived and this is it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Enrollment {
    /// The token is live and unspent: no machine has enrolled with it.
    Pending,
    /// The token was spent, and this is the host it enrolled.
    Enrolled {
        /// The machine that arrived.
        host: HostView,
    },
}

/// Request body of `POST /v1/hosts/enroll`.
///
/// Presented by `flycod host enroll` on the machine itself. It carries no
/// user credential — the enrollment token *is* the credential, and it is
/// bound to the user who minted it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct EnrollHost {
    /// The single-use enrollment token, `fh_…`.
    pub token: String,
    /// What the machine is, as it measured itself.
    pub facts: HostFacts,
}

/// Answer of `POST /v1/hosts/enroll` and of `POST
/// /v1/hosts/{id}/token/rotate`.
///
/// The long-lived credential the host keeps root-only on disk, returned
/// exactly once. Rotating mints another and revokes this one, which is how
/// a host that leaked its token is recovered without re-enrolling it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct EnrolledHost {
    /// The host that now exists.
    pub host_id: HostId,
    /// Its token, `fh_…`. The only copy that will ever exist.
    pub host_token: String,
}

/// Request body of `PATCH /v1/hosts/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UpdateHost {
    /// What to call it from now on.
    pub label: String,
}

/// Longest label a host may be given.
///
/// Long enough for a sentence describing where the machine is, short enough
/// that a list stays a list.
pub const MAX_HOST_LABEL_CHARS: usize = 64;

/// What came of one container job on a host.
///
/// A sum rather than a status and a nullable pair, because "the container
/// is up and here is what it is called" and "podman refused" are different
/// answers and only one of them names a container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum JobOutcome {
    /// The session's container is running, under these Podman names.
    ///
    /// Both are reported by the host rather than assumed by the control
    /// plane, for the same reason a cloud driver's answer is recorded
    /// rather than the request that produced it: what exists is what has to
    /// be stopped and removed later.
    Running {
        /// Name of the container Podman created.
        container: String,
        /// Name of the volume holding the session's work.
        volume: String,
    },
    /// The job did what it was asked and named nothing new — a stop, a
    /// start, a removal.
    Done,
    /// It failed, with what Podman said.
    Failed {
        /// The failure, as the host saw it.
        message: String,
    },
}

impl JobOutcome {
    /// The Podman names a job produced, when it produced any.
    #[must_use]
    pub fn names(&self) -> Option<(&str, &str)> {
        match self {
            Self::Running { container, volume } => Some((container, volume)),
            Self::Done | Self::Failed { .. } => None,
        }
    }
}

/// Request body of `POST /v1/hosts/{id}/job-results`.
///
/// The durable half of `HostToControl::JobResult`. The relay frame beside
/// it is what lets the host's room forget a job it was holding; this is what
/// completes the machine row — and it has to be a REST call rather than a
/// room frame because a Durable Object can reach neither D1 nor the
/// provisioning queue, exactly as [`crate::wire::ReportSpotNotice`]
/// documents for a session's daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ReportJobResult {
    /// The machine the job acted on, which is also the job's identity: one
    /// container job is one machine, and every Podman name in it is derived
    /// from this id.
    pub job_id: MachineId,
    /// What came of it.
    pub outcome: JobOutcome,
}

#[cfg(test)]
mod tests {
    use super::{
        Enrollment, HOST_FAMILY, HostFacts, HostState, HostView, JobOutcome, ReportJobResult,
    };
    use crate::id::{HostId, MachineId};
    use crate::machine::CpuArchitecture;

    /// Round-trips through the JSON *text*, which is the only form that
    /// proves a tagged document survives the wire.
    fn round_trip<T>(value: &T)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + core::fmt::Debug,
    {
        let json = serde_json::to_string(value).expect("serialize");
        let back: T = serde_json::from_str(&json)
            .unwrap_or_else(|error| panic!("{json} did not deserialize: {error}"));
        assert_eq!(&back, value);
    }

    fn facts() -> HostFacts {
        HostFacts {
            architecture: CpuArchitecture::Arm64,
            vcpus: 10,
            memory_mib: 32 * 1024,
            disk_free_gib: 400,
            podman_version: "5.4.0".to_owned(),
            kernel: "6.11.0-19-generic".to_owned(),
            hostname: "build.lexo.cool".to_owned(),
        }
    }

    fn view() -> HostView {
        HostView {
            id: HostId::generate(),
            label: "the build box".to_owned(),
            state: HostState::Online,
            facts: facts(),
            last_seen_unix: Some(1_800_000_000),
            created_at_unix: 1_799_000_000,
        }
    }

    #[test]
    fn a_host_view_round_trips() {
        round_trip(&view());
    }

    #[test]
    fn every_host_state_round_trips_as_its_wire_token() {
        for (state, token) in [
            (HostState::Online, "online"),
            (HostState::Offline, "offline"),
            (HostState::Draining, "draining"),
            (HostState::Removed, "removed"),
        ] {
            assert_eq!(
                serde_json::to_string(&state).expect("serialize"),
                format!("\"{token}\"")
            );
            round_trip(&state);
        }
        assert!(HostState::Online.is_schedulable());
        for state in [HostState::Offline, HostState::Draining, HostState::Removed] {
            assert!(!state.is_schedulable());
        }
    }

    #[test]
    fn an_enrollment_is_pending_or_a_machine() {
        let pending = Enrollment::Pending;
        assert_eq!(
            serde_json::to_value(&pending).expect("serialize")["status"],
            "pending"
        );
        round_trip(&pending);

        let enrolled = Enrollment::Enrolled { host: view() };
        assert_eq!(
            serde_json::to_value(&enrolled).expect("serialize")["status"],
            "enrolled"
        );
        round_trip(&enrolled);
    }

    #[test]
    fn a_job_result_round_trips_with_its_outcome_tag() {
        for outcome in [
            JobOutcome::Running {
                container: "flyco-9d0f".to_owned(),
                volume: "flyco-9d0f-work".to_owned(),
            },
            JobOutcome::Done,
            JobOutcome::Failed {
                message: "podman: no space left on device".to_owned(),
            },
        ] {
            let report = ReportJobResult {
                job_id: MachineId::generate(),
                outcome: outcome.clone(),
            };
            round_trip(&report);
            assert_eq!(
                serde_json::to_value(&report).expect("serialize")["outcome"]["outcome"],
                match outcome {
                    JobOutcome::Running { .. } => "running",
                    JobOutcome::Done => "done",
                    JobOutcome::Failed { .. } => "failed",
                }
            );
        }
    }

    #[test]
    fn only_a_finished_container_names_podman_resources() {
        assert_eq!(
            JobOutcome::Running {
                container: "flyco-9d0f".to_owned(),
                volume: "flyco-9d0f-work".to_owned(),
            }
            .names(),
            Some(("flyco-9d0f", "flyco-9d0f-work"))
        );
        assert_eq!(JobOutcome::Done.names(), None);
    }

    #[test]
    fn a_hosts_catalog_facts_come_from_what_it_measured() {
        let facts = facts();
        assert_eq!(facts.capacity().vcpus, 10);
        assert_eq!(facts.capacity().memory_mib, 32 * 1024);

        let lineage = facts.lineage();
        assert_eq!(lineage.architecture, CpuArchitecture::Arm64);
        assert_eq!(lineage.family, HOST_FAMILY);
        assert!(
            lineage.generation.is_none(),
            "a machine somebody owns is not a numbered generation of a product line"
        );
    }
}
