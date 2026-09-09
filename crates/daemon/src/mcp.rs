//! `flycod`'s local MCP server: the only way an agent acts on its session.
//!
//! An agent on a flyco machine cannot configure MCP for itself — the harness
//! config is root-owned and the server allowlist is enforced on the machine
//! (docs/ARCHITECTURE.md) — so everything it is allowed to do beyond editing
//! files comes through here. This module carries the machine half of that
//! surface: what machine the session is on, what the budget has left, and
//! moving to another machine.
//!
//! # A second process, on purpose
//!
//! The harness launches this over stdio (`flycod mcp --config …`), so it is
//! a different process from the `flycod run` that supervises the harness and
//! holds the relay. It therefore shares no memory with it, and every fact it
//! reports comes from the control plane rather than from the daemon's own
//! state. That is the right shape rather than a limitation: a resize
//! restarts the machine, so the *config file* on disk describes the machine
//! the session booted on, not the one it is on now. Asking the control plane
//! is the only answer that is still true after a resize.
//!
//! What the config does carry, and what nothing else could tell this
//! process, is
//! [`machine_origin`](crate::config::DaemonConfig::machine_origin): who
//! chose the machine. It is a fact about the session and does not move.
//!
//! # What is enforced here and what is enforced by flyco
//!
//! The dirty-tree refusal is enforced here, because the working tree is on
//! *this* machine and the control plane cannot see it. The licence gate is
//! not: the tool raises an approval rather than resizing, and the control
//! plane refuses a license-bound resize on a daemon's authority anyway
//! ([`ApiError::LicenseBoundResizeNeedsApproval`]). A rule an agent could
//! talk its way past is not a rule.
//!
//! [`ApiError::LicenseBoundResizeNeedsApproval`]: https://github.com/lexoliu/flyco/blob/main/crates/api/src/error.rs

use std::future::Future;
use std::sync::Arc;

use askama::Template as _;
use flyco_core::wire::ApprovalPayload;
use flyco_core::{MachineOrigin, SessionMachine};
use rmcp::handler::server::common::{schema_for_empty_input, schema_for_input};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::schemars::{self, JsonSchema};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde::Deserialize;

use crate::control::rest::{AgentApi, ControlApiError};
use crate::git::{GitError, GitWorkdir};
use crate::notice::{
    BudgetStatus, MachineLine, MachineStatus, ResizeAccepted, ResizeDescription, ResizePending,
    ResizeRefusedDirty,
};

/// Name of the tool that reports the machine.
pub const MACHINE_STATUS: &str = "machine_status";

/// Name of the tool that reports the compute budget.
pub const BUDGET_STATUS: &str = "budget_status";

/// Name of the tool that moves the session onto another machine.
pub const MACHINE_RESIZE: &str = "machine_resize";

/// The working tree, as the resize tool needs to see it.
///
/// Narrower than [`WorkingTree`](crate::git::WorkingTree), which is a
/// long-lived watcher with a snapshot handle: this process asks once,
/// answers one tool call, and exits. Stated as a trait so the refusal can be
/// tested without a checkout on disk.
pub trait TreeStatus: Send + Sync {
    /// `git status --short` as it stands right now. Empty is a clean tree.
    fn status(&self) -> impl Future<Output = Result<String, GitError>> + Send;
}

impl TreeStatus for GitWorkdir {
    async fn status(&self) -> Result<String, GitError> {
        self.read_status().await
    }
}

/// What `machine_resize` takes.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ResizeInput {
    /// Provider-native machine type to move to, from the list in this
    /// tool's description.
    pub machine_type: String,
    /// Why this session needs that machine.
    ///
    /// Required rather than optional, and not decoration: it is what the
    /// user reads on the approval card for a license-bound type, and what
    /// justifies resizing over a dirty tree.
    pub reason: String,
    /// Resize even though the working tree has uncommitted changes.
    ///
    /// The changes survive — they are on the disk, and the disk survives —
    /// but every running process dies with the machine.
    #[serde(default)]
    pub force: bool,
}

