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

pub mod approval;
pub mod auth;
pub mod budget;
pub mod harness;
pub mod id;
pub mod machine;
pub mod memory;
pub mod money;
pub mod problem;
pub mod repo;
pub mod session;
pub mod wire;

pub use approval::{ApprovalState, ApprovalView, DecideApproval};
pub use auth::{
    ApiKeySummary, AuthorizeUrl, CreateApiKey, CreatedApiKey, CurrentUser, DAEMON_TOKEN_PREFIX,
    DaemonToken, SESSION_CAP_DEFAULT, SESSION_CAP_MAX, SESSION_CAP_MIN, UpdateMe,
};
pub use budget::{
    BudgetConfig, BudgetSignal, BudgetStage, BudgetState, BudgetView, SpendEvent, SpendKind,
};
pub use harness::{Availability, ContextWindow, Feature, HarnessEvent, HarnessKind, UsageReport};
pub use id::{
    ApiKeyId, ApprovalId, BudgetId, Id, MachineId, MemoryNodeId, SessionId, SkillId, SpendEventId,
    UserId,
};
pub use machine::{CloudProviderKind, MachineCatalogEntry, MachineSpec, MachineState, OsFamily};
pub use money::Usd;
pub use problem::Problem;
pub use repo::RepoSlug;
pub use session::{
    CreateSession, SessionDetail, SessionState, SessionSummary, SessionTransitionError,
};
pub use wire::{ApprovalDecision, ApprovalPayload, ClientEvent, ControlToDaemon, DaemonToControl};

/// Version of the daemon⇄control-plane wire protocol.
///
/// Bumped on every incompatible change to [`wire`]; the control plane
/// refuses daemons speaking a different version (fast fail, no
/// best-effort compatibility).
pub const WIRE_PROTOCOL_VERSION: u32 = 1;
