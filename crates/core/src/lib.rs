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
pub mod harness;
pub mod id;
pub mod machine;
pub mod mcp;
pub mod memory;
pub mod money;
pub mod problem;
pub mod providers;
pub mod push;
pub mod repo;
pub mod session;
pub mod skills;
#[cfg(feature = "sql")]
pub mod sql;
pub mod usage;
pub mod wire;

pub use agents::{AgentsDocument, UpdateAgentsDocument};
pub use approval::{ApprovalState, ApprovalView, DecideApproval};
pub use auth::{
    ApiKeySummary, AuthorizeUrl, CreateApiKey, CreatedApiKey, CurrentUser, DAEMON_TOKEN_PREFIX,
    DaemonToken, SESSION_CAP_DEFAULT, SESSION_CAP_MAX, SESSION_CAP_MIN, UpdateMe,
};
pub use budget::{
    BudgetConfig, BudgetSignal, BudgetStage, BudgetState, BudgetView, SpendEvent, SpendKind,
};
pub use catalog::curate;
pub use env::{EnvDocument, EnvEntry, NETWORK_CONTROL_WARNING, UpdateEnv};
pub use github::{BranchPage, BranchSummary, RepoSummary};
pub use harness::{
    Availability, ClaudeOauthStart, CompleteClaudeOauth, ContextWindow, Feature,
    HarnessAccountView, HarnessCredentialInput, HarnessEvent, HarnessFeature, HarnessKind,
    LinkHarnessAccount, PermissionMode, UsageReport, availability, matrix,
};
pub use id::{
    ApiKeyId, ApprovalId, BudgetId, BudgetSignalId, ClaudeOauthAttemptId, HarnessAccountId,
    HarnessObservationId, Id, MachineId, McpServerId, MemoryNodeId, ProviderAccountId,
    PushSubscriptionId, SessionId, SkillId, SpendEventId, UserId,
};
pub use machine::{
    AUTO_MIN_MEMORY_MIB, AUTO_MIN_VCPUS, AgentMachineView, BillingMinimum, CloudProviderKind,
    CpuArchitecture, MachineCapacity, MachineCatalogEntry, MachineDefault, MachineLineage,
    MachinePricing, MachineSpec, MachineState, MachineView, OsFamily, ResizeMachine,
    SessionMachine, StoragePriceTier, StoragePricing, auto_linux_choice,
};
pub use mcp::{HeaderEntry, McpServerConfig, McpServerView, UpsertMcpServer};
pub use memory::{CreateMemoryNode, MemoryNode, UpdateMemoryNode};
pub use money::Usd;
pub use problem::Problem;
pub use providers::{
    AwsIamPolicy, LinkProvider, ProviderAccountView, ProviderBonusHint, ProviderCredentials,
    QuickstartAnswers,
};
pub use push::{PushKeys, PushSubscription, PushSubscriptionView, VapidPublicKey};
pub use repo::{BranchName, BranchNameError, RepoSlug, RepoStatus};
pub use session::{
    ARCHIVE_AFTER_IDLE_SECS, CreateSession, DEFAULT_DISK_GIB, MAX_SESSION_TITLE_CHARS,
    MachineChoice, MachineOrigin, PROMPT_EXCERPT_CHARS, SendMessage, SessionDetail, SessionState,
    SessionSummary, SessionTransitionError, TurnPage, TurnSummary, UpdateSession, excerpt,
};
pub use skills::{SkillScope, SkillView};
pub use usage::{
    CloudSpend, CloudUsageView, HarnessObservation, LlmUsageView, OBSERVATION_WINDOW_SECONDS,
    RateLimitObservation,
};
pub use wire::{
    ApprovalDecision, ApprovalPayload, ClientEvent, ControlToDaemon, DaemonToControl,
    ProvisioningStage, ReportProvisioningStage,
};

/// Version of the daemon⇄control-plane wire protocol.
///
/// Bumped on every incompatible change to [`wire`]; the control plane
/// refuses daemons speaking a different version (fast fail, no
/// best-effort compatibility).
pub const WIRE_PROTOCOL_VERSION: u32 = 4;