/// Why a tool call could not be answered at all.
///
/// Distinct from a *refusal*, which is an answer: a refusal is a
/// [`CallToolResult`] the agent can read and act on, and this is the control
/// plane being unreachable or a schema the agent did not honour.
#[derive(Debug, thiserror::Error)]
enum ToolFailure {
    /// The control plane could not be reached or refused the call.
    #[error(transparent)]
    ControlApi(#[from] ControlApiError),
    /// The working tree could not be read.
    #[error(transparent)]
    Git(#[from] GitError),
    /// The arguments did not match the tool's schema.
    #[error("`{tool}` was called with arguments it does not accept: {detail}")]
    Arguments {
        /// Tool that was called.
        tool: &'static str,
        /// What was wrong with them.
        detail: String,
    },
    /// A tool nobody defined.
    #[error("this server has no tool called `{0}`")]
    UnknownTool(String),
    /// A notice could not be rendered, which is this daemon's own bug.
    #[error("a flyco notice could not be rendered: {0}")]
    Render(#[from] askama::Error),
}

impl From<ToolFailure> for ErrorData {
    fn from(failure: ToolFailure) -> Self {
        match failure {
            // Arguments the schema does not admit, and a tool that does not
            // exist, are both the caller's mistake and are answered as such;
            // everything else is flyco failing to answer.
            argued @ (ToolFailure::Arguments { .. } | ToolFailure::UnknownTool(_)) => {
                Self::invalid_params(argued.to_string(), None)
            }
            other => Self::internal_error(other.to_string(), None),
        }
    }
}

/// flyco's tools, over one session's stdio.
///
/// Generic over the control plane and the working tree so both can be
/// replaced in a test; the shipped server is
/// `FlycoTools<HttpControlApi, GitWorkdir>`.
pub struct FlycoTools<A, T> {
    api: A,
    tree: T,
    origin: MachineOrigin,
}

impl<A, T> core::fmt::Debug for FlycoTools<A, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FlycoTools")
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

impl<A: AgentApi, T: TreeStatus + 'static> FlycoTools<A, T> {
    /// Builds the server for one session.
    ///
    /// `origin` comes from the daemon's configuration because nothing else
    /// on the machine knows it and it never changes: it says who decided
    /// this session would run on a machine of its own choosing.
    pub const fn new(api: A, tree: T, origin: MachineOrigin) -> Self {
        Self { api, tree, origin }
    }

    /// The three tools, described against this session's own catalog.
    ///
    /// `machine_resize` names the types it will accept, with prices, so the
    /// agent chooses from the curated list rather than from a memory of what
    /// a cloud sells. That means reading the catalog to answer
    /// `tools/list` — and it means a catalog that cannot be read fails the
    /// listing rather than advertising a tool that cannot say what it does.
    async fn tools(&self) -> Result<Vec<Tool>, ToolFailure> {
        let machine = self.api.agent_machine().await?;
        let catalog: Vec<SessionMachine> = self
            .api
            .agent_machine_catalog()
            .await?
            .iter()
            .map(|entry| SessionMachine::of(entry, machine.machine.spot))
            .collect();

        Ok(vec![
            tool(
                MACHINE_STATUS,
                include_str!("../templates/tool_machine_status.md"),
                schema_for_empty_input(),
            ),
            tool(
                BUDGET_STATUS,
                include_str!("../templates/tool_budget_status.md"),
                schema_for_empty_input(),
            ),
            tool(
                MACHINE_RESIZE,
                &ResizeDescription::new(&catalog, self.origin).render()?,
                schema_for_input::<ResizeInput>().map_err(|detail| ToolFailure::Arguments {
                    tool: MACHINE_RESIZE,
                    detail,
                })?,
            ),
        ])
    }

    /// Answers one tool call.
    async fn call(&self, request: CallToolRequestParams) -> Result<CallToolResult, ToolFailure> {
        match request.name.as_ref() {
            MACHINE_STATUS => self.machine_status().await,
            BUDGET_STATUS => self.budget_status().await,
            MACHINE_RESIZE => {
                self.machine_resize(arguments(MACHINE_RESIZE, request.arguments)?)
                    .await
            }
            other => Err(ToolFailure::UnknownTool(other.to_owned())),
        }
    }

    async fn machine_status(&self) -> Result<CallToolResult, ToolFailure> {
        let view = self.api.agent_machine().await?;
        Ok(answer(&MachineStatus::of(&view).render()?))
    }

    async fn budget_status(&self) -> Result<CallToolResult, ToolFailure> {
        let budget = self.api.agent_budget().await?;
        Ok(answer(&BudgetStatus::of(&budget).render()?))
    }

    /// Moves the session onto another machine, or explains why it did not.
    ///
    /// Three answers, in the order the checks have to happen: the type has
    /// to be one this session can become, the work has to be safe to
    /// restart on, and a type that bills a minimum on boot is the user's
    /// decision rather than the agent's.
    async fn machine_resize(&self, input: ResizeInput) -> Result<CallToolResult, ToolFailure> {
        let current = self.api.agent_machine().await?;
        let catalog = self.api.agent_machine_catalog().await?;
        let Some(entry) = catalog
            .iter()
            .find(|entry| entry.machine_type == input.machine_type)
        else {
            return Ok(refusal(&format!(
                "`{}` is not one of the machine types this session can move to. \
                 The types it can move to are listed in this tool's description.",
                input.machine_type
            )));
        };
        let wanted = SessionMachine::of(entry, current.machine.spot);

        // The working tree is on this machine and the control plane cannot
        // see it, so this check lives here and nowhere else.
        let summary = self.tree.status().await?;
        if !summary.trim().is_empty() && !input.force {
            return Ok(refusal(&ResizeRefusedDirty { summary }.render()?));
        }

        if let Some(minimum) = wanted.minimum {
            let id = self
                .api
                .raise_approval(ApprovalPayload::MachineResizeLicenseBound {
                    machine_type: wanted.machine_type.clone(),
                    minimum,
                    reason: input.reason,
                })
                .await?;
            tracing::info!(
                approval = %id,
                machine_type = %wanted.machine_type,
                "raised an approval for a license-bound resize"
            );
            return Ok(answer(
                &ResizePending {
                    machine_type: wanted.machine_type.clone(),
                    minimum: MachineLine::of(&wanted)
                        .minimum
                        .unwrap_or_else(|| minimum.charge.to_string()),
                    current_type: current.machine.machine_type,
                }
                .render()?,
            ));
        }

        self.api.resize_machine(&wanted.machine_type).await?;
        tracing::info!(
            machine_type = %wanted.machine_type,
            reason = %input.reason,
            forced = input.force,
            "the agent resized this session's machine"
        );
        Ok(answer(
            &ResizeAccepted {
                line: MachineLine::of(&wanted),
            }
            .render()?,
        ))
    }
}

/// One tool definition.
///
/// The description is trimmed because it comes out of a template file, which
/// ends with a newline the way every text file does, and a tool listing is
/// not the place to argue about trailing whitespace.
fn tool(name: &'static str, description: &str, input_schema: Arc<JsonObject>) -> Tool {
    Tool::new(name, description.trim_end().to_owned(), input_schema)
}

/// Reads a call's arguments, or says what was wrong with them.
fn arguments(
    tool: &'static str,
    arguments: Option<JsonObject>,
) -> Result<ResizeInput, ToolFailure> {
    serde_json::from_value(serde_json::Value::Object(arguments.unwrap_or_default())).map_err(
        |error| ToolFailure::Arguments {
            tool,
            detail: error.to_string(),
        },
    )
}

/// A tool call that answered.
fn answer(text: &str) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text.trim_end())])
}

