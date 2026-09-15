//! Cloud provider abstraction for flyco.
//!
//! Every cloud implementation is an HTTP client over the provider's public
//! API ([`http`], backed by zenwave natively and the Worker's `fetch` on
//! wasm32), never a native SDK — the same code runs in the Cloudflare Worker
//! (wasm32) and in tests: [`azure`], [`aws`] and [`gcp`]. Beside them is
//! [`host`], the machine the user owns, which is not a cloud at all.
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
//! A machine the user owns is not reachable at all: flyco never opens a
//! connection to it. The two shapes are told apart in the type system rather
//! than by a runtime check —
//!
//! * [`azure::AzureProvider`], [`aws::AwsProvider`] and [`gcp::GcpProvider`]
//!   speak only HTTPS, so they implement [`CloudProvider`] on every target —
//!   the Worker included.
//! * [`host::Host`] compiles everywhere and implements **no** provider
//!   trait. It *plans*: it turns a [`MachineOperation`] into a
//!   [`host::ContainerJob`], a serializable description of the container
//!   lifecycle work, which the control plane sends down that host's own
//!   command stream for `flycod host` to perform.
//!
//! So a Worker build contains no code that dials anybody's machine: there is
//! none in the crate.
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
mod login_key;
pub use login_key::{LoginKey, LoginKeyError};
pub mod clock;
pub mod cloud_init;
pub mod codespaces;
pub mod datetime;
pub mod flycod;
pub mod gcp;
pub mod host;
pub mod http;
pub mod naming;
pub mod polling;

use core::fmt;

use flyco_core::machine::{MachineCatalogEntry, MachineSpec, MachineState, SessionMachine};
use flyco_core::{BranchName, MachineId, MachineOrigin, PermissionMode, RepoSlug, SessionId};
use serde::{Deserialize, Serialize};

pub use clock::{MonotonicClock, SystemClock, SystemWallClock, WallClock};
pub use flycod::{ClaudeCredential, CodexCredential, DevinCredential, HarnessCredential};
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
    /// Provider-native resource identifier: an instance id, a VM resource
    /// id, the `job/execution` pair of a managed container, or the
    /// container name on a machine the user owns.
    pub native_id: String,
    /// Whether this machine is a virtual machine or a managed container.
    ///
    /// Carried rather than inferred from [`native_id`](Self::native_id),
    /// because it decides *which API a lifecycle call is even addressed
    /// to*: on Azure a stop is a `deallocate` action on a virtual machine
    /// and a `stop` on a Container Apps execution, and telling the two
    /// apart by the shape of an identifier would be a parser standing in
    /// for a fact the row already holds. It is the same runtime the
    /// [`MachineSpec`] asked for — a provider never answers a container
    /// with a virtual machine — which is why nothing downstream has to
    /// reconcile the two.
    pub runtime: flyco_core::Runtime,
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
    /// Which provider built the machine.
    ///
    /// The daemon needs it for exactly one thing, and nothing else on the
    /// machine can tell it: an eviction notice arrives on an
    /// instance-metadata endpoint whose address, headers and document are
    /// the provider's own, so a daemon that did not know whose machine it
    /// is on would have to probe three endpoints and guess. Written into
    /// the configuration only when the capacity is interruptible — see
    /// [`flycod::render`].
    pub provider: flyco_core::CloudProviderKind,
    /// Whether the machine is a virtual machine or a managed container.
    ///
    /// The daemon needs it for exactly one thing, and again nothing on the
    /// machine can tell it: whether the filesystem it is working on will
    /// still be there after this process stops. A VM's `SIGTERM` is a
    /// shutdown onto a disk that survives; a container's is the platform
    /// taking the working tree away in thirty seconds, so the daemon has to
    /// write the `workdir-patch` before it goes. See
    /// [`flyco_core::Runtime`].
    pub runtime: flyco_core::Runtime,
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
    /// The model the harness runs on, and the effort it runs at.
    ///
    /// Always concrete: `POST /v1/sessions` resolves the harness account's
    /// default when the caller names none, so the configuration on the
    /// machine states the model rather than leaving it to whichever version
    /// of the CLI the image happens to carry.
    pub model: flyco_core::ModelChoice,
    /// The user's enabled MCP servers, as the machine's harness is given
    /// them.
    ///
    /// Provisioned rather than fetched by the daemon, and it is the whole
    /// set: `flycod` writes these — and flyco's own local server — into the
    /// harness's root-owned MCP configuration, which is the allowlist the
    /// agent is held to. A server absent from this list is one the session
    /// cannot reach, and there is no route by which the agent adds one.
    pub mcp_servers: Vec<flyco_core::McpServerMount>,
}

