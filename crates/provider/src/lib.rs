//! Cloud provider abstraction for flyco.
//!
//! Every implementation is an HTTP client over the provider's public API
//! ([`http`], backed by zenwave natively and the Worker's `fetch` on wasm32), never a native SDK — the same code runs
//! in the Cloudflare Worker (wasm32) and in tests: [`byo_ssh`], [`azure`],
//! [`aws`] and [`gcp`].
//!
//! Where two drivers need the same decision they share it rather than each
//! having one: [`cloud_init`] is the document every provisioned machine
//! boots, [`polling`] is how long to wait before asking again, and
//! [`datetime`] turns an instant into the strings a provider's API asks for.
//!
//! # Where a driver runs
//!
//! All three cloud drivers are reachable from the Worker because every step
//! of them is an HTTPS call — an AWS signature and a Google assertion are
//! both computed in-process rather than by an SDK, which is what keeps it so.
//! byo-ssh is not: SSH is a TCP transport and a Worker has no sockets. The
//! two shapes are told apart in the type system rather than by a runtime
//! check —
//!
//! * [`azure::AzureProvider`], [`aws::AwsProvider`] and [`gcp::GcpProvider`]
//!   speak only HTTPS, so they implement [`CloudProvider`] on every target —
//!   the Worker included.
//! * [`byo_ssh::ByoSsh`] compiles everywhere but implements **no** provider
//!   trait. It *plans*: it turns a machine operation into a
//!   [`byo_ssh::ContainerJob`], a serializable description of the container
//!   lifecycle work, which the control plane enqueues.
//! * `byo_ssh::SshExecutor` exists only behind the native `ssh` feature and
//!   is what implements [`CloudProvider`] for byo-ssh, by performing a
//!   `ContainerJob` over a real SSH connection.
//!
//! So a Worker build cannot accidentally call SSH code: there is none in it.
//!
//! # Why the trait takes `&mut self`
//!
//! A driver is stateful. Azure caches an access token and must be able to
//! replace it — at 80% of its lifetime, and on any 401. Expressing that with
//! `&self` would need interior mutability, which on a `Send` future means a
//! lock, and flyco does not put a lock on a hot path to model a field that is
//! only ever touched by its owner. `&mut self` says the same thing with the
//! borrow checker and costs nothing.

pub mod aws;
pub mod azure;
pub mod byo_ssh;
pub mod clock;
pub mod cloud_init;
pub mod datetime;
pub mod flycod;
pub mod gcp;
pub mod http;
pub mod naming;
pub mod polling;

use core::fmt;

use flyco_core::machine::{MachineCatalogEntry, MachineSpec, MachineState, SessionMachine};
use flyco_core::{BranchName, MachineId, MachineOrigin, PermissionMode, RepoSlug, SessionId};
use serde::{Deserialize, Serialize};

pub use clock::{MonotonicClock, SystemClock, SystemWallClock, WallClock};
pub use flycod::{ClaudeCredential, CodexCredential, HarnessCredential};
pub use http::{HttpError, HttpRequest, HttpResponse, HttpTransport, LiveTransport};

/// Which capacity market a running machine actually holds.
///
/// Recorded rather than assumed: a spot request that a provider cannot
/// honour is retried as on-demand, and the price flyco bills follows what
/// was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityMode {
    /// Interruptible capacity, at the spot price.
    Spot,
    /// Ordinary capacity, at the on-demand price.
    OnDemand,
}

impl CapacityMode {
    /// Whether this is spot capacity.
    #[must_use]
    pub const fn is_spot(self) -> bool {
        matches!(self, Self::Spot)
    }
}

/// A provisioned machine as the provider reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    /// Flyco's identifier for the machine.
    pub id: MachineId,
    /// Provider-native resource identifier (instance id, VM resource id,
    /// or container id for byo-ssh).
    pub native_id: String,
    /// Provider-native region the machine lives in.
    ///
    /// Carried rather than derived: every later operation needs it — an EC2
    /// call is addressed to a regional endpoint, an ARM resize re-checks the
    /// region's quota — and the alternatives are all worse. Reading it back
    /// off a public DNS name works until a provider spells a region
    /// differently in one (`us-east-1` is `compute-1` in an EC2 hostname),
    /// and a machine with no address yet would have nowhere to keep it.
    pub region: String,
    /// Current lifecycle state.
    pub state: MachineState,
    /// Which capacity market it actually holds.
    pub capacity_mode: CapacityMode,
    /// Address the daemon bootstrap reaches it on, once known.
    pub address: Option<String>,
}

