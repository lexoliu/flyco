//! The single error type the auth handlers return.
//!
//! Every variant knows the RFC 9457 document it renders as, so a client sees
//! one error shape across the whole API. Server-side failures deliberately
//! describe themselves only in the log: the response says the status and
//! nothing that would leak internals.

use flyco_core::workdir::{FILE_BYTES_MAX, WorkdirRefusal};
use flyco_core::{ApprovalState, Problem, ProblemExtensions, RepoSlug, SessionState};
use skyzen::{Response, StatusCode};
use skyzen_services::queue::QueueError;
use skyzen_services::{DbError, KvError, StorageError};

use crate::anthropic::AnthropicError;
use crate::crypto::CryptoError;
use crate::github::{GithubCall, GithubError};
use crate::google::GoogleError;
use crate::microsoft::MicrosoftError;
use crate::openai::{DEVICE_AUTH_SETTINGS_URL, OpenAiError};
use crate::problem::{self, Challenge};

/// Detail returned for any failure that is flyco's fault rather than the
/// caller's.
const SERVER_DETAIL: &str = "The control plane failed to handle this request.";

impl From<WorkdirRefusal> for ApiError {
    /// The daemon's refusal, as the RFC 9457 problem the browser sees.
    ///
    /// One arm per refusal, deliberately: the whole reason the daemon
    /// answers with a typed refusal rather than an error string is that the
    /// UI says something different for each — a binary file offers the
    /// terminal, a file that is too large quotes both sizes, and a path
    /// outside the checkout is a `400` rather than a `404`, because it is a
    /// bug in the caller and not a state of the disk.
    fn from(refusal: WorkdirRefusal) -> Self {
        match refusal {
            WorkdirRefusal::NotFound { path } => Self::PathNotFound { path },
            WorkdirRefusal::OutsideCheckout { path } => Self::PathOutsideCheckout { path },
            WorkdirRefusal::NotADirectory { path } => Self::PathNotADirectory { path },
            WorkdirRefusal::NotAFile { path } => Self::PathNotAFile { path },
            WorkdirRefusal::NotText { path } => Self::FileNotText { path },
            WorkdirRefusal::TooLarge { path, bytes } => Self::FileTooLarge {
                path,
                bytes,
                limit: FILE_BYTES_MAX,
            },
            WorkdirRefusal::NoBaseBranch => Self::NoBaseBranch,
            WorkdirRefusal::Unreadable { detail } => Self::WorkdirUnreadable(detail),
        }
    }
}

/// Every way an auth request can fail.
#[skyzen::error(status = StatusCode::INTERNAL_SERVER_ERROR)]
pub enum ApiError {
    /// The request carried no `Authorization` header.
    #[error("no credential was presented", status = StatusCode::UNAUTHORIZED)]
    MissingCredential,

    /// The presented bearer token is unknown, expired, or not a flyco token.
    #[error("the presented credential is not valid", status = StatusCode::UNAUTHORIZED)]
    InvalidCredential,

    /// The `state` echoed back by GitHub is unknown, already consumed, or
    /// past its ten-minute lifetime.
    #[error("the OAuth `state` parameter is unknown or expired", status = StatusCode::BAD_REQUEST)]
    UnknownOauthState,

    /// The caller asked to revoke a key that is not theirs, or does not exist.
    #[error("api key not found", status = StatusCode::NOT_FOUND)]
    ApiKeyNotFound,

    /// The session does not exist, or belongs to somebody else. The two are
    /// deliberately indistinguishable.
    #[error("session not found", status = StatusCode::NOT_FOUND)]
    SessionNotFound,

    /// The requested public execution-plane artifact is not published.
    #[error("release artifact not found", status = StatusCode::NOT_FOUND)]
    ReleaseArtifactNotFound,

    /// The approval does not exist, or belongs to somebody else.
    #[error("approval not found", status = StatusCode::NOT_FOUND)]
    ApprovalNotFound,

    /// The memory node does not exist, or belongs to somebody else.
    #[error("memory node not found", status = StatusCode::NOT_FOUND)]
    MemoryNodeNotFound,

    /// The MCP server does not exist, or belongs to somebody else.
    #[error("MCP server not found", status = StatusCode::NOT_FOUND)]
    McpServerNotFound,

