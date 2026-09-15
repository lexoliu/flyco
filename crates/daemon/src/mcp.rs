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
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use askama::Template as _;
use flyco_core::wire::{ApprovalPayload, DesktopButton, DesktopInputEvent};
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
use crate::desktop::ipc::{self, AgentReply, AgentRequest, IpcError};
use crate::git::{GitError, GitWorkdir};
use crate::notice::{
    BudgetStatus, MachineLine, MachineStatus, RepoPending, ResizeAccepted, ResizeDescription,
    ResizePending, ResizeRefusedDirty,
};

/// Name of the tool that reports the machine.
pub const MACHINE_STATUS: &str = "machine_status";

/// Name of the tool that reports the compute budget.
pub const BUDGET_STATUS: &str = "budget_status";

/// Name of the tool that moves the session onto another machine.
pub const MACHINE_RESIZE: &str = "machine_resize";

/// Name of the tool that reports the desktop's `DISPLAY`, geometry, and
/// whose hands are on it.
pub const COMPUTER_STATE: &str = "computer_state";

/// Name of the tool that reads the screen.
pub const COMPUTER_SCREENSHOT: &str = "computer_screenshot";

/// Name of the tool that moves the pointer.
pub const COMPUTER_MOVE: &str = "computer_move";

/// Name of the tool that clicks a spot.
pub const COMPUTER_CLICK: &str = "computer_click";

/// Name of the tool that drags between two spots.
pub const COMPUTER_DRAG: &str = "computer_drag";

/// Name of the tool that types text.
pub const COMPUTER_TYPE: &str = "computer_type";

/// Name of the tool that presses one key, optionally under modifiers.
pub const COMPUTER_KEY: &str = "computer_key";

/// Name of the tool that scrolls at a spot.
pub const COMPUTER_SCROLL: &str = "computer_scroll";

/// Name of the tool that waits — the desktop's answer to animations and
/// loads that only time can settle.
pub const COMPUTER_WAIT: &str = "computer_wait";

/// The desktop tools, in the order they are listed.
///
/// A session's flyco server exposes them only while its `computer_use`
/// flag is on, so [`crate::mount`]'s required set learns them
/// conditionally rather than advertising tools that would refuse.
pub const COMPUTER_TOOLS: [&str; 9] = [
    COMPUTER_STATE,
    COMPUTER_SCREENSHOT,
    COMPUTER_MOVE,
    COMPUTER_CLICK,
    COMPUTER_DRAG,
    COMPUTER_TYPE,
    COMPUTER_KEY,
    COMPUTER_SCROLL,
    COMPUTER_WAIT,
];

/// The longest `computer_wait` may hold a tool call.
///
/// A wait is how the agent lets a screen settle — minutes of it is a
/// stalled turn wearing a tool call's clothes, so past this the tool
/// says so rather than sleeping.
const WAIT_CAP_SECONDS: u16 = 120;

/// Name of the tool that asks for another repository in the workspace.
pub const REPO_ADD: &str = "repo_add";

/// The working trees, as the resize tool needs to see them.
///
/// Narrower than [`WorkingSet`](crate::git::WorkingSet), which is a
/// long-lived watcher with a snapshot handle: this process asks once,
/// answers one tool call, and exits. Stated as a trait so the refusal can be
/// tested without a checkout on disk.
pub trait TreeStatus: Send + Sync {
    /// `git status --short` per checkout, as it stands right now.
    ///
    /// Each pair is the checkout's workspace directory — `None` for the
    /// developer-machine shape, where the workspace root is the checkout —
    /// and its summary; an empty summary is a clean tree.
    fn status(
        &self,
    ) -> impl Future<Output = Result<Vec<(Option<String>, String)>, GitError>> + Send;
}

/// The session's checkouts as one-shot status readers.
///
/// `flycod mcp` is a second process beside `flycod run` and shares none of
/// the relay's state, so it answers "is the work safe to restart" by
/// running `git status` itself, once per checkout.
pub struct RepoTrees {
    checkouts: Vec<(Option<String>, GitWorkdir)>,
}