/// Who a session's commits are authored as.
///
/// Flyco's default is to behave *as the user* rather than as a bot: the
/// agent pushes with the user's own GitHub authorization, so the commits it
/// writes must carry the user's own name and address or the history would
/// disagree with the account that produced it.
///
/// The address is GitHub's own `id+login@users.noreply.github.com` form,
/// which is what GitHub itself attributes web edits to — it maps to the
/// account without publishing a private address flyco has no right to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitIdentity {
    /// `user.name` for the checkout.
    pub name: String,
    /// `user.email` for the checkout.
    pub email: String,
}

/// The repository a session's machine checks out before its agent starts.
///
/// Carried as a structure rather than as environment strings the daemon
/// would have to re-parse: the slug and the branch are already types by the
/// time the control plane has them, and a machine is not the place to
/// discover that one of them was never a repository.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoCheckout {
    /// The repository, `owner/name`.
    pub slug: RepoSlug,
    /// The branch to check out.
    pub branch: BranchName,
    /// The user's own GitHub token, which is what "behave as the user"
    /// means in practice: the clone and any later push are the user's, not a
    /// flyco bot's.
    ///
    /// Kept out of [`fmt::Debug`] and never written into a remote URL — the
    /// daemon feeds it to git through a credential helper that reads it from
    /// the environment of that one child process.
    pub token: String,
    /// Who the checkout's commits are authored as.
    pub identity: GitIdentity,
}

impl fmt::Debug for RepoCheckout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RepoCheckout")
            .field("slug", &self.slug)
            .field("branch", &self.branch)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

/// Everything a provisioned machine's `flycod` needs to come up already
/// paired with its session.
///
/// Three of these fields are live credentials — the daemon token, the
/// harness credential inside [`auth`](Self::auth), and the GitHub token
/// inside [`repo`](Self::repo) — and all three travel inside
/// cloud-init documents and container environments, which are exactly the
/// values a driver is tempted to trace. The hand-written [`fmt::Debug`] is
/// what keeps them out of a log line.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonBootstrap {
    /// The session this machine serves.
    pub session: SessionId,
    /// Base URL of the control plane, e.g. `https://flyco.dev/`.
    pub control_plane_url: String,
    /// The session's `fd_` daemon token.
    pub daemon_token: String,
    /// Permission mode the harness runs under.
    pub permission_mode: PermissionMode,
    /// How the supervised harness authenticates, which is also which
    /// harness the daemon drives.
    pub auth: HarnessCredential,
    /// The repository to check out before the harness starts.
    pub repo: RepoCheckout,
    /// Whether flyco or the user chose the machine this session runs on.
    ///
    /// The agent is told, and told what it means: a machine the user picked
    /// is not one to trade away for a faster build (docs/ux.md §9.5). It is
    /// a fact about the *session* rather than about the machine, so it
    /// survives every resize the session goes through.
    pub machine_origin: MachineOrigin,
    /// The machine being provisioned, as the agent is told about it.
    ///
    /// What the daemon states in the notice it injects at session start:
    /// the type, the rate, whether the capacity is interruptible, how big it
    /// is, and — the fact nothing else carries — whether booting it already
    /// committed the user to a licence minimum.
    pub machine: SessionMachine,
    /// Harness-native session id to resume, for a session moving onto a new
    /// machine.
    pub resume_session_id: Option<String>,
}

impl fmt::Debug for DaemonBootstrap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DaemonBootstrap")
            .field("session", &self.session)
            .field("control_plane_url", &self.control_plane_url)
            .field("permission_mode", &self.permission_mode)
            .field("auth", &self.auth)
            .field("repo", &self.repo)
            .field("machine_origin", &self.machine_origin)
            .field("machine", &self.machine)
            .field("resume_session_id", &self.resume_session_id)
            .finish_non_exhaustive()
    }
}

/// Everything one provisioning call needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvisionRequest {
    /// Flyco's identifier for the machine about to exist, which is also how
    /// its provider-native resources are named.
    pub machine: MachineId,
    /// What to provision.
    pub spec: MachineSpec,
    /// How its daemon phones home.
    pub bootstrap: DaemonBootstrap,
}