    /// The caller already registered a server under this name.
    ///
    /// The name is what the harness announces the server under, so two
    /// answering to one name collide on the machine.
    #[error(
        "you already registered an MCP server called {name}",
        status = StatusCode::CONFLICT
    )]
    McpServerNameTaken {
        /// The name already in use.
        name: String,
    },

    /// The submitted server definition is unusable.
    #[error("this MCP server definition is unusable: {0}", status = StatusCode::UNPROCESSABLE_ENTITY)]
    InvalidMcpServer(&'static str),

    /// The skill does not exist, or belongs to somebody else.
    #[error("skill not found", status = StatusCode::NOT_FOUND)]
    SkillNotFound,

    /// The uploaded bundle is not one flyco can install.
    #[error("this skill bundle is unusable: {0}", status = StatusCode::UNPROCESSABLE_ENTITY)]
    InvalidSkill(&'static str),

    /// The delivery carried no `X-Hub-Signature-256`, or one that does not
    /// match the body.
    ///
    /// Deliberately one variant for both: telling a forger which half they
    /// got wrong is a free oracle, and neither answer is actionable by
    /// GitHub, which retries a delivery flyco could not verify.
    #[error(
        "this delivery is not signed by the secret this deployment holds",
        status = StatusCode::FORBIDDEN
    )]
    WebhookUnverified,

    /// The delivery was signed but its body is not the document its event
    /// header claims.
    #[error(
        "this `{event}` delivery is not the payload that event carries",
        status = StatusCode::BAD_REQUEST
    )]
    WebhookMalformed {
        /// The event the delivery announced itself as.
        event: String,
    },

    /// The push subscription does not exist, or belongs to somebody else.
    #[error("push subscription not found", status = StatusCode::NOT_FOUND)]
    PushSubscriptionNotFound,

    /// The browser sent a subscription flyco cannot use.
    #[error("this push subscription is unusable: {0}", status = StatusCode::UNPROCESSABLE_ENTITY)]
    InvalidPushSubscription(&'static str),

    /// A subscription could not be encoded or delivered to its push service.
    #[error("web push delivery failed: {0}", status = StatusCode::BAD_GATEWAY)]
    PushDeliveryFailed(String),

    /// The provider account does not exist, or belongs to somebody else.
    #[error("provider account not found", status = StatusCode::NOT_FOUND)]
    ProviderAccountNotFound,

    /// The submitted harness credential cannot be stored or used.
    #[error("this harness credential is unusable: {0}", status = StatusCode::UNPROCESSABLE_ENTITY)]
    InvalidHarnessCredential(&'static str),

    /// The harness account does not exist, or belongs to somebody else.
    #[error("harness account not found", status = StatusCode::NOT_FOUND)]
    HarnessAccountNotFound,

    /// Unlinking would leave running sessions with no credential to renew.
    ///
    /// The account is what a session's daemon refreshes its token against,
    /// and it is refreshed where it is *used* — so a session whose agent
    /// runs on this harness keeps working right up to the moment its grant
    /// expires, and then stops with nothing to explain it. The refusal
    /// happens here instead, while there is still something the user can do
    /// about it.
    #[error(
        "{sessions} session(s) still run on this account; archive them before unlinking",
        status = StatusCode::CONFLICT
    )]
    HarnessAccountInUse {
        /// How many non-archived sessions run on this harness.
        ///
        /// Repeated as the `active_sessions` extension member of the problem
        /// document, so the confirmation a browser puts up states the count
        /// without reading it back out of this sentence.
        sessions: u32,
    },

    /// The Claude sign-in attempt this code names is unknown, already
    /// redeemed, past its ten-minute lifetime, or another user's.
    ///
    /// Deliberately one variant for all four: the attempt id is the only
    /// thing that names an attempt, and telling a caller which of the four
    /// their id is would be a free oracle over somebody else's sign-in.
    #[error(
        "this Claude sign-in has expired or was already completed; start it again",
        status = StatusCode::BAD_REQUEST
    )]
    ClaudeOauthAttemptExpired,

    /// The `state` pasted alongside the code is not the one this attempt
    /// was started with.
    #[error(
        "this code belongs to a different Claude sign-in than the one it was pasted into",
        status = StatusCode::BAD_REQUEST
    )]
    ClaudeOauthStateMismatch,

    /// Anthropic refused the grant, and said why.
    ///
    /// A caller error rather than an outage: the code was mistyped, already
    /// used, or expired, and the answer is to run the flow again.
    #[error("Anthropic refused this Claude sign-in: {reason}", status = StatusCode::UNPROCESSABLE_ENTITY)]
    ClaudeOauthRejected {
        /// Anthropic's own error code and description.
        reason: String,
    },

    /// The Codex sign-in this poll names is unknown, already completed,
    /// past its fifteen-minute lifetime, or another user's.
    ///
    /// One variant for all four, for the same reason
    /// [`ClaudeOauthAttemptExpired`](Self::ClaudeOauthAttemptExpired) is
    /// one: the attempt id is the only thing that names an attempt, and
    /// telling a caller which of the four theirs is would be a free oracle
    /// over somebody else's sign-in.
    #[error(
        "this Codex sign-in has expired or was already completed; start it again",
        status = StatusCode::BAD_REQUEST
    )]
    CodexOauthAttemptExpired,

    /// `OpenAI` will not start a device sign-in for this account.
    ///
    /// Device code authorization is off by default — it is more open to
    /// social engineering than a browser redirect — and only the account's
    /// owner, or a workspace admin, can switch it on. The detail names the
    /// page where they do it, because that is the entire fix.
    #[error(
        "OpenAI will not start a device sign-in for this account. Turn on device code \
         authorization in your ChatGPT security settings at {settings_url} — on a workspace \
         account a workspace admin does it — and try again.",
        status = StatusCode::CONFLICT
    )]
    CodexDeviceAuthDisabled {
        /// Where the switch is, so the problem document carries the fix
        /// rather than describing it.
        settings_url: &'static str,
    },

    /// `OpenAI` refused the grant, and said why.
    #[error("OpenAI refused this Codex sign-in: {reason}", status = StatusCode::UNPROCESSABLE_ENTITY)]
    CodexOauthRejected {
        /// `OpenAI`'s own error code and description.
        reason: String,
    },

    /// The cloud sign-in this call names is unknown, already finished, past
    /// its ten-minute lifetime, another vendor's, or another user's.
    ///
    /// One variant for all five, for the reason
    /// [`CodexOauthAttemptExpired`](Self::CodexOauthAttemptExpired) is one:
    /// the attempt id is the only thing that names an attempt, and telling a
    /// caller which of the five theirs is would be a free oracle over
    /// somebody else's sign-in.
    #[error(
        "this cloud sign-in has expired or was already completed; start it again",
        status = StatusCode::NOT_FOUND
    )]
    ProviderOauthAttemptExpired,

    /// The sign-in exists, but the browser has not come back from the vendor
    /// yet, so there is nothing to link.
    ///
    /// A state rather than a mistake: the page polls until the callback has
    /// landed, and finishing before that is a request made too early.
    #[error(
        "this cloud sign-in has not come back from the provider yet",
        status = StatusCode::CONFLICT
    )]
    ProviderOauthNotAuthorized,

    /// Microsoft refused, and said why.
    ///
    /// A caller error rather than an outage: consent was declined, the code
    /// expired, or the account may not do what flyco asked of it.
    #[error("Microsoft refused this sign-in: {reason}", status = StatusCode::UNPROCESSABLE_ENTITY)]
    MicrosoftRejected {
        /// Microsoft's own code and description.
        reason: String,
    },

    /// Google refused, and said why.
    #[error("Google refused this sign-in: {reason}", status = StatusCode::UNPROCESSABLE_ENTITY)]
    GoogleRejected {
        /// Google's own status and message.
        reason: String,
    },

    /// GitHub refused a Codespaces link sign-in, and said why.
    ///
    /// The same refusal [`MicrosoftRejected`](Self::MicrosoftRejected) is,
    /// on the one provider whose sign-in is OAuth rather than an issued
    /// credential: consent declined, the code expired, or the granted
    /// scopes do not cover what a codespace needs.
    #[error("GitHub refused this sign-in: {reason}", status = StatusCode::UNPROCESSABLE_ENTITY)]
    GithubRejected {
        /// GitHub's own reason, or which required scope was not granted.
        reason: String,
    },

    /// The machine exists but the provider has not named it yet.
    ///
    /// It is still being created, so there is nothing to act on. Distinct
    /// from a missing machine: retrying later succeeds.
    #[error(
        "this session's machine is still being created",
        status = StatusCode::CONFLICT
    )]
    MachineNotReady,

    /// The provider refused or could not complete the operation.
    #[error("the provider could not complete this: {0}", status = StatusCode::BAD_GATEWAY)]
    Provisioning(String),

    /// The linked account cannot deploy the machine the caller asked for.
    ///
    /// Checked against the account's own catalog while the session is being
    /// created, so an impossible choice is refused where it was made rather
    /// than two minutes later inside a queue consumer. The detail is the
    /// provider's own reason, which is what tells the three answers apart:
    /// this machine type is not sold to you here, your quota does not cover
    /// it, or your subscription's policy forbids the region outright.
    #[error("{0}", status = StatusCode::UNPROCESSABLE_ENTITY)]
    MachineUnavailable(String),

    /// The caller named a machine type and a runtime the catalog does not
    /// pair.
    ///
    /// `400` rather than the `422` beside it, and the difference is which
    /// half is wrong: [`MachineUnavailable`](Self::MachineUnavailable) is a
    /// well-formed choice this account cannot honour, while this is a
    /// request that contradicts itself — the type is on offer, and the
    /// runtime sent with it is not the runtime it is offered as. That
    /// happens when a picker is working from a catalog that has since
    /// changed, and the fix is to read it again rather than to link
    /// anything or raise a quota.
    ///
    /// Never resolved in the caller's favour by taking the runtime on
    /// offer. A session that asked for a virtual machine asked for a disk
    /// that survives a stop, and a container would lose its working tree
    /// the first time the platform stopped it.
    #[error(
        "`{machine_type}` is offered as a {offered:?} and this request asks for it          as a {requested:?}; read the catalog again",
        status = StatusCode::BAD_REQUEST
    )]
    MachineRuntimeMismatch {
        /// The type both halves name.
        machine_type: String,
        /// What the request asked for.
        requested: flyco_core::Runtime,
        /// What the catalog offers it as.
        offered: flyco_core::Runtime,
    },

    /// The session has not been given a machine yet.
    ///
    /// Distinct from a destroyed one: nothing was ever provisioned.
    #[error("this session has no machine", status = StatusCode::NOT_FOUND)]
    MachineNotFound,

    /// A codespace asked for a daemon configuration, and no flyco machine
    /// goes by the name it presented.
    ///
    /// `404` rather than a refusal: the provisioning leg writes the codespace's
    /// name to the row after GitHub starts the machine, so a bootstrap that
    /// arrives in that window is told the truth — nothing answers to this
    /// name *yet* — and the entrypoint retries until the row lands.
    #[error("no flyco machine is a codespace of this name", status = StatusCode::NOT_FOUND)]
    CodespacesMachineUnknown,

    /// A codespace asked for a daemon configuration with a `GITHUB_TOKEN`
    /// that cannot read the private environment repository the session's
    /// account provisions on.
    ///
    /// That read is the whole authentication of the bootstrap endpoint: a
    /// codespace's injected token is scoped to exactly that repository, so
    /// a token GitHub turns away from it is not a credential flyco answers
    /// to — whatever else it may open.
    #[error(
        "this codespace's token cannot read the environment repository its session's account provisions on",
        status = StatusCode::FORBIDDEN
    )]
    CodespacesBootstrapDenied,

    /// The named type is not in the curated catalog this session can move to.
    ///
    /// A resize keeps the disk, so it stays inside the account and region the
    /// machine already lives in; a type outside that list is not a machine
    /// flyco can turn this one into. The agent sees the same curated list its
    /// `machine_resize` tool describes, so naming something else is a mistake
    /// worth saying out loud rather than a request to search harder.
    #[error(
        "`{0}` is not one of the machine types this session can be resized to",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    MachineTypeNotOffered(String),

    /// The agent asked to move onto a type that bills a minimum on boot.
    ///
    /// Never performed on the agent's own authority: an EC2 Mac bills a full
    /// day under the Apple licence the moment it starts, so the move is the
    /// user's decision. The daemon raises an
    /// [`ApprovalPayload::MachineResizeLicenseBound`] instead, and the
    /// control plane performs the resize when the user approves it.
    ///
    /// [`ApprovalPayload::MachineResizeLicenseBound`]: flyco_core::ApprovalPayload::MachineResizeLicenseBound
    #[error(
        "`{machine_type}` bills a {hours}-hour minimum the moment it boots; \
         raise an approval for it instead of resizing",
        status = StatusCode::CONFLICT
    )]
    LicenseBoundResizeNeedsApproval {
        /// The type that was asked for.
        machine_type: String,
        /// Hours the provider bills however briefly it runs.
        hours: u32,
    },

    /// Unlinking would strand machines still running on the account.
    #[error(
        "{sessions} session(s) still run on this account; archive them before unlinking",
        status = StatusCode::CONFLICT
    )]
    ProviderInUse {
        /// How many sessions still hold a machine there.
        ///
        /// Repeated as the `active_sessions` extension member of the problem
        /// document, so the confirmation a browser puts up states the count
        /// without reading it back out of this sentence.
        sessions: u32,
    },

    /// Flyco has no driver for this provider yet.
    ///
    /// Linking credentials flyco cannot act on would leave a user holding an
    /// account that silently fails at the first provision, so the refusal
    /// happens where the mistake is made.
    #[error(
        "flyco cannot provision on {provider} yet",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    ProviderUnsupported {
        /// The provider named by the submitted credentials.
        provider: &'static str,
    },

    /// The provider itself rejected the credentials.
    #[error(
        "the provider rejected these credentials: {reason}",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    ProviderRejectedCredentials {
        /// What the provider said, so the user can fix it.
        reason: String,
    },

    /// The host does not exist, or belongs to somebody else. The two are
    /// deliberately indistinguishable.
    #[error("host not found", status = StatusCode::NOT_FOUND)]
    HostNotFound,

    /// The host was drained and its token revoked; it is finished with.
    #[error(
        "this host has been removed; enroll the machine again to use it",
        status = StatusCode::CONFLICT
    )]
    HostRemoved,

    /// The machine is not connected, so nothing can be run on it.
    ///
    /// Flyco never dials a host — the machine holds the attachment — so a
    /// host that is not connected is not one to retry against from here: the
    /// answer is to start `flycod host` on it.
    #[error(
        "this host is not connected; start flycod on the machine and try again",
        status = StatusCode::CONFLICT
    )]
    HostOffline,

    /// Removing the host would stop sessions that are still running on it.
    #[error(
        "{sessions} session(s) still run on this host; pass force to stop them",
        status = StatusCode::CONFLICT
    )]
    HostHasActiveSessions {
        /// How many machines are still live there.
        ///
        /// Repeated as the `active_sessions` extension member of the problem
        /// document, so the confirmation a browser puts up states the count
        /// without reading it back out of this sentence.
        sessions: u32,
    },

    /// The label is empty or longer than a list can render.
    #[error(
        "a host label must be between 1 and {max} characters",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    InvalidHostLabel {
        /// The longest label a host may carry.
        max: usize,
    },

    /// A host account was offered to `POST /v1/providers`.
    ///
    /// A host is linked by *enrolling the machine*, which is what mints its
    /// token and proves it exists. Accepting a host id here would create an
    /// account naming a machine that can never answer.
    #[error(
        "a machine you own is linked by enrolling it, not by linking credentials",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    HostNotLinkable,

    /// The enrollment token does not exist, or belongs to somebody else.
    #[error("enrollment token not found", status = StatusCode::NOT_FOUND)]
    EnrollmentTokenNotFound,

    /// The presented enrollment token is unknown, expired, or already spent.
    ///
    /// Deliberately one variant for all three: an enrollment token is
    /// single-use and ten minutes long, the machine presenting it can act on
    /// exactly one answer — ask for another — and telling the three apart
    /// would be a free oracle for anyone guessing tokens.
    #[error(
        "this enrollment token is expired or already spent; mint another",
        status = StatusCode::GONE
    )]
    EnrollmentTokenExpired,

    /// The presented credential is not this host's live token.
    #[error("the presented host credential is not valid", status = StatusCode::UNAUTHORIZED)]
    InvalidHostCredential,

    /// No daemon has reported this session's working tree yet.
    ///
    /// Distinct from "the tree is clean": nothing has looked. Answering
    /// `dirty: false` would be inventing a fact the control plane does not
    /// have, and archiving on the strength of it is exactly the mistake the
    /// route exists to prevent.
    #[error(
        "no daemon has reported this session's working tree yet",
        status = StatusCode::NOT_FOUND
    )]
    RepoStatusUnknown,

    /// The session's checkout cannot be read because no daemon is
    /// connected to read it.
    ///
    /// The `Files` and `Diff` tabs are answered *live* by the machine —
    /// there is no copy of a working tree in the control plane — so a
    /// session that is still provisioning, stopped, or reconnecting has
    /// nothing to show and says so rather than answering an empty tree.
    #[error(
        "this session has no daemon connected to read its checkout",
        status = StatusCode::SERVICE_UNAVAILABLE
    )]
    SessionDaemonOffline,

    /// The room has no answer to this question yet.
    ///
    /// Internal to the Worker⇄room hop and never rendered for a browser:
    /// the Worker polls the room while it holds the browser's request open,
    /// and this is the "not yet" it polls against. What a browser is told
    /// when the polling runs out is [`WorkdirTimeout`](Self::WorkdirTimeout).
    #[error(
        "the room has not been given this answer yet",
        status = StatusCode::NOT_FOUND
    )]
    WorkdirNotAnsweredYet,

    /// The daemon did not answer a question about the checkout in time.
    #[error(
        "the session's daemon did not answer a question about its checkout in time",
        status = StatusCode::GATEWAY_TIMEOUT
    )]
    WorkdirTimeout,

    /// Nothing is at that path in the session's checkout.
    #[error("`{path}` is not in this session's checkout", status = StatusCode::NOT_FOUND)]
    PathNotFound {
        /// The path asked for.
        path: String,
    },

    /// The path leaves the checkout, or names its `.git` directory.
    #[error(
        "`{path}` is outside this session's checkout",
        status = StatusCode::BAD_REQUEST
    )]
    PathOutsideCheckout {
        /// The path asked for.
        path: String,
    },

    /// A listing was asked for something that is not a directory.
    #[error("`{path}` is not a directory", status = StatusCode::BAD_REQUEST)]
    PathNotADirectory {
        /// The path asked for.
        path: String,
    },

    /// Content was asked for something that is not a regular file.
    #[error("`{path}` is not a file", status = StatusCode::BAD_REQUEST)]
    PathNotAFile {
        /// The path asked for.
        path: String,
    },

    /// The file is not text, so there is nothing to render.
    #[error(
        "`{path}` is not a text file; open it from the session's terminal",
        status = StatusCode::UNSUPPORTED_MEDIA_TYPE
    )]
    FileNotText {
        /// The path asked for.
        path: String,
    },

    /// The file is larger than the control plane will serve.
    #[error(
        "`{path}` is {bytes} bytes, past the {limit} this route serves; open it from the session's terminal",
        status = StatusCode::PAYLOAD_TOO_LARGE
    )]
    FileTooLarge {
        /// The path asked for.
        path: String,
        /// What it actually measures, in bytes.
        bytes: u64,
        /// What the route will serve, in bytes.
        limit: u64,
    },

    /// The session has no base branch, so there is nothing to diff against.
    #[error(
        "this session has no base branch to diff against",
        status = StatusCode::CONFLICT
    )]
    NoBaseBranch,

    /// git could not read the session's checkout.
    #[error("the session's checkout could not be read: {0}", status = StatusCode::BAD_GATEWAY)]
    WorkdirUnreadable(String),

    /// The working tree is dirty and the caller has not confirmed discarding
    /// the uncommitted work.
    #[error(
        "this session's working tree has uncommitted changes; pass discard_uncommitted to archive without keeping them: {summary}",
        status = StatusCode::CONFLICT
    )]
    DirtyArchive {
        /// `git status --short` as last reported.
        summary: String,
    },

    /// The caller already holds as many live sessions as they may.
    #[error(
        "you already hold {cap} sessions, which is your limit; archive one first",
        status = StatusCode::CONFLICT
    )]
    SessionCapReached {
        /// The cap that was reached.
        cap: u32,
    },

    /// A `handoff` upload route or `handoff/complete` named a session that
    /// is not a pending handoff — either it never was one, or `complete`
    /// already ran under a different manifest.
    #[error(
        "this session has no pending handoff",
        status = StatusCode::CONFLICT
    )]
    HandoffNotPending,

    /// `handoff/complete` declared an object that is not stored, or is
    /// stored at another size.
    #[error(
        "the handoff {object} was not uploaded, or landed at another size",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    HandoffObjectMissing {
        /// Which payload — `patch` or `transcript`.
        object: &'static str,
    },

    /// `handoff/complete` declared checksums or sizes that disagree with
    /// what the upload routes actually received.
    #[error(
        "the handoff {object} the manifest declares is not what was uploaded",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    HandoffChecksumMismatch {
        /// Which payload — `patch` or `transcript`.
        object: &'static str,
    },

    /// A `handoff` upload body is larger than the route accepts.
    #[error(
        "the handoff {object} is {bytes} bytes, past the {limit} this route accepts",
        status = StatusCode::PAYLOAD_TOO_LARGE
    )]
    HandoffTooLarge {
        /// Which payload — `patch` or `transcript`.
        object: &'static str,
        /// What the body measures, in bytes.
        bytes: u64,
        /// What the route accepts, in bytes.
        limit: u64,
    },

    /// A create under this `Idempotency-Key` is already in flight.
    ///
    /// The claim row has no session bound yet, so the honest answer is a
    /// conflict: replaying would have nothing to replay, and proceeding
    /// would be the double-provision the key exists to prevent. The caller
    /// retries until the in-flight request records its session — or, if it
    /// failed, released the key.
    #[error(
        "a session create under this Idempotency-Key is already in flight; \
         retry to pick up its result, or list your sessions to reconcile",
        status = StatusCode::CONFLICT
    )]
    IdempotencyInFlight,

    /// The `Idempotency-Key` header named nothing usable.
    #[error(
        "an Idempotency-Key must be between 1 and {max} characters",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    InvalidIdempotencyKey {
        /// Longest key the control plane accepts.
        max: usize,
    },

    /// The CLI sign-in being polled or approved is gone.
    ///
    /// `410` rather than `404`: a cli-session is born knowing it dies — ten
    /// minutes after creation, or the moment its key was collected — so an
    /// absent record is an expired one, not a route the URL was wrong
    /// about.
    #[error(
        "this sign-in attempt expired; run `flyco login` again",
        status = StatusCode::GONE
    )]
    CliSessionGone,

    /// The user refused the CLI sign-in on the approval page.
    #[error(
        "this sign-in was refused in the browser",
        status = StatusCode::FORBIDDEN
    )]
    CliSessionDenied,

    /// The `s` token presented does not match the attempt's.
    #[error(
        "the poll token does not match this sign-in attempt",
        status = StatusCode::FORBIDDEN
    )]
    CliSessionPollDenied,

    /// The CLI sign-in already carries a decision, and a decision is final.
    #[error(
        "this sign-in attempt was already {state}",
        status = StatusCode::CONFLICT
    )]
    CliSessionAlreadyDecided {
        /// What it was already decided as — `approved` or `denied`.
        state: &'static str,
    },

    /// The caller asked flyco to pick a machine, but no linked account
    /// offers a Linux type big enough for one.
    ///
    /// Deliberately not answered with something smaller: a machine under
    /// the floor is not a cheaper version of the same session, and a
    /// catalog offering nothing big enough is a fact the user has to act
    /// on.
    #[error(
        "none of your linked accounts can deploy a Linux machine of at least \
         {vcpus} vCPUs and {memory_gib} GiB for flyco to choose",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    NoDeployableLinuxMachine {
        /// Smallest vCPU count flyco picks on its own.
        vcpus: u32,
        /// Smallest memory, in GiB, flyco picks on its own.
        memory_gib: u64,
    },

    /// The caller asked flyco to pick a machine before it had finished
    /// reading what their accounts can deploy.
    ///
    /// Not [`Self::NoDeployableLinuxMachine`], which says the catalog was
    /// read and offers nothing big enough — a fact the user has to act on.
    /// This says the opposite: nothing is wrong, the answer is coming, ask
    /// again. A cloud catalog is thousands of SKUs, quotas and price pages
    /// per region, read on the provisioning queue rather than in a request;
    /// see [`crate::catalog`].
    ///
    /// `409` rather than `503`: the control plane is healthy and every other
    /// route answers, and what is not ready is the state of *this* caller's
    /// accounts, which is exactly the conflict-with-current-state a `409`
    /// names. A `503` would state the service is unavailable, which would be
    /// untrue and would tell a client to back away from the whole API.
    #[error(
        "flyco is still reading what your linked accounts can deploy;          {accounts} of them have not answered yet",
        status = StatusCode::CONFLICT
    )]
    CatalogNotReady {
        /// How many linked accounts have not been read yet.
        accounts: usize,
    },

    /// The requested lifecycle move is not part of the session state machine.
    #[error(
        "a session cannot move from {from:?} to {to:?}",
        status = StatusCode::CONFLICT
    )]
    InvalidTransition {
        /// State the session is in.
        from: SessionState,
        /// State the caller asked for.
        to: SessionState,
    },

    /// The approval already carries a decision, and a decision is final.
    #[error(
        "this approval was already decided as {state:?}",
        status = StatusCode::CONFLICT
    )]
    ApprovalAlreadyDecided {
        /// The decision that stands.
        state: ApprovalState,
    },

    /// The session is not running, so it cannot be driven.
    #[error(
        "this session is {state:?}, and only an active session can be driven",
        status = StatusCode::CONFLICT
    )]
    SessionNotActive {
        /// The state the session is actually in.
        state: SessionState,
    },

    /// A daemon reported a usage limit whose window names no reset time.
    ///
    /// Refused rather than stored, because the whole of what the control
    /// plane does with a usage limit is stop the machine until an instant and
    /// start it again before it: a wait with no stated end is a session
    /// stopped for ever. The harness that cannot name a reset time announces
    /// the limit in the transcript and nothing else — see
    /// `crate::usage_limits`.
    #[error(
        "a usage limit can only pause a session if it says when the window resets",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    UsageLimitWithoutReset,

    /// The submitted repository is not `owner/name`.
    #[error(
        "`{0}` is not a GitHub repository in `owner/name` form",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    InvalidRepo(String),

    /// The submitted branch is not a name git would accept.
    #[error("`{name}` is not a branch name: {reason}", status = StatusCode::UNPROCESSABLE_ENTITY)]
    InvalidBranch {
        /// What was submitted.
        name: String,
        /// Which of `git check-ref-format`'s rules it breaks.
        reason: String,
    },

    /// The caller's stored GitHub token does not grant the `repo` scope a
    /// session's checkout needs.
    ///
    /// Its own variant rather than a `403` from GitHub, because the fix is
    /// specific and nothing else can produce it: the user signed flyco in
    /// before it asked for `repo`, or narrowed the authorization afterwards,
    /// and signing in again is the only thing that widens it. A session that
    /// failed with a bare "GitHub said 403" would leave them re-running the
    /// provision instead.
    #[error(
        "flyco's stored GitHub authorization does not grant the `{scope}` scope it needs to \
         check out {repo}; sign in with GitHub again to grant it",
        status = StatusCode::FORBIDDEN
    )]
    GithubTokenInsufficient {
        /// The scope the stored token is missing.
        scope: &'static str,
        /// The repository that cannot be reached without it.
        repo: RepoSlug,
    },

    /// An environment variable name is not one a shell can export.
    #[error(
        "`{0}` is not an environment variable name: use letters, digits and `_`, not starting with a digit",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    InvalidEnvKey(String),

    /// The submitted session title is empty or too long.
    #[error(
        "a session title must be between 1 and {max} characters",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    InvalidTitle {
        /// Longest title the control plane accepts.
        max: usize,
    },

    /// The submitted budget limit cannot fund anything.
    #[error(
        "a session budget must be greater than zero",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    InvalidBudget,

    /// The caller named a model, or an effort, the harness does not offer.
    ///
    /// A `400` rather than a `422`, and the difference is which of the two
    /// the caller can fix: an unprocessable body is one flyco understood
    /// and refused on its own terms, and this is a body naming something
    /// that does not exist — a picker rendered from a model list the
    /// harness has since revised. The detail is the refusal's own sentence,
    /// which names the model, because "invalid model" alone leaves the user
    /// guessing which half they got wrong.
    #[error("{0}", status = StatusCode::BAD_REQUEST)]
    InvalidModel(flyco_core::ModelChoiceError),

    /// A `PATCH` body named nothing to change.
    ///
    /// Every field of an update is optional, so a body with none of them is
    /// a caller that meant something and sent nothing. Answering it with an
    /// unchanged session would report success for a request that had no
    /// content.
    #[error(
        "an update must name at least one thing to change",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    EmptyUpdate,

    /// The submitted session cap is outside the allowed range.
    #[error(
        "a session cap must be between {min} and {max}",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    InvalidSessionCap {
        /// Smallest cap the control plane accepts.
        min: u32,
        /// Largest cap the control plane accepts.
        max: u32,
    },

    /// A daemon posted an observation that observes nothing.
    ///
    /// The LLM usage panel is the sum of what actually happened, so a row
    /// reporting neither a cost nor a rate limit would add nothing to it
    /// and would make "no observations yet" indistinguishable from "several
    /// observations of nothing".
    #[error(
        "an observation must report a cost, a rate limit, or both",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    EmptyObservation,

    /// A message with nothing in it was sent to an agent.
    #[error(
        "a message to an agent cannot be empty",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    EmptyMessage,

    /// A pagination cursor was not one this API issued.
    #[error("`{0}` is not a page cursor from this API", status = StatusCode::BAD_REQUEST)]
    InvalidCursor(String),

    /// A path parameter that must be a UUID was not one.
    #[error("`{0}` is not a valid identifier", status = StatusCode::BAD_REQUEST)]
    MalformedId(String),

    /// A transcript stream key is not a single safe path segment.
    #[error(
        "`{0}` is not a transcript stream key: use letters, digits, `.`, `_`, and `-` only",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    InvalidStreamKey(String),

    /// A transcript batch sequence number is wider than the key format.
    #[error(
        "batch sequence {seq} is beyond the largest a transcript key can address",
        status = StatusCode::UNPROCESSABLE_ENTITY
    )]
    BatchSeqOutOfRange {
        /// The sequence number that was submitted.
        seq: u64,
    },

    /// A transcript batch with this sequence number is already stored.
    ///
    /// Batches are immutable: overwriting one would silently reorder the
    /// transcript, so a repeat is a conflict rather than an update.
    #[error(
        "transcript batch {seq} is already stored and batches are immutable",
        status = StatusCode::CONFLICT
    )]
    BatchAlreadyStored {
        /// The sequence number that was submitted again.
        seq: u64,
    },

    /// The presented daemon token does not pair with this session.
    #[error("the presented credential is not this session's daemon token", status = StatusCode::UNAUTHORIZED)]
    InvalidDaemonCredential,

    /// A daemon speaks a wire protocol version this control plane does not.
    ///
    /// `409` rather than `426`: nothing here can be upgraded in place — the
    /// daemon has to be replaced by a build that speaks this version.
    #[error(
        "the daemon speaks wire protocol {daemon}, this control plane speaks {control}",
        status = StatusCode::CONFLICT
    )]
    ProtocolMismatch {
        /// The version the daemon declared.
        daemon: u32,
        /// The version this control plane speaks.
        control: u32,
    },

    /// A daemon's stream or frames named an attach that a newer one replaced.
    ///
    /// `409`: the caller's attach is stale state, and the conflict resolves
    /// by attaching again rather than by retrying.
    #[error(
        "attach epoch {opened} is stale; the room's current epoch is {current}",
        status = StatusCode::CONFLICT
    )]
    RelayEpochStale {
        /// The epoch the room is serving now.
        current: u64,
        /// The epoch the request named.
        opened: u64,
    },

    /// A frames batch skips sequence numbers the room never stored.
    ///
    /// `409`: the room's record of the stream cannot advance past a hole,
    /// so the batch is refused and the daemon re-sends from the gap.
    #[error(
        "frames resume at {got} but the room has stored only through {next}",
        status = StatusCode::CONFLICT
    )]
    RelayFramesGap {
        /// The sequence the next batch must resume at.
        next: u64,
        /// The sequence the refused batch began at.
        got: u64,
    },

    /// The live relay is not available on this build of the control plane.
    #[error(
        "this control plane does not host session relays: {0}",
        status = StatusCode::NOT_IMPLEMENTED
    )]
    RelayUnavailable(&'static str),

    /// The session's Durable Object could not be reached, or refused.
    #[error("the session room failed: {0}", status = StatusCode::BAD_GATEWAY)]
    Room(String),

    /// A room's own refusal, forwarded to the caller untouched.
    ///
    /// The Durable Object answered with the RFC 9457 document the caller
    /// should see — a stale epoch, a frames gap, an offline daemon — and
    /// re-wrapping it as `502` would tell a daemon that can recover
    /// ("attach again") only that something failed. The document carries
    /// its own status; the variant's declared one is never rendered.
    #[error("the room refused: {0:?}", status = StatusCode::BAD_GATEWAY)]
    RoomRefused(Box<Problem>),

    /// Object storage failed.
    #[error("object storage failed: {0}")]
    Storage(#[from] StorageError),

    /// A stored row does not match the schema the control plane expects.
    #[error("stored record is inconsistent: {0}")]
    CorruptRecord(&'static str),

    /// A portable service the handler needs was never injected — a wiring
    /// bug in `Skyzen.toml`, not something a caller can provoke.
    #[error("required service `{0}` is not configured")]
    ServiceMissing(&'static str),

    /// GitHub refused the authorization code the browser came back with.
    ///
    /// The caller's failure rather than flyco's — a code that was mistyped,
    /// replayed, or left to expire — so it says exactly what GitHub said,
    /// which is the only text that tells the user to start the sign-in
    /// again rather than to wait for an outage to pass.
    #[error("{0}", status = StatusCode::BAD_REQUEST)]
    GithubCodeRejected(String),

    /// GitHub no longer accepts the token flyco holds for this user.
    ///
    /// A `401` from `GET /user/repos` or any other call made with the
    /// user's token means GitHub stopped honouring that token, and only a
    /// new GitHub authorization can replace it. The flyco session is not
    /// the thing that failed, so this is deliberately not a `401` of
    /// flyco's own: a `424` names a dependency the request needed and could
    /// not use, the client keeps the user signed in, and the repository
    /// chip offers to reconnect GitHub in place.
    #[error(
        "GitHub no longer accepts flyco's access to your account; reconnect GitHub",
        status = StatusCode::FAILED_DEPENDENCY
    )]
    GithubTokenRevoked,

    /// GitHub answered one of flyco's calls with a status it cannot use.
    ///
    /// Still a `502` — flyco cannot serve the request — but the status and
    /// the call are facts about GitHub, not flyco internals, and they are
    /// the difference between "sign-in is broken" and "your token no longer
    /// grants what the repository picker reads".
    #[error("{0}", status = StatusCode::BAD_GATEWAY)]
    GithubStatus(String),

    /// GitHub could not be reached at all.
    ///
    /// The one GitHub failure that stays opaque: its message is whatever
    /// the HTTP client or the deserializer said, which is flyco's own
    /// plumbing. [`problem`](Self::problem) logs it in full.
    #[error("GitHub call failed: {0}", status = StatusCode::BAD_GATEWAY)]
    Github(GithubError),

    /// Anthropic could not be reached, or answered with something flyco
    /// cannot interpret.
    ///
    /// A refusal is not one of these — that is
    /// [`ClaudeOauthRejected`](Self::ClaudeOauthRejected), which the caller
    /// can act on. See [`From<AnthropicError>`](Self::from).
    #[error("Anthropic call failed: {0}", status = StatusCode::BAD_GATEWAY)]
    Anthropic(AnthropicError),

    /// `OpenAI` could not be reached, or answered with something flyco
    /// cannot interpret.
    ///
    /// A refusal is not one of these — those are
    /// [`CodexOauthRejected`](Self::CodexOauthRejected) and
    /// [`CodexDeviceAuthDisabled`](Self::CodexDeviceAuthDisabled), both of
    /// which the caller can act on. See [`From<OpenAiError>`](Self::from).
    #[error("OpenAI call failed: {0}", status = StatusCode::BAD_GATEWAY)]
    OpenAi(OpenAiError),

    /// Microsoft could not be reached, or answered with something flyco
    /// cannot interpret.
    ///
    /// A refusal is not one of these — that is
    /// [`MicrosoftRejected`](Self::MicrosoftRejected), which the caller can
    /// act on.
    #[error("Microsoft call failed: {0}", status = StatusCode::BAD_GATEWAY)]
    Microsoft(MicrosoftError),

    /// Google could not be reached, or answered with something flyco cannot
    /// interpret.
    ///
    /// A refusal is not one of these — that is
    /// [`GoogleRejected`](Self::GoogleRejected).
    #[error("Google call failed: {0}", status = StatusCode::BAD_GATEWAY)]
    Google(GoogleError),

    /// The key-value store failed.
    #[error("key-value store failed: {0}")]
    Kv(#[from] KvError),

    /// The provisioning queue would not take, or would not give up, a job.
    #[error("the provisioning queue failed: {0}")]
    Queue(#[from] QueueError),

    /// The database failed.
    #[error("database failed: {0}")]
    Db(#[from] DbError),

    /// A cryptographic primitive failed.
    #[error("cryptography failed: {0}")]
    Crypto(#[from] CryptoError),
}

/// Sorts an Anthropic failure into "your code was no good" and "Anthropic
/// is not answering".
///
/// The distinction is the whole difference between a 422 the user can fix
/// by pasting the code again and a 502 they can only wait out, so it is made
/// once, here, rather than at each call site.
impl From<AnthropicError> for ApiError {
    fn from(error: AnthropicError) -> Self {
        match error {
            rejected @ AnthropicError::Rejected { .. } => Self::ClaudeOauthRejected {
                reason: rejected.to_string(),
            },
            unavailable => Self::Anthropic(unavailable),
        }
    }
}

/// Sorts a GitHub failure into what the caller can act on and what only the
/// operator can.
///
/// The whole of the fix for a sign-in that says nothing: a rejected code is
/// a `400` quoting GitHub's own reason, a refused status is a `502` naming
/// the call and the status, and only a transport failure — whose text is
/// flyco's own plumbing — stays the opaque one.
impl From<GithubError> for ApiError {
    fn from(error: GithubError) -> Self {
        match error {
            rejected @ GithubError::Rejected { .. } => {
                Self::GithubCodeRejected(rejected.to_string())
            }
            // A 401 to a call made with the user's *stored* token is the
            // token's death, not GitHub's unavailability. The exchange and
            // the profile read that follows it happen during a sign-in,
            // with a token just minted; their 401 stays GitHub's refusal.
            GithubError::Status {
                call: GithubCall::Repositories | GithubCall::Repository | GithubCall::Branches,
                status: 401,
                reason,
            } => {
                // GitHub's own sentence is the only record of *why* a token
                // it issued stopped working; the problem the user sees says
                // what to do, so the reason is kept here.
                tracing::warn!(%reason, "GitHub refused the user's stored token");
                Self::GithubTokenRevoked
            }
            refused @ GithubError::Status { .. } => Self::GithubStatus(refused.to_string()),
            unreachable => Self::Github(unreachable),
        }
    }
}

/// Sorts an `OpenAI` failure into what the user can fix and what they can
/// only wait out.
///
/// Three answers rather than two, because the device flow has a refusal
/// with a *fix*: device code authorization being switched off is a 409 that
/// says where the switch is, not a 502 the user can do nothing about.
impl From<OpenAiError> for ApiError {
    fn from(error: OpenAiError) -> Self {
        match error {
            OpenAiError::DeviceAuthDisabled => Self::CodexDeviceAuthDisabled {
                settings_url: DEVICE_AUTH_SETTINGS_URL,
            },
            rejected @ OpenAiError::Rejected { .. } => Self::CodexOauthRejected {
                reason: rejected.to_string(),
            },
            unavailable => Self::OpenAi(unavailable),
        }
    }
}

/// Sorts a Microsoft failure into what the caller can act on and what they
/// can only wait out.
///
/// The same split [`From<AnthropicError>`](ApiError::from) makes, and for
/// the same reason: a refusal is a `422` naming what Microsoft objected to,
/// and everything else is a `502` the user can do nothing about.
impl From<MicrosoftError> for ApiError {
    fn from(error: MicrosoftError) -> Self {
        match error {
            rejected @ MicrosoftError::Rejected { .. } => Self::MicrosoftRejected {
                reason: rejected.to_string(),
            },
            unavailable => Self::Microsoft(unavailable),
        }
    }
}

/// The same split, for Google.
impl From<GoogleError> for ApiError {
    fn from(error: GoogleError) -> Self {
        match error {
            rejected @ GoogleError::Rejected { .. } => Self::GoogleRejected {
                reason: rejected.to_string(),
            },
            unavailable => Self::Google(unavailable),
        }
    }
}

impl ApiError {
    /// The slug this failure is documented under, below
    /// [`TYPE_BASE`](flyco_core::problem::TYPE_BASE).
    ///
    /// `pub(crate)` because one route answers with it rather than with a
    /// problem document: the cloud OAuth callbacks are browser navigations,
    /// so they carry the slug on the redirect they send the browser to —
    /// see [`crate::provider_oauth`].
    #[expect(
        clippy::too_many_lines,
        reason = "one arm per variant, which is the point: the match is exhaustive, so a \
                  variant added without a slug is a compile error rather than an untyped \
                  problem document"
    )]
    pub(crate) const fn slug(&self) -> &'static str {
        match self {
            Self::MissingCredential => "missing-credential",
            Self::InvalidCredential => "invalid-credential",
            Self::UnknownOauthState => "unknown-oauth-state",
            Self::ApiKeyNotFound => "api-key-not-found",
            Self::SessionNotFound => "session-not-found",
            Self::ReleaseArtifactNotFound => "release-artifact-not-found",
            Self::ApprovalNotFound => "approval-not-found",
            Self::MemoryNodeNotFound => "memory-node-not-found",
            Self::McpServerNotFound => "mcp-server-not-found",
            Self::McpServerNameTaken { .. } => "mcp-server-name-taken",
            Self::InvalidMcpServer(_) => "invalid-mcp-server",
            Self::SkillNotFound => "skill-not-found",
            Self::InvalidSkill(_) => "invalid-skill",
            Self::WebhookUnverified => "webhook-unverified",
            Self::WebhookMalformed { .. } => "webhook-malformed",
            Self::PushSubscriptionNotFound => "push-subscription-not-found",
            Self::InvalidPushSubscription(_) => "invalid-push-subscription",
            Self::PushDeliveryFailed(_) => "push-delivery-failed",
            Self::ProviderAccountNotFound => "provider-account-not-found",
            Self::InvalidHarnessCredential(_) => "invalid-harness-credential",
            Self::HarnessAccountNotFound => "harness-account-not-found",
            Self::HarnessAccountInUse { .. } => "harness-account-in-use",
            Self::ClaudeOauthAttemptExpired => "claude-oauth-attempt-expired",
            Self::ClaudeOauthStateMismatch => "claude-oauth-state-mismatch",
            Self::ClaudeOauthRejected { .. } => "claude-oauth-rejected",
            Self::Anthropic(_) => "anthropic-unavailable",
            Self::CodexOauthAttemptExpired => "codex-oauth-attempt-expired",
            Self::CodexDeviceAuthDisabled { .. } => "codex-device-auth-disabled",
            Self::CodexOauthRejected { .. } => "codex-oauth-rejected",
            Self::OpenAi(_) => "openai-unavailable",
            Self::ProviderOauthAttemptExpired => "provider-oauth-attempt-expired",
            Self::ProviderOauthNotAuthorized => "provider-oauth-not-authorized",
            Self::MicrosoftRejected { .. } => "microsoft-rejected",
            Self::Microsoft(_) => "microsoft-unavailable",
            Self::GoogleRejected { .. } => "google-rejected",
            Self::Google(_) => "google-unavailable",
            Self::GithubRejected { .. } => "github-rejected",
            Self::MachineNotFound => "machine-not-found",
            Self::CodespacesMachineUnknown => "codespaces-machine-unknown",
            Self::CodespacesBootstrapDenied => "codespaces-bootstrap-denied",
            Self::MachineTypeNotOffered(_) => "machine-type-not-offered",
            Self::LicenseBoundResizeNeedsApproval { .. } => "license-bound-resize-needs-approval",
            Self::MachineNotReady => "machine-not-ready",
            Self::Provisioning(_) => "provisioning-failed",
            Self::MachineUnavailable(_) => "machine-unavailable",
            Self::MachineRuntimeMismatch { .. } => "machine-runtime-mismatch",
            Self::ProviderInUse { .. } => "provider-in-use",
            Self::ProviderUnsupported { .. } => "provider-unsupported",
            Self::ProviderRejectedCredentials { .. } => "provider-rejected-credentials",
            Self::HostNotFound => "host-not-found",
            Self::HostRemoved => "host-removed",
            Self::HostOffline => "host-offline",
            Self::HostHasActiveSessions { .. } => "host-has-active-sessions",
            Self::InvalidHostLabel { .. } => "invalid-host-label",
            Self::HostNotLinkable => "host-not-linkable",
            Self::EnrollmentTokenNotFound => "enrollment-token-not-found",
            Self::EnrollmentTokenExpired => "enrollment-token-expired",
            Self::InvalidHostCredential => "invalid-host-credential",
            Self::RepoStatusUnknown => "repo-status-unknown",
            Self::SessionDaemonOffline => "session-daemon-offline",
            Self::WorkdirNotAnsweredYet => "workdir-not-answered-yet",
            Self::WorkdirTimeout => "workdir-timeout",
            Self::PathNotFound { .. } => "path-not-found",
            Self::PathOutsideCheckout { .. } => "path-outside-checkout",
            Self::PathNotADirectory { .. } => "path-not-a-directory",
            Self::PathNotAFile { .. } => "path-not-a-file",
            Self::FileNotText { .. } => "file-not-text",
            Self::FileTooLarge { .. } => "file-too-large",
            Self::NoBaseBranch => "no-base-branch",
            Self::WorkdirUnreadable(_) => "workdir-unreadable",
            Self::DirtyArchive { .. } => "dirty-archive",
            Self::SessionNotActive { .. } => "session-not-active",
            Self::UsageLimitWithoutReset => "usage-limit-without-reset",
            Self::SessionCapReached { .. } => "session-cap-reached",
            Self::IdempotencyInFlight => "idempotency-in-flight",
            Self::InvalidIdempotencyKey { .. } => "invalid-idempotency-key",
            Self::HandoffNotPending => "handoff-not-pending",
            Self::HandoffObjectMissing { .. } => "handoff-object-missing",
            Self::HandoffChecksumMismatch { .. } => "handoff-checksum-mismatch",
            Self::HandoffTooLarge { .. } => "handoff-too-large",
            Self::CliSessionGone => "cli-session-gone",
            Self::CliSessionDenied => "cli-session-denied",
            Self::CliSessionPollDenied => "cli-session-poll-denied",
            Self::CliSessionAlreadyDecided { .. } => "cli-session-already-decided",
            Self::NoDeployableLinuxMachine { .. } => "no-deployable-linux-machine",
            Self::CatalogNotReady { .. } => "catalog-not-ready",
            Self::InvalidTransition { .. } => "invalid-session-transition",
            Self::ApprovalAlreadyDecided { .. } => "approval-already-decided",
            Self::InvalidRepo(_) => "invalid-repo",
            Self::InvalidBranch { .. } => "invalid-branch",
            Self::GithubTokenInsufficient { .. } => "github-token-insufficient",
            Self::InvalidEnvKey(_) => "invalid-env-key",
            Self::InvalidTitle { .. } => "invalid-title",
            Self::InvalidBudget => "invalid-budget",
            Self::InvalidModel(_) => "invalid-model",
            Self::EmptyUpdate => "empty-update",
            Self::InvalidSessionCap { .. } => "invalid-session-cap",
            Self::EmptyMessage => "empty-message",
            Self::EmptyObservation => "empty-observation",
            Self::InvalidCursor(_) => "invalid-cursor",
            Self::MalformedId(_) => "malformed-id",
            Self::InvalidStreamKey(_) => "invalid-stream-key",
            Self::BatchSeqOutOfRange { .. } => "batch-seq-out-of-range",
            Self::BatchAlreadyStored { .. } => "batch-already-stored",
            Self::InvalidDaemonCredential => "invalid-daemon-credential",
            Self::ProtocolMismatch { .. } => "protocol-mismatch",
            Self::RelayEpochStale { .. } => "relay-epoch-stale",
            Self::RelayFramesGap { .. } => "relay-frames-gap",
            Self::RelayUnavailable(_) => "relay-unavailable",
            Self::Room(_) | Self::RoomRefused(_) => "session-room-unavailable",
            Self::GithubCodeRejected(_) => "github-code-rejected",
            Self::GithubTokenRevoked => "github-token-revoked",
            Self::GithubStatus(_) => "github-status",
            Self::Github(_) => "github-unavailable",
            Self::CorruptRecord(_)
            | Self::ServiceMissing(_)
            | Self::Kv(_)
            | Self::Db(_)
            | Self::Queue(_)
            | Self::Storage(_)
            | Self::Crypto(_) => "internal",
        }
    }

    /// The RFC 6750 challenge this failure must carry, if any.
    pub(crate) const fn challenge(&self) -> Option<Challenge> {
        match self {
            Self::MissingCredential => Some(Challenge::Bearer),
            Self::InvalidCredential
            | Self::InvalidDaemonCredential
            | Self::InvalidHostCredential => Some(Challenge::InvalidToken),
            _ => None,
        }
    }

    /// Whether this failure's explanation must stay in the log.
    ///
    /// Anything that broke on flyco's side describes itself only to the
    /// operator. The exceptions are the refusals whose whole point is to
    /// name something the caller could not otherwise know:
    /// [`RelayUnavailable`](Self::RelayUnavailable) says which capability
    /// this build does not have, and [`GithubStatus`](Self::GithubStatus)
    /// says which call GitHub refused and with what — neither of which is
    /// a flyco internal, and both of which are the difference between a
    /// user who can act and one staring at a bare `502`.
    ///
    /// [`SessionDaemonOffline`](Self::SessionDaemonOffline) and
    /// [`WorkdirTimeout`](Self::WorkdirTimeout) are there for the same
    /// reason and are the sharper case: they are `5xx` because the machine
    /// is not answering, which is a state of the *session* rather than a
    /// fault in the control plane. Telling a user their machine is not
    /// connected is the whole answer; telling them flyco failed is a lie
    /// that sends them looking in the wrong place.
    fn is_opaque(&self) -> bool {
        !matches!(
            self,
            Self::RelayUnavailable(_)
                | Self::GithubStatus(_)
                | Self::SessionDaemonOffline
                | Self::WorkdirTimeout
        ) && skyzen::HttpError::status(self).is_server_error()
    }

    /// The RFC 9457 document describing this failure.
    ///
    /// Server-side failures are logged in full and reported as a bare status.
    #[must_use]
    pub fn problem(&self) -> Problem {
        // A refusal the room already typed is handed back untouched: its
        // document says the same thing this Worker would have said, and it
        // was already logged where it was raised.
        if let Self::RoomRefused(problem) = self {
            return (**problem).clone();
        }

        let status = skyzen::HttpError::status(self);
        let title = status.canonical_reason().unwrap_or("Error");

        if status.is_server_error() {
            tracing::error!(error = %self, "request failed");
        } else {
            tracing::debug!(error = %self, "rejected a request");
        }

        let detail = if self.is_opaque() {
            SERVER_DETAIL.to_owned()
        } else {
            self.to_string()
        };

        Problem::of_type(self.slug(), status.as_u16(), title, detail)
            .with_extensions(self.extensions())
    }

    /// The typed facts this failure carries beyond its prose.
    ///
    /// RFC 9457 §3.2 extension members, and the reason a refusal never has
    /// to be read as a sentence: a client that needs the number in
    /// "3 session(s) still run on this host" reads `active_sessions`
    /// instead of the words around it. Every other failure states its whole
    /// self in `detail` and carries none.
    ///
    /// All three "something is still running there" refusals carry it, and
    /// they carry it for one reason: each is answered by the same dialog,
    /// which has to say how much work ending would cost. A refusal that
    /// counted only in prose would make that dialog parse English.
    fn extensions(&self) -> ProblemExtensions {
        match *self {
            Self::HostHasActiveSessions { sessions }
            | Self::ProviderInUse { sessions }
            | Self::HarnessAccountInUse { sessions } => ProblemExtensions {
                active_sessions: Some(sessions),
            },
            _ => ProblemExtensions::default(),
        }
    }

    /// Renders this failure as a complete response.
    #[must_use]
    pub fn into_response(self) -> Response {
        problem::response(&self.problem(), self.challenge())
    }
}

#[cfg(test)]
mod tests {
    use super::ApiError;
    use crate::github::{GithubCall, GithubError};

    #[test]
    fn a_client_error_explains_itself() {
        let problem = ApiError::MalformedId("nope".to_owned()).problem();

        assert_eq!(problem.status, 400);
        assert_eq!(problem.kind, "https://flyco.dev/problems/malformed-id");
        assert_eq!(problem.title, "Bad Request");
        assert!(problem.detail.contains("nope"));
    }

    #[test]
    fn a_server_error_says_nothing_about_its_internals() {
        let problem = ApiError::CorruptRecord("users.id is not a UUID").problem();

        assert_eq!(problem.status, 500);
        assert_eq!(problem.kind, "https://flyco.dev/problems/internal");
        assert!(!problem.detail.contains("users.id"));
    }

    #[test]
    fn a_machine_that_is_not_answering_says_so_rather_than_blaming_flyco() {
        // Both are `5xx` because the machine is not answering, which is a
        // state of the session and not a fault in the control plane: a user
        // told "the control plane failed" goes looking in the wrong place.
        for error in [ApiError::SessionDaemonOffline, ApiError::WorkdirTimeout] {
            let problem = error.problem();

            assert!(problem.status >= 500);
            assert!(
                problem.detail.contains("daemon"),
                "a {} problem says only: {}",
                problem.status,
                problem.detail
            );
        }
    }

    #[test]
    fn a_busy_host_states_its_count_as_a_typed_member() {
        let problem = ApiError::HostHasActiveSessions { sessions: 3 }.problem();

        // The number a browser acts on is a member of the document, not a
        // word in a sentence it would have to parse.
        assert_eq!(problem.extensions.active_sessions, Some(3));
        let json = serde_json::to_value(&problem).expect("serialize");
        assert_eq!(json["active_sessions"], 3);
    }

    #[test]
    fn every_in_use_refusal_states_its_count_as_a_typed_member() {
        // One dialog answers all three, so all three have to hand it the
        // number rather than a sentence about the number.
        for error in [
            ApiError::ProviderInUse { sessions: 2 },
            ApiError::HarnessAccountInUse { sessions: 2 },
        ] {
            let problem = error.problem();

            assert_eq!(problem.status, 409);
            assert_eq!(problem.extensions.active_sessions, Some(2));
            let json = serde_json::to_value(&problem).expect("serialize");
            assert_eq!(json["active_sessions"], 2);
        }
    }

    #[test]
    fn a_failure_that_defines_no_extension_carries_none() {
        let problem = ApiError::HostNotFound.problem();

        assert_eq!(problem.extensions, flyco_core::ProblemExtensions::default());
        assert!(
            serde_json::to_value(&problem)
                .expect("serialize")
                .get("active_sessions")
                .is_none()
        );
    }

    #[test]
    fn a_rejected_authorization_code_quotes_githubs_own_reason() {
        let problem = ApiError::from(GithubError::Rejected {
            code: "bad_verification_code".to_owned(),
            description: "The code passed is incorrect or expired.".to_owned(),
        })
        .problem();

        assert_eq!(problem.status, 400);
        assert_eq!(
            problem.kind,
            "https://flyco.dev/problems/github-code-rejected"
        );
        assert_eq!(
            problem.detail,
            "GitHub rejected the authorization code: bad_verification_code (The code passed is \
             incorrect or expired.)"
        );
    }

    #[test]
    fn a_401_to_a_call_made_with_the_users_token_asks_to_reconnect_github() {
        let problem = ApiError::from(GithubError::Status {
            call: GithubCall::Repositories,
            status: 401,
            reason: "Bad credentials".to_owned(),
        })
        .problem();

        // Not a 401 of flyco's own: the session is fine, the credential
        // flyco holds for GitHub is what died, and the client must not
        // sign the user out over it.
        assert_eq!(problem.status, 424);
        assert_eq!(
            problem.kind,
            "https://flyco.dev/problems/github-token-revoked"
        );

        // A sign-in's own calls carry no stored token: their 401 is GitHub
        // refusing flyco, and stays the 502 it was.
        let exchange = ApiError::from(GithubError::Status {
            call: GithubCall::TokenExchange,
            status: 401,
            reason: "Bad credentials".to_owned(),
        })
        .problem();
        assert_eq!(exchange.status, 502);
    }

    #[test]
    fn a_status_github_refused_with_names_the_call_it_refused() {
        let problem = ApiError::from(GithubError::Status {
            call: GithubCall::UserProfile,
            status: 401,
            reason: "Bad credentials".to_owned(),
        })
        .problem();

        assert_eq!(problem.status, 502);
        assert_eq!(problem.kind, "https://flyco.dev/problems/github-status");
        assert_eq!(
            problem.detail,
            "GitHub answered HTTP 401 to the account profile request: Bad credentials",
            "a 5xx that is GitHub's own answer is stated, not blanked"
        );
    }

    #[test]
    fn a_github_transport_failure_keeps_its_internals_in_the_log() {
        let problem = ApiError::from(GithubError::Transport {
            call: GithubCall::TokenExchange,
            message: "dns error: failed to lookup github.com".to_owned(),
        })
        .problem();

        assert_eq!(problem.status, 502);
        assert_eq!(
            problem.kind,
            "https://flyco.dev/problems/github-unavailable"
        );
        assert!(!problem.detail.contains("dns error"));
    }

    #[test]
    fn only_the_two_unauthorized_variants_carry_a_challenge() {
        assert!(ApiError::MissingCredential.challenge().is_some());
        assert!(ApiError::InvalidCredential.challenge().is_some());
        assert!(ApiError::ApiKeyNotFound.challenge().is_none());
    }
}