/// A tool call that refused, in terms the agent can act on.
fn refusal(text: &str) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(text.trim_end())])
}

impl<A: AgentApi, T: TreeStatus + 'static> ServerHandler for FlycoTools<A, T> {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("flyco", env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.tools().await?))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        Ok(self.call(request).await?.into())
    }
}

#[cfg(test)]
mod tests {
    use core::future::{Future, ready};
    use std::sync::Mutex;

    use flyco_core::wire::ApprovalPayload;
    use flyco_core::{
        AgentMachineView, ApprovalId, BillingMinimum, BudgetStage, BudgetView, CloudProviderKind,
        MachineCapacity, MachineCatalogEntry, MachineOrigin, MachinePricing, MachineState,
        OsFamily, Runtime, SessionMachine, StoragePricing, Usd,
    };

    use super::{
        BUDGET_STATUS, CallToolResult, ContentBlock, FlycoTools, MACHINE_RESIZE, MACHINE_STATUS,
        ResizeInput, TreeStatus,
    };
    use crate::control::rest::{AgentApi, ApprovalRaiser, ControlApiError};
    use crate::git::GitError;

    /// A control plane that answers from memory and remembers what it was
    /// asked to do.
    ///
    /// A `Mutex` around two `Vec`s rather than channels: these are
    /// assertions read *after* the call under test finished, and a channel
    /// would be a queue nobody is draining.
    #[derive(Debug)]
    struct FakePlane {
        catalog: Vec<MachineCatalogEntry>,
        machine: AgentMachineView,
        approvals: Mutex<Vec<ApprovalPayload>>,
        resizes: Mutex<Vec<String>>,
    }