/// One thing a caller wants done to one machine.
///
/// Named as data rather than as a method call because it is what the control
/// plane puts on a queue: a Worker decides *what* should happen, and the
/// executor that can actually reach the provider decides *when*. It is also
/// what [`byo_ssh::ByoSsh::plan`] turns into container work without
/// performing any of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum MachineOperation {
    /// Bring a new machine into existence.
    ///
    /// Boxed for the reason [`byo_ssh::ContainerJob::Create`] is: a
    /// provisioning request carries the whole daemon bootstrap — three
    /// credentials, a repository, a commit identity and the machine the
    /// session is on — while every other operation here is a machine and a
    /// string. Without the indirection each `Destroy` would be padded out
    /// to the size of a `Provision`. The JSON is unchanged: a `Box`
    /// serializes as what it holds.
    Provision(Box<ProvisionRequest>),
    /// Move an existing machine to another type, keeping its disk.
    Resize {
        /// The machine to change.
        machine: Machine,
        /// Provider-native machine type to move to.
        machine_type: String,
    },
    /// Release compute, keep the disk.
    Deallocate {
        /// The machine to stop.
        machine: Machine,
    },
    /// Put a deallocated machine back on compute.
    Start {
        /// The machine to start.
        machine: Machine,
    },
    /// Release compute and disk.
    Destroy {
        /// The machine to remove.
        machine: Machine,
    },
}

impl MachineOperation {
    /// The operation's name, for a log line or an error message.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Provision(_) => "provision",
            Self::Resize { .. } => "resize",
            Self::Deallocate { .. } => "deallocate",
            Self::Start { .. } => "start",
            Self::Destroy { .. } => "destroy",
        }
    }

    /// Which machine it acts on.
    #[must_use]
    pub const fn machine(&self) -> MachineId {
        match self {
            Self::Provision(request) => request.machine,
            Self::Resize { machine, .. }
            | Self::Deallocate { machine }
            | Self::Start { machine }
            | Self::Destroy { machine } => machine.id,
        }
    }
}

/// An error from a provider operation.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// The provider's API rejected the request, without a code to act on.
    #[error("provider rejected the request: {0}")]
    Rejected(String),
    /// The provider refused, naming a machine-readable error code.
    ///
    /// Separate from [`Rejected`](Self::Rejected) because some codes are
    /// instructions: Azure's `AzureSpotIsNotSupportedForThisVMSize` means
    /// "ask again without the spot fields", and a driver can only act on
    /// that if the code survives as a field rather than as prose.
    #[error("{code}: {message}")]
    Refused {
        /// Provider-native error code.
        code: String,
        /// Provider-native message.
        message: String,
    },
    /// The provider has no capacity for the requested spec.
    #[error("no capacity for the requested machine type: {0}")]
    NoCapacity(String),
    /// The account's quota does not cover the request, so it was refused
    /// before it was attempted.
    #[error(
        "{quota} in {region} allows {limit} and {used} are in use, \
         which does not cover {requested} more"
    )]
    QuotaExceeded {
        /// Provider-native name of the quota that binds.
        quota: String,
        /// Region the quota applies to.
        region: String,
        /// The limit.
        limit: u32,
        /// How much of it is already used.
        used: u32,
        /// How much this request needs on top.
        requested: u32,
    },
    /// The requested machine type cannot be deployed into that region on
    /// this account at all.
    #[error("{machine_type} is not available to this account in {region}: {reason}")]
    Unavailable {
        /// Machine type that was asked for.
        machine_type: String,
        /// Region it was asked for in.
        region: String,
        /// What the provider said about it.
        reason: String,
    },
    /// The provider does not implement this operation, and no amount of
    /// retrying will change that.
    ///
    /// Distinct from a rejection: a caller can offer the user another route
    /// (recreate the machine, say) instead of surfacing a transient-looking
    /// failure.
    #[error("{provider} does not support {operation}: {reason}")]
    Unsupported {
        /// Which driver refused.
        provider: &'static str,
        /// The operation it does not implement.
        operation: &'static str,
        /// Why, in terms the user can act on.
        reason: &'static str,
    },
    /// An asynchronous operation finished in a terminal failure state.
    #[error("the provider's operation ended as {status}: {code} — {message}")]
    OperationFailed {
        /// Terminal status the provider reported.
        status: String,
        /// Provider-native error code, or an empty string when it gave none.
        code: String,
        /// Provider-native message.
        message: String,
    },
    /// The provider answered something this driver cannot make sense of.
    #[error("the provider's response was not usable: {0}")]
    Malformed(&'static str),
    /// Transport-level failure talking to the provider.
    #[error("transport error: {0}")]
    Transport(#[from] HttpError),
}

impl ProviderError {
    /// The provider-native error code, when the failure carries one.
    ///
    /// A driver reads this to decide whether a refusal is an instruction —
    /// "retry without spot" — rather than a dead end.
    #[must_use]
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Refused { code, .. } | Self::OperationFailed { code, .. } => Some(code),
            _ => None,
        }
    }

    /// Whether making the identical request again could succeed.
    ///
    /// Exactly the failures that are about the *conversation* rather than
    /// about the request: a connection that did not open, a response that
    /// never arrived. Everything else is the provider answering — no quota,
    /// no such machine type here, this operation is not implemented — and
    /// repeating the request would produce the same answer, more slowly, and
    /// leave the caller waiting for a machine that is never coming.
    ///
    /// A [`Transport`](Self::Transport) failure carrying an
    /// [`HttpError::Encoding`] or [`HttpError::Decoding`] is not transient
    /// either: those are flyco's own bug or the provider's, and neither
    /// changes on a second attempt.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        matches!(self, Self::Transport(HttpError::Transport(_)))
    }
}

