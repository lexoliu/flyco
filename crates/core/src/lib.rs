//! Shared domain model for flyco.
//!
//! This crate is the single source of truth for every type that crosses a
//! boundary in flyco: the REST DTOs served by the control plane, the wire
//! protocol spoken between `flycod` (the VM daemon) and the control plane,
//! and the pure business logic that both sides must agree on — most
//! importantly the [`budget`] engine.
//!
//! It compiles on `wasm32-unknown-unknown` (the Cloudflare Worker) and on
//! native targets (the daemon), and performs no I/O.

pub mod agents;
pub mod approval;
pub mod auth;
pub mod budget;
pub mod catalog;
pub mod env;
pub mod github;
pub mod handoff;
pub mod harness;
pub mod host;
pub mod id;
pub mod machine;
pub mod mcp;
pub mod mcp_catalog;
pub mod memory;
pub mod money;
pub mod problem;
pub mod providers;
pub mod push;
pub mod release;
pub mod repo;
pub mod session;
pub mod skill_catalog;
pub mod skills;
#[cfg(feature = "sql")]
pub mod sql;
pub mod usage;
pub mod wire;
pub mod workdir;

pub use agents::{AgentsDocument, UpdateAgentsDocument};
pub use approval::{ApprovalState, ApprovalView, DecideApproval};
pub use auth::{
    ApiKeySummary, AuthorizeUrl, CliSession, CliSessionKey, CreateApiKey, CreateCliSession,
    CreatedApiKey, CurrentUser, DAEMON_TOKEN_PREFIX, DaemonToken, SESSION_CAP_DEFAULT,
    SESSION_CAP_MAX, SESSION_CAP_MIN, UpdateMe,
};
pub use budget::{
    BudgetConfig, BudgetSignal, BudgetStage, BudgetState, BudgetView, SpendEvent, SpendKind,
};
pub use catalog::curate;
pub use env::{EnvDocument, EnvEntry, NETWORK_CONTROL_WARNING, UpdateEnv};
pub use github::{BranchPage, BranchSummary, RepoSummary};
pub use handoff::{
    HANDOFF_DIR, HANDOFF_PATCH_BYTES_MAX, HANDOFF_TRANSCRIPT_BYTES_MAX, HANDOFF_TRANSCRIPT_PATH,
    HandoffManifest, HandoffView, LocalHandoff, SESSION_WORKDIR, SessionSource,
};
pub use harness::{
    Availability, ClaudeOauthStart, CodexOauthPending, CodexOauthStart, CompleteClaudeOauth,
    CompleteDevinOauth, ContextCost, ContextUsage, ContextWindow, DEVIN_ID_TAILS, DevinOauthStart,
    DriverKind, Feature, HarnessAccountView, HarnessCredentialInput, HarnessEvent, HarnessFeature,
    HarnessKind, LinkHarnessAccount, ModelChoice, ModelChoiceError, ModelOption, PermissionMode,
    ReportModels, ReportUsage, UsageLimitHit, UsageReport, availability, builtin_models, matrix,
    normalize_models,
};
pub use host::{
    ENROLLMENT_TOKEN_TTL_SECONDS, EnrollHost, EnrolledHost, Enrollment, EnrollmentToken,
    HOST_TOKEN_PREFIX, HostFacts, HostState, HostView, JobOutcome, MAX_HOST_LABEL_CHARS,
    ReportJobResult, UpdateHost,
};
pub use id::{
    ApiKeyId, ApprovalId, BudgetId, BudgetSignalId, ClaudeOauthAttemptId, CliSessionId,
    CodexOauthAttemptId, DevinOauthAttemptId, EnrollmentTokenId, HarnessAccountId,
    HarnessObservationId, HostId, Id, MachineId, MarketplaceId, McpServerId, MemoryNodeId,
    ProviderAccountId, ProviderOauthAttemptId, PushSubscriptionId, SessionId, ShellRunId, SkillId,
    SpendEventId, UserId, WorkdirRequestId,
};
pub use machine::{
    AUTO_MIN_MEMORY_MIB, AUTO_MIN_VCPUS, AgentMachineView, BillingMinimum, CloudProviderKind,
    CpuArchitecture, FreeGrant, MachineCapacity, MachineCatalog, MachineCatalogEntry,
    MachineDefault, MachineLineage, MachinePricing, MachineSpec, MachineState, MachineView,
    OsFamily, RegionLocation, ResizeMachine, Runtime, SessionMachine, StoragePriceTier,
    StoragePricing, auto_linux_choice,
};
pub use mcp::{HeaderEntry, McpServerConfig, McpServerMount, McpServerView, UpsertMcpServer};
pub use mcp_catalog::{
    CatalogInput, CatalogInstallKind, CatalogMcpInstall, CatalogMcpServer, InstallCatalogMcpServer,
    McpCatalogPage,
};
pub use memory::{CreateMemoryNode, MemoryNode, UpdateMemoryNode};
pub use money::Usd;
pub use problem::{Problem, ProblemExtensions};
pub use providers::{
    AwsIamPolicy, CodespacesBootstrap, CodespacesBootstrapRequest, FinishAzureOauth,
    FinishCodespacesOauth, FinishGcpOauth, LinkProvider, ProviderAccountView, ProviderBonusHint,
    ProviderCredentials, ProviderOauthChoice, ProviderOauthProgress, ProviderOauthStart,
    QuickstartAnswers,
};
pub use push::{PushKeys, PushSubscription, PushSubscriptionView, VapidPublicKey};
pub use release::{PublishedBinary, PublishedObject};
pub use repo::{
    BranchName, BranchNameError, CheckoutStatus, MAX_SESSION_REPOS, RepoAddedBy, RepoSelection,
    RepoSlug, RepoStatus, SessionRepo, checkout_dir,
};
pub use session::{
    ARCHIVE_AFTER_IDLE_SECS, ARCHIVE_FINISHED_AFTER_IDLE_SECS, CreateSession, DEFAULT_DISK_GIB,
    DesktopInputRequest, DesktopTakeoverRequest, HarnessSessionView, HarnessTui, InterruptedReason,
    MAX_SESSION_TITLE_CHARS, MachineChoice, MachineOrigin, PROMPT_EXCERPT_CHARS,
    PROVISION_DEADLINE_SECS, PausedReason, RunShell, SUSPEND_AFTER_IDLE_SECS, SendMessage,
    SessionActivity, SessionDetail, SessionState, SessionSummary, SessionTransitionError,
    TerminalInput, TerminalSize, TurnPage, TurnSummary, USAGE_LIMIT_CONTINUE_MESSAGE,
    USAGE_LIMIT_STOP_AFTER_SECS, USAGE_LIMIT_WAKE_LEAD_SECS, UpdateSession, UsageLimitPause,
    excerpt,
};
pub use skill_catalog::{
    AddMarketplace, BUILT_IN_MARKETPLACE, CatalogSkill, InstallCatalogSkill, MarketplaceProblem,
    MarketplaceView, SkillCatalog,
};
pub use skills::{SkillMount, SkillScope, SkillView};
pub use usage::{
    CloudSpend, CloudUsageView, HarnessObservation, LlmUsageView, OBSERVATION_WINDOW_SECONDS,
    RateLimitObservation,
};
pub use wire::{
    ApprovalDecision, ApprovalPayload, ClientEvent, ControlToDaemon, DaemonToControl,
    DesktopButton, DesktopInputEvent, DesktopStatus, HarnessCommand, MessageOrigin,
    ProvisioningStage, ReportProvisioningStage, ReportSpotNotice, ReportStartupFailure,
    ReportStopping, ShellOutcome, ShellStream, StopReason, UsageWindow, blocking_window,
};
pub use workdir::{
    DIFF_PATCH_BYTES_MAX, DIRECTORY_ENTRIES_MAX, DirectoryEntry, DirectoryListing, EntryKind,
    FILE_BYTES_MAX, FileChange, FileContent, FileDiff, WorkdirDiff, WorkdirRefusal, WorkdirReply,
    WorkdirRequest,
};

/// Version of the daemon⇄control-plane wire protocol.
///
/// Bumped on every incompatible change to [`wire`]; the control plane
/// refuses daemons speaking a different version (fast fail, no
/// best-effort compatibility).
pub const WIRE_PROTOCOL_VERSION: u32 = 18;