impl fmt::Debug for DaemonBootstrap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DaemonBootstrap")
            .field("session", &self.session)
            .field("provider", &self.provider)
            .field("runtime", &self.runtime)
            .field("control_plane_url", &self.control_plane_url)
            .field("permission_mode", &self.permission_mode)
            .field("auth", &self.auth)
            .field("repo", &self.repo)
            .field("machine_origin", &self.machine_origin)
            .field("machine", &self.machine)
            .field("resume_session_id", &self.resume_session_id)
            .field("model", &self.model)
            // Names only: a remote MCP server's headers routinely carry a
            // bearer token, which is a fourth credential this structure
            // holds and the fourth this rendering keeps out of a log line.
            .field(
                "mcp_servers",
                &self
                    .mcp_servers
                    .iter()
                    .map(|server| server.name.as_str())
                    .collect::<Vec<_>>(),
            )
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
/// what [`host::Host::plan`] turns into container work without
/// performing any of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum MachineOperation {
    /// Bring a new machine into existence.
    ///
    /// Boxed for the reason [`host::ContainerJob::Create`] is: a
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

/// What a quota's numbers count.
///
/// Every provider states its compute quotas in one of these two, and which
/// one it is decides whether `allows 1` means one machine or sixty-four
/// cores. Carried as a field rather than assumed, because AWS states the
/// Mac families in whole hosts and everything else in vCPUs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaUnit {
    /// Virtual CPUs, the unit of every core quota.
    Vcpus,
    /// Whole dedicated hosts, which is how the Mac families are sold.
    Hosts,
}

impl QuotaUnit {
    /// The noun to put after a count, agreeing with it.
    #[must_use]
    pub const fn noun(self, count: u32) -> &'static str {
        match (self, count) {
            (Self::Vcpus, 1) => "vCPU",
            (Self::Vcpus, _) => "vCPUs",
            (Self::Hosts, 1) => "dedicated host",
            (Self::Hosts, _) => "dedicated hosts",
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
    ///
    /// The message is read by the person whose session just failed, so it
    /// says what the numbers count. `StandardDpsv6Family in westeurope
    /// allows 10 and 8 are in use` was three bare integers and a
    /// provider-internal identifier, and left a reader to guess whether the
    /// unit was machines, cores or dollars.
    ///
    /// The provider-native quota name stays, because it is the string the
    /// user types into Azure's or AWS's own increase request: the way out of
    /// this error runs through the provider's console, and a name flyco
    /// prettified would not be findable there.
    #[error(
        "the {quota} quota in {region} allows {limit} {unit} and {used} \
         are already in use, so there is no room for the {requested} this \
         machine needs",
        unit = unit.noun(*limit),
    )]
    QuotaExceeded {
        /// Provider-native name of the quota that binds.
        quota: String,
        /// Region the quota applies to.
        region: String,
        /// What the three numbers count.
        unit: QuotaUnit,
        /// The limit.
        limit: u32,
        /// How much of it is already used.
        used: u32,
        /// How much this request needs on top.
        requested: u32,
    },
    /// The requested machine type cannot be deployed into that region on
    /// this account at all.
    #[error("this account cannot start {machine_type} in {region}: {reason}")]
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
    /// The provider no longer holds the resource the request named.
    ///
    /// A 404 on a lifecycle call is this, not a rejection: a codespace
    /// past its retention is *deleted*, and starting it again will never
    /// work — the caller's move is to rebuild rather than retry, which is
    /// what carrying it as its own variant rather than inside
    /// [`Rejected`](Self::Rejected) lets it decide.
    #[error("the resource no longer exists: {0}")]
    Gone(String),
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