    impl ApprovalRaiser for FakePlane {
        fn raise_approval(
            &self,
            payload: ApprovalPayload,
        ) -> impl Future<Output = Result<ApprovalId, ControlApiError>> + Send {
            self.approvals.lock().expect("not poisoned").push(payload);
            ready(Ok(ApprovalId::generate()))
        }
    }

    impl AgentApi for FakePlane {
        fn agent_machine(
            &self,
        ) -> impl Future<Output = Result<AgentMachineView, ControlApiError>> + Send {
            ready(Ok(self.machine.clone()))
        }

        fn agent_machine_catalog(
            &self,
        ) -> impl Future<Output = Result<Vec<MachineCatalogEntry>, ControlApiError>> + Send
        {
            ready(Ok(self.catalog.clone()))
        }

        fn resize_machine(
            &self,
            machine_type: &str,
        ) -> impl Future<Output = Result<(), ControlApiError>> + Send {
            self.resizes
                .lock()
                .expect("not poisoned")
                .push(machine_type.to_owned());
            ready(Ok(()))
        }

        fn agent_budget(&self) -> impl Future<Output = Result<BudgetView, ControlApiError>> + Send {
            ready(Ok(BudgetView {
                limit: Usd::from_dollars(10),
                spent: Usd::from_dollars(2),
                remaining: Usd::from_dollars(8),
                stage: BudgetStage::Ok,
            }))
        }
    }