impl core::fmt::Debug for RepoTrees {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RepoTrees")
            .field(
                "dirs",
                &self
                    .checkouts
                    .iter()
                    .map(|(dir, _)| dir)
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl RepoTrees {
    /// One reader per configured `[[repos]]` entry — or the workspace root
    /// alone, keyed `None`, when none are: the developer-machine shape,
    /// where the root itself is the checkout.
    #[must_use]
    pub fn over(workdir: &std::path::Path, repos: &[crate::config::RepoConfig]) -> Self {
        Self {
            checkouts: if repos.is_empty() {
                vec![(None, GitWorkdir::new(workdir.to_owned()))]
            } else {
                repos
                    .iter()
                    .map(|repo| {
                        (
                            Some(repo.dir.clone()),
                            GitWorkdir::new(repo.checkout_path(workdir)),
                        )
                    })
                    .collect()
            },
        }
    }
}

impl TreeStatus for RepoTrees {
    async fn status(&self) -> Result<Vec<(Option<String>, String)>, GitError> {
        let mut statuses = Vec::with_capacity(self.checkouts.len());
        for (dir, checkout) in &self.checkouts {
            statuses.push((dir.clone(), checkout.read_status().await?));
        }
        Ok(statuses)
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

/// The default button a click or a drag uses.
const fn left() -> DesktopButton {
    DesktopButton::Left
}

/// What `computer_move` takes: a spot on the display, in its own
/// pixels.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
pub struct MoveInput {
    /// Display x.
    pub x: u16,
    /// Display y.
    pub y: u16,
}

/// What `computer_click` takes.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
pub struct ClickInput {
    /// Display x.
    pub x: u16,
    /// Display y.
    pub y: u16,
    /// The button to click.
    #[serde(default = "left")]
    pub button: DesktopButton,
}

/// What `computer_drag` takes.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
pub struct DragInput {
    /// Where the drag starts, display x.
    pub from_x: u16,
    /// Display y of the start.
    pub from_y: u16,
    /// Where the drag ends, display x.
    pub to_x: u16,
    /// Display y of the end.
    pub to_y: u16,
    /// The button held through it.
    #[serde(default = "left")]
    pub button: DesktopButton,
}

/// What `computer_type` takes.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TypeInput {
    /// The text to type — one keystroke pair per character.
    pub text: String,
}

/// A modifier `computer_key` holds through a keystroke.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KeyModifier {
    /// Shift.
    Shift,
    /// Control.
    Control,
    /// Alt.
    Alt,
    /// The command or windows key.
    Meta,
}

impl KeyModifier {
    /// The DOM `key` name the display resolves it by — the same name a
    /// browser's `KeyboardEvent.key` reports for the modifier.
    const fn key(self) -> &'static str {
        match self {
            Self::Shift => "Shift",
            Self::Control => "Control",
            Self::Alt => "Alt",
            Self::Meta => "Meta",
        }
    }
}

/// What `computer_key` takes.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct KeyInput {
    /// The DOM `key` name of the key to press: a character or a named
    /// key — `Enter`, `Tab`, `Escape`, `Backspace`, `F5`, `ArrowDown`.
    pub key: String,
    /// Modifiers held through the keystroke — `["control", "c"]` style
    /// chords are spelled `key: "c", modifiers: ["control"]`.
    #[serde(default)]
    pub modifiers: Vec<KeyModifier>,
}

/// What `computer_scroll` takes.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
pub struct ScrollInput {
    /// Display x where the wheel lands.
    pub x: u16,
    /// Display y where the wheel lands.
    pub y: u16,
    /// Horizontal delta, DOM convention — positive scrolls right.
    #[serde(default)]
    pub delta_x: i32,
    /// Vertical delta, DOM convention — positive scrolls down.
    pub delta_y: i32,
}

/// What `computer_wait` takes.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
pub struct WaitInput {
    /// How long to wait, in seconds — at most
    /// [`WAIT_CAP_SECONDS`].
    pub seconds: u16,
}

/// What `repo_add` takes.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RepoAddInput {
    /// Repository in `owner/name` form.
    pub repo: String,
    /// Branch to check out — the repository's default when absent.
    #[serde(default)]
    pub branch: Option<String>,
    /// Why this session needs the repository.
    ///
    /// Required rather than optional: it is what the user reads on the
    /// approval card, and a clone they never picked is their call.
    pub reason: String,
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
    /// The desktop socket answered late, wrongly, or not at all.
    ///
    /// `NoDesktop` never reaches here — a session without a screen is an
    /// expected condition the model reads as a refusal, not this
    /// process's failure to reach one.
    #[error(transparent)]
    Desktop(#[from] IpcError),
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
    /// The desktop's agent socket — `Some` when the session's
    /// `computer_use` flag was on at provision, which is also what makes
    /// the `computer_*` tools listed at all.
    desktop: Option<PathBuf>,
}

impl<A, T> core::fmt::Debug for FlycoTools<A, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FlycoTools")
            .field("origin", &self.origin)
            .field("desktop", &self.desktop.is_some())
            .finish_non_exhaustive()
    }
}