/// Where a driver had got to when its turn ran out.
///
/// A provision runs inside one control-plane invocation, and an invocation
/// has a budget: Cloudflare allows a Worker fifty subrequests on the free
/// plan, and a machine whose image is cold on the node can take a provider
/// six minutes of polling to bring up — a poll every ten seconds is the
/// budget spent before the machine exists (issue #257). So a driver that has
/// not finished when its polls run out hands back *where it got to*, the
/// control plane stores that in the queue message that resumes the build,
/// and [`CloudProvider::resume`] carries on under a fresh budget.
///
/// Opaque outside the driver that wrote it: a JSON document in a string,
/// so each driver keeps its own typed shape and nothing else reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Continuation(String);

impl Continuation {
    /// Writes a driver's own state down.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Malformed`] if the state cannot be
    /// serialized, which is a driver bug rather than a provider answer.
    pub fn write<T: Serialize>(state: &T) -> Result<Self, ProviderError> {
        serde_json::to_string(state).map(Self).map_err(|_| {
            ProviderError::Malformed("a provisioning continuation could not be written")
        })
    }

    /// Reads a driver's own state back.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Malformed`] for a continuation this driver
    /// did not write, which is a row from another build of the control
    /// plane rather than a state to recover from.
    pub fn read<T: serde::de::DeserializeOwned>(&self) -> Result<T, ProviderError> {
        serde_json::from_str(&self.0)
            .map_err(|_| ProviderError::Malformed("a provisioning continuation could not be read"))
    }
}

/// What a provision answered: the machine, or how far the provider got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provisioning {
    /// The machine exists and its daemon is on its way.
    Ready(Machine),
    /// The provider is still building it.
    ///
    /// `machine` is what is known so far — enough of an identity for the
    /// control plane to destroy it if the build is given up — and
    /// `continuation` is what [`CloudProvider::resume`] needs to carry on.
    Pending {
        /// The machine as far as it exists: its `native_id` names what the
        /// provider has created, and its state is still provisioning.
        machine: Machine,
        /// Where the driver got to.
        continuation: Continuation,
    },
}