    /// A working tree that is whatever the test says it is.
    #[derive(Debug)]
    struct FakeTree(&'static str);

    impl TreeStatus for FakeTree {
        fn status(&self) -> impl Future<Output = Result<String, GitError>> + Send {
            ready(Ok(self.0.to_owned()))
        }
    }

    fn entry(machine_type: &str, cents: u64, minimum_hours: Option<u32>) -> MachineCatalogEntry {
        MachineCatalogEntry {
            provider: CloudProviderKind::Aws,
            account: None,
            region: "us-east-1".to_owned(),
            machine_type: machine_type.to_owned(),
            runtime: Runtime::Vm,
            free_grant: None,
            os: OsFamily::Linux,
            capacity: Some(MachineCapacity {
                vcpus: 8,
                memory_mib: 32 * 1024,
            }),
            lineage: None,
            pricing: MachinePricing::Metered {
                on_demand_hourly: Usd::from_cents(cents),
                spot_hourly: None,
                minimum: minimum_hours
                    .map(|hours| BillingMinimum::new(hours, Usd::from_cents(cents))),
                storage: StoragePricing::PerGibHourly {
                    rate: Usd::from_micros(100),
                },
            },
        }
    }

    fn tools(tree: &'static str, origin: MachineOrigin) -> FlycoTools<FakePlane, FakeTree> {
        FlycoTools::new(
            FakePlane {
                catalog: vec![
                    entry("m7i.2xlarge", 40, None),
                    entry("mac2.metal", 65, Some(24)),
                ],
                machine: AgentMachineView {
                    origin,
                    machine: SessionMachine {
                        machine_type: "m7i.xlarge".to_owned(),
                        hourly: Some(Usd::from_cents(20)),
                        spot: false,
                        capacity: Some(MachineCapacity {
                            vcpus: 4,
                            memory_mib: 16 * 1024,
                        }),
                        minimum: None,
                    },
                    state: MachineState::Running,
                    region: "us-east-1".to_owned(),
                },
                approvals: Mutex::new(Vec::new()),
                resizes: Mutex::new(Vec::new()),
            },
            FakeTree(tree),
            origin,
        )
    }

    fn resize(machine_type: &str, force: bool) -> ResizeInput {
        ResizeInput {
            machine_type: machine_type.to_owned(),
            reason: "the test suite needs more cores".to_owned(),
            force,
        }
    }

    fn text(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn the_three_tools_are_named_and_described_as_the_contract_says() {
        let listed = tools("", MachineOrigin::Auto).tools().await.expect("list");
        let names: Vec<&str> = listed.iter().map(|tool| tool.name.as_ref()).collect();
        assert_eq!(names, [MACHINE_STATUS, BUDGET_STATUS, MACHINE_RESIZE]);

        let described = listed
            .iter()
            .find(|tool| tool.name == MACHINE_RESIZE)
            .and_then(|tool| tool.description.clone())
            .expect("the resize tool describes itself");

        // The three facts a resize costs, in the tool that costs them.
        assert!(described.contains("Resizing restarts the machine."));
        assert!(described.contains("The disk survives untouched"));
        assert!(described.contains("refuses while the working tree has uncommitted changes"));
        assert!(described.contains("`force: true`"));
        // The curated catalog, priced, with the licence minimum in dollars.
        assert!(described.contains("- m7i.2xlarge · 8 vCPU / 32 GiB · $0.40/hr"));
        assert!(described.contains(
            "- mac2.metal · 8 vCPU / 32 GiB · $0.65/hr · starts a 24-hour minimum charge of \
             $15.60 the moment it boots"
        ));
    }

    #[tokio::test]
    async fn the_resize_schema_takes_a_type_a_reason_and_an_optional_force() {
        let listed = tools("", MachineOrigin::Auto).tools().await.expect("list");
        let schema = listed
            .iter()
            .find(|tool| tool.name == MACHINE_RESIZE)
            .map(|tool| tool.input_schema.clone())
            .expect("the resize tool");

        let properties = schema["properties"].as_object().expect("an object schema");
        assert!(properties.contains_key("machine_type"));
        assert!(properties.contains_key("reason"));
        assert!(properties.contains_key("force"));

        let mut required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required names")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        required.sort_unstable();
        assert_eq!(required, ["machine_type", "reason"]);
    }

    #[tokio::test]
    async fn a_tool_listing_says_the_user_chose_the_machine_when_they_did() {
        let chosen = tools("", MachineOrigin::User).tools().await.expect("list");
        let described = chosen
            .iter()
            .find(|tool| tool.name == MACHINE_RESIZE)
            .and_then(|tool| tool.description.clone())
            .expect("a description");
        assert!(described.contains(
            "The user chose this machine themselves; do not switch it unless the task cannot \
             proceed on it, and say why when you do."
        ));
    }

    #[tokio::test]
    async fn a_resize_over_uncommitted_work_is_refused_until_it_is_forced() {
        let dirty = tools(" M crates/daemon/src/mcp.rs\n", MachineOrigin::Auto);

        let refused = dirty
            .machine_resize(resize("m7i.2xlarge", false))
            .await
            .expect("a refusal is an answer");
        assert_eq!(refused.is_error, Some(true));
        let said = text(&refused);
        assert!(said.starts_with("Refused: the working tree has uncommitted changes."));
        assert!(said.contains(" M crates/daemon/src/mcp.rs"));
        assert!(dirty.api.resizes.lock().expect("not poisoned").is_empty());

        let forced = dirty
            .machine_resize(resize("m7i.2xlarge", true))
            .await
            .expect("a forced resize is performed");
        assert_ne!(forced.is_error, Some(true));
        assert_eq!(
            dirty.api.resizes.lock().expect("not poisoned").as_slice(),
            ["m7i.2xlarge"]
        );
    }

    #[tokio::test]
    async fn a_clean_tree_resizes_and_says_what_the_restart_costs() {
        let clean = tools("", MachineOrigin::Auto);
        let done = clean
            .machine_resize(resize("m7i.2xlarge", false))
            .await
            .expect("resize");

        assert_ne!(done.is_error, Some(true));
        let said = text(&done);
        assert!(said.contains("Resizing this session onto m7i.2xlarge"));
        assert!(said.contains("The machine restarts now"));
        assert!(said.contains("The disk is kept"));
        assert_eq!(
            clean.api.resizes.lock().expect("not poisoned").as_slice(),
            ["m7i.2xlarge"]
        );
    }

    #[tokio::test]
    async fn a_license_bound_type_becomes_an_approval_rather_than_a_resize() {
        let clean = tools("", MachineOrigin::Auto);
        let pending = clean
            .machine_resize(resize("mac2.metal", false))
            .await
            .expect("an approval is an answer");

        // Nothing was resized: the money is the user's to spend.
        assert!(clean.api.resizes.lock().expect("not poisoned").is_empty());
        let raised = clean.api.approvals.lock().expect("not poisoned").clone();
        assert_eq!(
            raised,
            [ApprovalPayload::MachineResizeLicenseBound {
                machine_type: "mac2.metal".to_owned(),
                minimum: BillingMinimum::new(24, Usd::from_cents(65)),
                reason: "the test suite needs more cores".to_owned(),
            }]
        );

        let said = text(&pending);
        assert!(said.starts_with("Pending the user's approval."));
        assert!(said.contains("$15.60"));
        assert!(said.contains("m7i.xlarge"));
    }

    #[tokio::test]
    async fn a_type_outside_the_catalog_is_refused_rather_than_attempted() {
        let clean = tools("", MachineOrigin::Auto);
        let refused = clean
            .machine_resize(resize("x1e.32xlarge", false))
            .await
            .expect("a refusal is an answer");

        assert_eq!(refused.is_error, Some(true));
        assert!(text(&refused).contains("`x1e.32xlarge` is not one of the machine types"));
        assert!(clean.api.resizes.lock().expect("not poisoned").is_empty());
    }

    #[tokio::test]
    async fn machine_status_reports_the_live_machine_and_who_chose_it() {
        let said = text(
            &tools("", MachineOrigin::User)
                .machine_status()
                .await
                .expect("status"),
        );
        assert!(said.contains("m7i.xlarge · 4 vCPU / 16 GiB · $0.20/hr"));
        assert!(said.contains("in us-east-1"));
        assert!(said.contains("The user chose this machine themselves"));
    }

    #[tokio::test]
    async fn budget_status_reports_what_is_left() {
        let said = text(
            &tools("", MachineOrigin::Auto)
                .budget_status()
                .await
                .expect("budget"),
        );
        assert!(said.contains("spent $2.00 of its $10.00 compute budget, leaving $8.00"));
    }
}