/// A compute provider flyco can provision session machines on.
///
/// Object-unsafe by design: the control plane matches on
/// [`flyco_core::machine::CloudProviderKind`] and calls the concrete
/// implementation, keeping every future free of boxing on wasm32.
///
/// Every method takes `&mut self` — see the crate documentation for why.
pub trait CloudProvider {
    /// The machine types this provider currently offers, with live pricing.
    fn catalog(&mut self) -> impl Future<Output = Result<Vec<MachineCatalogEntry>, ProviderError>>;

    /// Provisions a machine for a session, already carrying its daemon's
    /// credentials.
    fn provision(
        &mut self,
        request: &ProvisionRequest,
    ) -> impl Future<Output = Result<Machine, ProviderError>>;

    /// Changes the machine type in place, preserving the disk
    /// (stop → modify → start).
    fn resize(
        &mut self,
        machine: &Machine,
        new_machine_type: &str,
    ) -> impl Future<Output = Result<Machine, ProviderError>>;

    /// Releases compute but keeps the disk (archive-pending, spot pause).
    fn deallocate(&mut self, machine: &Machine) -> impl Future<Output = Result<(), ProviderError>>;

    /// Puts a deallocated machine back on compute, on the same disk.
    fn start(&mut self, machine: &Machine) -> impl Future<Output = Result<Machine, ProviderError>>;

    /// Releases compute and disk. Irreversible.
    fn destroy(&mut self, machine: &Machine) -> impl Future<Output = Result<(), ProviderError>>;
}

/// Recorded HTTP exchanges and a timer that never waits, so anything built
/// on [`HttpTransport`] can be tested without a cloud account.
///
/// Behind a feature rather than always on: it is test scaffolding, and the
/// Worker build must not carry it. The control plane enables it as a
/// dev-dependency, because the Anthropic OAuth client it runs is an
/// [`HttpTransport`] caller like every driver here.
#[cfg(any(test, feature = "testing"))]
pub mod testing;

#[cfg(test)]
mod tests {
    use super::{CapacityMode, DaemonBootstrap};
    use flyco_core::SessionId;

    #[test]
    fn a_bootstrap_never_debug_prints_the_credentials_it_carries() {
        let bootstrap = DaemonBootstrap {
            session: SessionId::generate(),
            control_plane_url: "https://flyco.dev/".to_owned(),
            daemon_token: "fd_a-live-credential".to_owned(),
            permission_mode: flyco_core::PermissionMode::Default,
            auth: crate::HarnessCredential::ClaudeCode(crate::ClaudeCredential::OauthToken {
                token: "sk-ant-oat01-live".to_owned(),
            }),
            repo: crate::testing::checkout(),
            machine_origin: flyco_core::MachineOrigin::Auto,
            machine: crate::testing::session_machine(),
            resume_session_id: None,
        };

        let rendered = format!("{bootstrap:?}");
        assert!(!rendered.contains("fd_a-live-credential"));
        assert!(!rendered.contains("sk-ant-oat01-live"));
        assert!(!rendered.contains(crate::testing::GITHUB_TOKEN));
        assert!(rendered.contains("https://flyco.dev/"));
    }

    #[test]
    fn capacity_mode_round_trips_as_its_wire_token() {
        assert_eq!(
            serde_json::to_string(&CapacityMode::OnDemand).expect("serialize"),
            "\"on_demand\""
        );
        assert!(CapacityMode::Spot.is_spot());
    }
}