impl<A: AgentApi, T: TreeStatus + 'static> FlycoTools<A, T> {
    /// Builds the server for one session.
    ///
    /// `origin` comes from the daemon's configuration because nothing else
    /// on the machine knows it and it never changes: it says who decided
    /// this session would run on a machine of its own choosing.
    ///
    /// `desktop` is the agent socket's path when the session was
    /// provisioned with a screen; `None` leaves the `computer_*` tools
    /// out of the listing entirely, so a session with no desktop offers
    /// no tools that could only refuse.
    pub const fn new(api: A, tree: T, origin: MachineOrigin, desktop: Option<PathBuf>) -> Self {
        Self {
            api,
            tree,
            origin,
            desktop,
        }
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

        let mut tools = vec![
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
                input_schema::<ResizeInput>(MACHINE_RESIZE)?,
            ),
            tool(
                REPO_ADD,
                include_str!("../templates/tool_repo_add.md"),
                input_schema::<RepoAddInput>(REPO_ADD)?,
            ),
        ];
        if self.desktop.is_some() {
            tools.extend([
                tool(
                    COMPUTER_STATE,
                    include_str!("../templates/tool_computer_state.md"),
                    schema_for_empty_input(),
                ),
                tool(
                    COMPUTER_SCREENSHOT,
                    include_str!("../templates/tool_computer_screenshot.md"),
                    schema_for_empty_input(),
                ),
                tool(
                    COMPUTER_MOVE,
                    include_str!("../templates/tool_computer_move.md"),
                    input_schema::<MoveInput>(COMPUTER_MOVE)?,
                ),
                tool(
                    COMPUTER_CLICK,
                    include_str!("../templates/tool_computer_click.md"),
                    input_schema::<ClickInput>(COMPUTER_CLICK)?,
                ),
                tool(
                    COMPUTER_DRAG,
                    include_str!("../templates/tool_computer_drag.md"),
                    input_schema::<DragInput>(COMPUTER_DRAG)?,
                ),
                tool(
                    COMPUTER_TYPE,
                    include_str!("../templates/tool_computer_type.md"),
                    input_schema::<TypeInput>(COMPUTER_TYPE)?,
                ),
                tool(
                    COMPUTER_KEY,
                    include_str!("../templates/tool_computer_key.md"),
                    input_schema::<KeyInput>(COMPUTER_KEY)?,
                ),
                tool(
                    COMPUTER_SCROLL,
                    include_str!("../templates/tool_computer_scroll.md"),
                    input_schema::<ScrollInput>(COMPUTER_SCROLL)?,
                ),
                tool(
                    COMPUTER_WAIT,
                    include_str!("../templates/tool_computer_wait.md"),
                    input_schema::<WaitInput>(COMPUTER_WAIT)?,
                ),
            ]);
        }
        Ok(tools)
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
            REPO_ADD => self.repo_add(arguments(REPO_ADD, request.arguments)?).await,
            name if COMPUTER_TOOLS.contains(&name) && self.desktop.is_some() => {
                self.computer(name, request.arguments).await
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

        // The working trees are on this machine and the control plane
        // cannot see them, so this check lives here and nowhere else.
        let dirty: Vec<(String, String)> = self
            .tree
            .status()
            .await?
            .into_iter()
            .filter(|(_, summary)| !summary.trim().is_empty())
            .map(|(dir, summary)| (dir.unwrap_or_else(|| ".".to_owned()), summary))
            .collect();
        if !dirty.is_empty() && !input.force {
            return Ok(refusal(&ResizeRefusedDirty { checkouts: dirty }.render()?));
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

    /// Answers one `computer_*` call.
    ///
    /// Every tool but `computer_wait` is a round-trip to the session's
    /// desktop over its agent socket; `wait` is time, not a display
    /// request, so it answers from here.
    async fn computer(
        &self,
        name: &str,
        args: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolFailure> {
        match name {
            COMPUTER_STATE => self.computer_state().await,
            COMPUTER_SCREENSHOT => self.computer_screenshot().await,
            COMPUTER_MOVE => {
                let input: MoveInput = arguments(COMPUTER_MOVE, args)?;
                self.inject(vec![DesktopInputEvent::Move {
                    x: input.x,
                    y: input.y,
                }])
                .await
            }
            COMPUTER_CLICK => {
                let input: ClickInput = arguments(COMPUTER_CLICK, args)?;
                self.inject(click_events(input)).await
            }
            COMPUTER_DRAG => {
                let input: DragInput = arguments(COMPUTER_DRAG, args)?;
                self.inject(drag_events(input)).await
            }
            COMPUTER_TYPE => {
                let input: TypeInput = arguments(COMPUTER_TYPE, args)?;
                self.inject(type_events(&input.text)).await
            }
            COMPUTER_KEY => {
                let input: KeyInput = arguments(COMPUTER_KEY, args)?;
                self.inject(key_events(&input)).await
            }
            COMPUTER_SCROLL => {
                let input: ScrollInput = arguments(COMPUTER_SCROLL, args)?;
                self.inject(vec![DesktopInputEvent::Scroll {
                    x: input.x,
                    y: input.y,
                    delta_x: input.delta_x,
                    delta_y: input.delta_y,
                }])
                .await
            }
            COMPUTER_WAIT => {
                let input: WaitInput = arguments(COMPUTER_WAIT, args)?;
                if input.seconds > WAIT_CAP_SECONDS {
                    return Ok(refusal(&format!(
                        "a wait may be at most {WAIT_CAP_SECONDS} seconds; \
                         longer is a stalled turn, not a settled screen"
                    )));
                }
                tokio::time::sleep(Duration::from_secs(u64::from(input.seconds))).await;
                Ok(answer("the wait is over — the screen has had its time"))
            }
            other => Err(ToolFailure::UnknownTool(other.to_owned())),
        }
    }

    /// What the desktop is — `DISPLAY`, geometry, and whose hands are
    /// on it — in the sentence the model reads before driving.
    async fn computer_state(&self) -> Result<CallToolResult, ToolFailure> {
        match self.ask(AgentRequest::State).await? {
            AgentReply::State {
                takeover,
                display,
                width,
                height,
            } => Ok(answer(&format!(
                "the desktop is {width}x{height} pixels on DISPLAY={display}; {}",
                if takeover {
                    "the user is driving it — input you send now is refused until control is handed back"
                } else {
                    "you may drive it; launch GUI apps with DISPLAY set to that value"
                }
            ))),
            AgentReply::Refused { reason } => Ok(refusal(&reason)),
            _ => Err(ToolFailure::Desktop(IpcError::Reply(
                "state was answered with the wrong kind of reply".to_owned(),
            ))),
        }
    }

    /// The screen, as the model sees it: a PNG image block at display
    /// resolution.
    async fn computer_screenshot(&self) -> Result<CallToolResult, ToolFailure> {
        match self.ask(AgentRequest::Screenshot).await? {
            AgentReply::Screenshot { png_base64 } => {
                Ok(CallToolResult::success(vec![ContentBlock::image(
                    png_base64,
                    "image/png",
                )]))
            }
            AgentReply::Refused { reason } => Ok(refusal(&reason)),
            _ => Err(ToolFailure::Desktop(IpcError::Reply(
                "a screenshot was answered with the wrong kind of reply".to_owned(),
            ))),
        }
    }

    /// One input batch to the session's desktop.
    ///
    /// The wire shape is the same [`DesktopInputEvent`] the user's
    /// takeover input takes — one event language for both drivers.
    async fn inject(&self, events: Vec<DesktopInputEvent>) -> Result<CallToolResult, ToolFailure> {
        match self.ask(AgentRequest::Input { events }).await? {
            AgentReply::Done => Ok(answer("the input landed on the desktop")),
            AgentReply::Refused { reason } => Ok(refusal(&reason)),
            _ => Err(ToolFailure::Desktop(IpcError::Reply(
                "input was answered with the wrong kind of reply".to_owned(),
            ))),
        }
    }

    /// One round-trip to the session's desktop.
    ///
    /// An absent socket is a session whose flag is off — an expected
    /// condition phrased as the refusal the model reads, not a
    /// transport failure this process reports as its own.
    async fn ask(&self, request: AgentRequest) -> Result<AgentReply, ToolFailure> {
        let Some(socket) = &self.desktop else {
            return Ok(AgentReply::Refused {
                reason: "this session has no desktop".to_owned(),
            });
        };
        match ipc::ask(socket, &request).await {
            Ok(reply) => Ok(reply),
            Err(IpcError::NoDesktop) => Ok(AgentReply::Refused {
                reason: "this session's desktop is not running".to_owned(),
            }),
            Err(error) => Err(ToolFailure::Desktop(error)),
        }
    }

    /// Asks the user to add a repository to this session's workspace.
    ///
    /// The clone is never this tool's to perform: a repository the user
    /// did not pick is code they have not agreed to put on the machine, so
    /// the tool raises an approval and the control plane is what performs
    /// the `AddRepo` once — and only if — it is allowed. The agent's answer
    /// is therefore "asked", never "added": the checkout landing is
    /// announced separately, when the clone finishes.
    ///
    /// The slug and branch are parsed here rather than left to the control
    /// plane because a malformed one is an argument error the agent can fix
    /// and retry — it should not be a card the user reads, decides on, and
    /// only then learns could never have been cloned.
    async fn repo_add(&self, input: RepoAddInput) -> Result<CallToolResult, ToolFailure> {
        let slug =
            input
                .repo
                .parse::<flyco_core::RepoSlug>()
                .map_err(|_| ToolFailure::Arguments {
                    tool: REPO_ADD,
                    detail: format!("`{}` is not a repository in `owner/name` form", input.repo),
                })?;
        let branch = input
            .branch
            .map(|branch| {
                branch
                    .parse::<flyco_core::BranchName>()
                    .map_err(|_| ToolFailure::Arguments {
                        tool: REPO_ADD,
                        detail: format!("`{branch}` is not a branch name git accepts"),
                    })
            })
            .transpose()?;
        let id = self
            .api
            .raise_approval(ApprovalPayload::RepoAdd {
                repo: slug.as_str().to_owned(),
                branch: branch.map(|branch| branch.as_str().to_owned()),
                reason: input.reason,
            })
            .await?;
        tracing::info!(approval = %id, repo = %slug, "raised an approval to add a repository");
        Ok(answer(
            &RepoPending {
                repo: slug.to_string(),
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

/// A tool's input schema, built from its argument type.
fn input_schema<I: JsonSchema + 'static>(
    name: &'static str,
) -> Result<Arc<JsonObject>, ToolFailure> {
    schema_for_input::<I>().map_err(|detail| ToolFailure::Arguments { tool: name, detail })
}

/// Reads a call's arguments, or says what was wrong with them.
fn arguments<I: for<'de> Deserialize<'de>>(
    tool: &'static str,
    arguments: Option<JsonObject>,
) -> Result<I, ToolFailure> {
    serde_json::from_value(serde_json::Value::Object(arguments.unwrap_or_default())).map_err(
        |error| ToolFailure::Arguments {
            tool,
            detail: error.to_string(),
        },
    )
}

/// A click as the display hears it: a motion to the spot, then the
/// button down and up where it lands.
fn click_events(input: ClickInput) -> Vec<DesktopInputEvent> {
    vec![
        DesktopInputEvent::Move {
            x: input.x,
            y: input.y,
        },
        DesktopInputEvent::Button {
            x: input.x,
            y: input.y,
            button: input.button,
            pressed: true,
        },
        DesktopInputEvent::Button {
            x: input.x,
            y: input.y,
            button: input.button,
            pressed: false,
        },
    ]
}

/// A drag: the button goes down at the start and up at the end, with
/// the motion between.
fn drag_events(input: DragInput) -> Vec<DesktopInputEvent> {
    vec![
        DesktopInputEvent::Move {
            x: input.from_x,
            y: input.from_y,
        },
        DesktopInputEvent::Button {
            x: input.from_x,
            y: input.from_y,
            button: input.button,
            pressed: true,
        },
        DesktopInputEvent::Move {
            x: input.to_x,
            y: input.to_y,
        },
        DesktopInputEvent::Button {
            x: input.to_x,
            y: input.to_y,
            button: input.button,
            pressed: false,
        },
    ]
}

/// Text as keystrokes — a press and release per character, addressed by
/// the produced `key` so the display picks the keymap level that yields
/// it, shift and all.
fn type_events(text: &str) -> Vec<DesktopInputEvent> {
    text.chars()
        .flat_map(|ch| {
            let key = ch.to_string();
            [
                DesktopInputEvent::Key {
                    code: String::new(),
                    key: key.clone(),
                    pressed: true,
                },
                DesktopInputEvent::Key {
                    code: String::new(),
                    key,
                    pressed: false,
                },
            ]
        })
        .collect()
}

/// One key under its modifiers: modifiers down first and up last, the
/// key's press and release between them — the same order a human's
/// hands produce a chord in.
fn key_events(input: &KeyInput) -> Vec<DesktopInputEvent> {
    let modifiers = input
        .modifiers
        .iter()
        .map(|modifier| modifier.key())
        .collect::<Vec<_>>();
    let mut events = Vec::with_capacity(modifiers.len() * 2 + 2);
    for key in &modifiers {
        events.push(DesktopInputEvent::Key {
            code: String::new(),
            key: (*key).to_owned(),
            pressed: true,
        });
    }
    for pressed in [true, false] {
        events.push(DesktopInputEvent::Key {
            code: String::new(),
            key: input.key.clone(),
            pressed,
        });
    }
    for key in modifiers.iter().rev() {
        events.push(DesktopInputEvent::Key {
            code: String::new(),
            key: (*key).to_owned(),
            pressed: false,
        });
    }
    events
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
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use flyco_core::wire::{ApprovalPayload, DesktopButton, DesktopInputEvent};
    use flyco_core::{
        AgentMachineView, ApprovalId, BillingMinimum, BudgetStage, BudgetView, CloudProviderKind,
        MachineCapacity, MachineCatalogEntry, MachineOrigin, MachinePricing, MachineState,
        OsFamily, Runtime, SessionMachine, StoragePricing, Usd,
    };
    use rmcp::model::CallToolRequestParams;
    use serde_json::json;

    use super::{
        BUDGET_STATUS, COMPUTER_CLICK, COMPUTER_KEY, COMPUTER_MOVE, COMPUTER_SCREENSHOT,
        COMPUTER_STATE, COMPUTER_TOOLS, COMPUTER_TYPE, COMPUTER_WAIT, CallToolResult, ContentBlock,
        FlycoTools, MACHINE_RESIZE, MACHINE_STATUS, REPO_ADD, RepoAddInput, ResizeInput,
        TreeStatus,
    };
    use crate::control::rest::{AgentApi, ApprovalRaiser, ControlApiError};
    use crate::desktop::ipc::{AgentReply, AgentRequest};
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

    /// The session's checkouts as one status string apiece.
    ///
    /// A single `&'static str` is the developer-machine shape — one
    /// checkout, the root, keyed `None` — which is what the resize tests
    /// exercise.
    #[derive(Debug)]
    struct FakeTree(&'static str);

    impl TreeStatus for FakeTree {
        fn status(
            &self,
        ) -> impl Future<Output = Result<Vec<(Option<String>, String)>, GitError>> + Send {
            ready(Ok(vec![(None, self.0.to_owned())]))
        }
    }

    /// A desktop socket that answers every request with one canned reply
    /// and keeps what it heard.
    ///
    /// A real unix listener on a scratch path, because the IPC's contract
    /// is the wire: a test that never serialized a request would prove the
    /// tools agree with themselves rather than with the desktop.
    #[derive(Debug)]
    struct FakeDesktop {
        path: PathBuf,
        heard: Arc<Mutex<Vec<AgentRequest>>>,
    }

    /// Distinct socket names per test process.
    static SOCKETS: AtomicUsize = AtomicUsize::new(0);

    impl FakeDesktop {
        /// Binds a scratch socket and answers `reply` to every request,
        /// on a thread so `ask`'s own runtime is the one under test.
        fn answering(reply: AgentReply) -> Self {
            let path = std::env::temp_dir().join(format!(
                "flycod-test-{}-{}.desktop.sock",
                std::process::id(),
                SOCKETS.fetch_add(1, Ordering::Relaxed),
            ));
            let listener = UnixListener::bind(&path).expect("a scratch socket binds");
            let heard = Arc::new(Mutex::new(Vec::new()));
            let keeping = heard.clone();
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    let Ok(stream) = conn else {
                        continue;
                    };
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        continue;
                    }
                    if let Ok(request) = serde_json::from_str::<AgentRequest>(&line) {
                        keeping.lock().expect("not poisoned").push(request);
                    }
                    let mut body = serde_json::to_vec(&reply).expect("the reply serializes");
                    body.push(b'\n');
                    let _ = reader.get_mut().write_all(&body);
                }
            });
            Self { path, heard }
        }

        /// The requests the socket heard.
        fn heard(&self) -> Vec<AgentRequest> {
            self.heard.lock().expect("not poisoned").clone()
        }
    }

    impl Drop for FakeDesktop {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// A tool call as the harness spells it.
    fn calling(name: &'static str, arguments: serde_json::Value) -> CallToolRequestParams {
        let mut params = CallToolRequestParams::new(name);
        params.arguments = match arguments {
            serde_json::Value::Object(args) => Some(args),
            _ => None,
        };
        params
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
        tools_with_desktop(tree, origin, None)
    }

    fn tools_with_desktop(
        tree: &'static str,
        origin: MachineOrigin,
        desktop: Option<std::path::PathBuf>,
    ) -> FlycoTools<FakePlane, FakeTree> {
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
            desktop,
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
    async fn the_tools_are_named_and_described_as_the_contract_says() {
        let listed = tools("", MachineOrigin::Auto).tools().await.expect("list");
        let names: Vec<&str> = listed.iter().map(|tool| tool.name.as_ref()).collect();
        assert_eq!(
            names,
            [MACHINE_STATUS, BUDGET_STATUS, MACHINE_RESIZE, REPO_ADD]
        );

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
        assert!(said.starts_with("Refused: a working tree has uncommitted changes."));
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

    #[tokio::test]
    async fn a_session_with_a_desktop_lists_the_computer_tools() {
        let listed = tools_with_desktop(
            "",
            MachineOrigin::Auto,
            Some(PathBuf::from("/tmp/flycod-unbound.sock")),
        )
        .tools()
        .await
        .expect("list");
        let names: Vec<&str> = listed.iter().map(|tool| tool.name.as_ref()).collect();
        assert_eq!(names.len(), 4 + COMPUTER_TOOLS.len());
        for name in COMPUTER_TOOLS {
            assert!(names.contains(&name), "{name} is listed");
        }
    }

    fn repo_add(repo: &str, branch: Option<&str>) -> RepoAddInput {
        RepoAddInput {
            repo: repo.to_owned(),
            branch: branch.map(str::to_owned),
            reason: "the task needs the fixture repository".to_owned(),
        }
    }

    #[tokio::test]
    async fn a_click_moves_presses_and_releases_over_the_socket() {
        let desktop = FakeDesktop::answering(AgentReply::Done);
        let server = tools_with_desktop("", MachineOrigin::Auto, Some(desktop.path.clone()));
        let result = server
            .call(calling(COMPUTER_CLICK, json!({"x": 100, "y": 50})))
            .await
            .expect("the call answers");

        assert_eq!(result.is_error, Some(false));
        let heard = desktop.heard();
        assert_eq!(heard.len(), 1);
        let AgentRequest::Input { events } = &heard[0] else {
            panic!("a click is an input request");
        };
        assert_eq!(
            events.as_slice(),
            [
                DesktopInputEvent::Move { x: 100, y: 50 },
                DesktopInputEvent::Button {
                    x: 100,
                    y: 50,
                    button: DesktopButton::Left,
                    pressed: true,
                },
                DesktopInputEvent::Button {
                    x: 100,
                    y: 50,
                    button: DesktopButton::Left,
                    pressed: false,
                },
            ]
        );
    }

    #[tokio::test]
    async fn repo_add_raises_an_approval_and_says_it_is_pending() {
        let tools = tools("", MachineOrigin::Auto);
        let asked = tools
            .repo_add(repo_add("lexoliu/aither", Some("main")))
            .await
            .expect("an approval is an answer");

        assert_ne!(asked.is_error, Some(true));
        let said = text(&asked);
        assert!(said.contains("Asked the user to add `lexoliu/aither`"));
        assert!(said.contains("approval card"));

        let raised = tools.api.approvals.lock().expect("not poisoned").clone();
        assert_eq!(
            raised,
            [ApprovalPayload::RepoAdd {
                repo: "lexoliu/aither".to_owned(),
                branch: Some("main".to_owned()),
                reason: "the task needs the fixture repository".to_owned(),
            }]
        );
    }

    #[tokio::test]
    async fn typing_sends_one_press_release_pair_per_character() {
        let desktop = FakeDesktop::answering(AgentReply::Done);
        let server = tools_with_desktop("", MachineOrigin::Auto, Some(desktop.path.clone()));
        server
            .call(calling(COMPUTER_TYPE, json!({"text": "Hi"})))
            .await
            .expect("the call answers");

        let AgentRequest::Input { events } = &desktop.heard()[0] else {
            panic!("typing is an input request");
        };
        assert_eq!(
            events.as_slice(),
            ["H", "i"]
                .iter()
                .flat_map(|key| [
                    DesktopInputEvent::Key {
                        code: String::new(),
                        key: (*key).to_owned(),
                        pressed: true,
                    },
                    DesktopInputEvent::Key {
                        code: String::new(),
                        key: (*key).to_owned(),
                        pressed: false,
                    },
                ])
                .collect::<Vec<_>>()
                .as_slice()
        );
    }

    #[tokio::test]
    async fn a_key_chord_holds_its_modifiers_around_the_key() {
        let desktop = FakeDesktop::answering(AgentReply::Done);
        let server = tools_with_desktop("", MachineOrigin::Auto, Some(desktop.path.clone()));
        server
            .call(calling(
                COMPUTER_KEY,
                json!({"key": "c", "modifiers": ["control"]}),
            ))
            .await
            .expect("the call answers");

        let AgentRequest::Input { events } = &desktop.heard()[0] else {
            panic!("a chord is an input request");
        };
        let keys: Vec<(&str, bool)> = events
            .iter()
            .map(|event| match event {
                DesktopInputEvent::Key { key, pressed, .. } => (key.as_str(), *pressed),
                other => panic!("a chord is all key events, not {other:?}"),
            })
            .collect();
        assert_eq!(
            keys,
            [
                ("Control", true),
                ("c", true),
                ("c", false),
                ("Control", false),
            ]
        );
    }

    #[tokio::test]
    async fn a_screenshot_comes_back_as_an_image_block() {
        let desktop = FakeDesktop::answering(AgentReply::Screenshot {
            png_base64: "aGk=".to_owned(),
        });
        let server = tools_with_desktop("", MachineOrigin::Auto, Some(desktop.path.clone()));
        let result = server
            .call(calling(COMPUTER_SCREENSHOT, json!({})))
            .await
            .expect("the call answers");

        let Some(ContentBlock::Image(image)) = result.content.first() else {
            panic!("a screenshot is an image, not {:?}", result.content);
        };
        assert_eq!(image.data, "aGk=");
        assert_eq!(image.mime_type, "image/png");
    }

    #[tokio::test]
    async fn computer_state_names_the_display_and_whose_hands_are_on_it() {
        let desktop = FakeDesktop::answering(AgentReply::State {
            takeover: true,
            display: ":99".to_owned(),
            width: 1280,
            height: 800,
        });
        let server = tools_with_desktop("", MachineOrigin::Auto, Some(desktop.path.clone()));
        let said = text(
            &server
                .call(calling(COMPUTER_STATE, json!({})))
                .await
                .expect("the call answers"),
        );
        assert!(said.contains("1280x800"));
        assert!(said.contains("DISPLAY=:99"));
        assert!(said.contains("the user is driving"));
    }

    #[tokio::test]
    async fn a_desktop_under_takeover_refuses_the_agent_in_its_own_words() {
        let desktop = FakeDesktop::answering(AgentReply::Refused {
            reason: "the user is driving the desktop".to_owned(),
        });
        let server = tools_with_desktop("", MachineOrigin::Auto, Some(desktop.path.clone()));
        let refused = server
            .call(calling(COMPUTER_MOVE, json!({"x": 1, "y": 1})))
            .await
            .expect("a refusal is an answer");
        assert_eq!(refused.is_error, Some(true));
        assert!(text(&refused).contains("the user is driving the desktop"));
    }

    #[tokio::test]
    async fn an_unbound_desktop_socket_is_a_refusal_not_an_error() {
        let path = std::env::temp_dir().join(format!(
            "flycod-test-{}-never.desktop.sock",
            std::process::id()
        ));
        let server = tools_with_desktop("", MachineOrigin::Auto, Some(path));
        let refused = server
            .call(calling(COMPUTER_MOVE, json!({"x": 1, "y": 1})))
            .await
            .expect("a refusal is an answer");
        assert_eq!(refused.is_error, Some(true));
        assert!(text(&refused).contains("desktop is not running"));
    }

    #[tokio::test]
    async fn computer_wait_refuses_to_hold_past_its_cap() {
        let server = tools_with_desktop(
            "",
            MachineOrigin::Auto,
            Some(PathBuf::from("/tmp/flycod-unbound.sock")),
        );
        let refused = server
            .call(calling(COMPUTER_WAIT, json!({"seconds": 121})))
            .await
            .expect("a refusal is an answer");
        assert_eq!(refused.is_error, Some(true));
        assert!(text(&refused).contains("at most 120 seconds"));
    }

    #[tokio::test]
    async fn repo_add_refuses_arguments_no_card_should_carry() {
        let tools = tools("", MachineOrigin::Auto);

        let bad_slug = tools.repo_add(repo_add("not-a-slug", None)).await;
        assert!(
            matches!(
                bad_slug,
                Err(crate::mcp::ToolFailure::Arguments { tool: REPO_ADD, .. })
            ),
            "a malformed slug is an argument error, not an approval card: {bad_slug:?}"
        );
        let bad_branch = tools.repo_add(repo_add("lexoliu/aither", Some("-f"))).await;
        assert!(
            matches!(
                bad_branch,
                Err(crate::mcp::ToolFailure::Arguments { tool: REPO_ADD, .. })
            ),
            "a branch git would read as a flag is an argument error: {bad_branch:?}"
        );
        assert!(tools.api.approvals.lock().expect("not poisoned").is_empty());
    }
}