impl Provisioning {
    /// The machine, for a caller that cannot carry a build across calls.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Rejected`] when the provider had not
    /// finished: the caller has nowhere to keep a continuation, so the
    /// build is refused rather than waited on past its budget.
    pub fn ready(self) -> Result<Machine, ProviderError> {
        match self {
            Self::Ready(machine) => Ok(machine),
            Self::Pending { .. } => Err(ProviderError::Rejected(
                "the provider had not finished building the machine within one call".to_owned(),
            )),
        }
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
    ///
    /// Answers [`Provisioning::Pending`] rather than waiting past one
    /// invocation's polling budget; the control plane calls
    /// [`resume`](Self::resume) later with what came back.
    fn provision(
        &mut self,
        request: &ProvisionRequest,
    ) -> impl Future<Output = Result<Provisioning, ProviderError>>;

    /// Carries on a provision that answered [`Provisioning::Pending`].
    ///
    /// `machine` is the one that answer carried. A driver whose provisions
    /// never answer pending refuses this with [`ProviderError::Malformed`]:
    /// there is nothing it could be asked to resume.
    fn resume(
        &mut self,
        machine: &Machine,
        continuation: &Continuation,
    ) -> impl Future<Output = Result<Provisioning, ProviderError>>;

    /// Changes the machine type in place, preserving the disk
    /// (stop → modify → start).
    ///
    /// On a [`Runtime::Container`](flyco_core::Runtime::Container) machine
    /// this is **stop, then start at the new size**, and that is a cheaper
    /// operation than the VM version rather than a degraded one: a job's
    /// size belongs to the execution, so there is nothing to modify between
    /// the two halves. What it costs instead is the filesystem — the
    /// execution that stops takes the working tree with it — which is why
    /// the stop writes the `workdir-patch` and the start replays it onto a
    /// fresh clone, exactly as an ordinary container start does.
    ///
    /// A driver with no size to change refuses with
    /// [`ProviderError::Unsupported`] rather than reporting a resize it did
    /// not perform: [`host::Host`] is the one such driver, because hardware
    /// the user owns has the cores it has.
    fn resize(
        &mut self,
        machine: &Machine,
        new_machine_type: &str,
    ) -> impl Future<Output = Result<Machine, ProviderError>>;

    /// Releases compute but keeps the disk (archive-pending, spot pause).
    ///
    /// "Keeps the disk" is a VM's promise. On a
    /// [`Runtime::Container`](flyco_core::Runtime::Container) machine there
    /// is no disk to keep: the definition survives and the filesystem does
    /// not, so what makes the session resumable is the `workdir-patch` its
    /// daemon wrote on the way out.
    fn deallocate(&mut self, machine: &Machine) -> impl Future<Output = Result<(), ProviderError>>;

    /// Puts a deallocated machine back on compute, on the same disk.
    ///
    /// On a [`Runtime::Container`](flyco_core::Runtime::Container) machine,
    /// on a fresh filesystem: a new execution of the same job, which clones
    /// the repository again and applies the stored patch on top.
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
    use super::{CapacityMode, DaemonBootstrap, ProviderError, QuotaUnit};
    use flyco_core::SessionId;

    #[test]
    fn a_refused_quota_reads_as_a_sentence_with_a_unit_in_it() {
        // What the session page showed before this: `Standard_D4ps_v6 is not
        // available to this account in westeurope: StandardDpsv6Family in
        // westeurope allows 10 and 8 are in use, which does not cover 4
        // more.` Three bare integers counting nothing a reader could name.
        let quota = ProviderError::QuotaExceeded {
            quota: "StandardDpsv6Family".to_owned(),
            region: "westeurope".to_owned(),
            unit: QuotaUnit::Vcpus,
            limit: 10,
            used: 8,
            requested: 4,
        };
        assert_eq!(
            quota.to_string(),
            "the StandardDpsv6Family quota in westeurope allows 10 vCPUs and 8 \
             are already in use, so there is no room for the 4 this machine needs"
        );

        let unavailable = ProviderError::Unavailable {
            machine_type: "Standard_D4ps_v6".to_owned(),
            region: "westeurope".to_owned(),
            reason: quota.to_string(),
        };
        assert!(
            unavailable
                .to_string()
                .starts_with("this account cannot start Standard_D4ps_v6 in westeurope: the ")
        );
    }

    #[test]
    fn a_quota_counted_in_hosts_does_not_claim_to_count_cores() {
        // AWS states the Mac families in whole dedicated hosts, so the unit
        // is carried rather than assumed — and it agrees with its count.
        assert_eq!(QuotaUnit::Hosts.noun(1), "dedicated host");
        assert_eq!(QuotaUnit::Hosts.noun(2), "dedicated hosts");
        assert_eq!(QuotaUnit::Vcpus.noun(1), "vCPU");
        assert_eq!(QuotaUnit::Vcpus.noun(4), "vCPUs");
    }

    #[test]
    fn a_bootstrap_never_debug_prints_the_credentials_it_carries() {
        let bootstrap = DaemonBootstrap {
            session: SessionId::generate(),
            provider: flyco_core::CloudProviderKind::Azure,
            runtime: flyco_core::Runtime::Vm,
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
            model: crate::testing::session_model(),
            mcp_servers: vec![flyco_core::McpServerMount {
                name: "deepwiki".to_owned(),
                config: flyco_core::McpServerConfig::Http {
                    url: "https://mcp.deepwiki.com/mcp".to_owned(),
                    headers: vec![flyco_core::HeaderEntry {
                        name: "authorization".to_owned(),
                        value: "Bearer a-live-mcp-credential".to_owned(),
                    }],
                },
            }],
        };

        let rendered = format!("{bootstrap:?}");
        assert!(!rendered.contains("fd_a-live-credential"));
        assert!(!rendered.contains("sk-ant-oat01-live"));
        assert!(!rendered.contains(crate::testing::GITHUB_TOKEN));
        assert!(!rendered.contains("a-live-mcp-credential"));
        assert!(rendered.contains("https://flyco.dev/"));
        assert!(rendered.contains("deepwiki"));
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
