//! The generic ACP driver — every harness but Claude Code.
//!
//! ACP, the Agent Client Protocol, is how flycod talks to Codex, to Devin,
//! and to any other agent a config points it at: a long-lived JSON-RPC
//! process over stdio. One driver serves them all because the protocol
//! standardizes the conversation — `initialize`, `session/new`,
//! `session/prompt`, `session/update` — and what it does not standardize
//! is written down in the `[acp]` table rather than in vendor branches
//! here: which config-option ids carry the model and the effort, how each
//! flyco permission mode is expressed, and which extension methods answer
//! compaction, plan usage, and MCP introspection.
//!
//! # Shape
//!
//! [`aither_acp::AcpClient`] owns the JSON-RPC routing, so this driver is
//! thinner than a hand-rolled protocol loop: the agent's notifications and
//! requests arrive on [`AgentHandler`], which forwards them as
//! [`AgentEvent`]s to the one task that owns session state. Prompts and
//! extension calls are futures spawned onto that same channel, so the
//! driver loop keeps reading updates while a turn is in flight. There is
//! no shared state and therefore no lock — the single atomic the handler
//! carries is the replay flag, written once around `session/load`.
//!
//! # Mounting
//!
//! MCP servers reach the agent on the session-lifecycle request itself —
//! `session/new`'s `mcpServers` — which is the mount the protocol
//! sanctions. Proving it differs by agent: where [`AcpMethods::mcp_status`]
//! names an introspection method the driver asks and refuses a session
//! without flyco's tools; where it does not, the driver's [`observe`] phase
//! watches the agent's own output for a bounded window and fails only on
//! evidence of failure.

pub mod normalize;

use std::collections::{BTreeMap, VecDeque};
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use aither_acp::{
    AcpClient, ClientCapabilities, ClientError, ClientHandler, ContentBlock, Implementation,
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionParams,
    RequestPermissionResult, SessionNewParams, SessionNotification, SessionResumeParams,
    SessionSetConfigOptionParams, SessionSetModeParams, TextContent,
};
use aither_mcp::protocol::{JsonRpcError, JsonRpcNotification};
use flyco_core::{ApprovalId, ContextUsage, HarnessEvent, ModelChoice, PermissionMode};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use self::normalize::Normalizer;
use super::{Harness, HarnessSession, SessionOutput, StartRequest, Started, ToolApproval};
use crate::config::{AcpConfig, AcpMethodCall};
use crate::mount::{Mount, MountState, MountedServer};

/// Capacity of the driver's command channel.
const CHANNEL_DEPTH: usize = 256;

/// How long a clean shutdown may take before the driver gives up on it.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// How long the driver waits for an agent's MCP servers to stop dialling.
///
/// Same rule the Codex driver held: a server that is still starting is not
/// a failure yet, and a deadline is what stops one that never will from
/// holding the session open.
const MOUNT_SETTLE: Duration = Duration::from_secs(10);

/// How often a still-dialling mount is re-asked.
const MOUNT_POLL: Duration = Duration::from_millis(250);

/// How long [`observe`] watches a mount-reportless agent for evidence.
///
/// Only negative evidence is waited on — an agent that never mentions its
/// MCP servers is one whose mount is provisioned but unprovable, which is
/// logged rather than made fatal.
const MOUNT_OBSERVE: Duration = Duration::from_secs(15);

/// The generic ACP driver failed.
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    /// An `[acp.files]` entry could not be materialized.
    #[error("could not write the agent's file at {path}")]
    File {
        /// Where the write was aimed.
        path: std::path::PathBuf,
        /// The underlying cause.
        #[source]
        source: std::io::Error,
    },
    /// The session working directory could not be created.
    #[error("could not create the session workdir at {path}")]
    Workdir {
        /// The directory that could not be created.
        path: std::path::PathBuf,
        /// The underlying cause.
        #[source]
        source: std::io::Error,
    },
    /// The agent process could not be spawned.
    #[error("could not run the ACP agent `{program}`")]
    Spawn {
        /// The configured program.
        program: String,
        /// Why the spawn failed.
        detail: String,
    },
    /// A request to the agent failed.
    #[error("the ACP agent {context}")]
    Agent {
        /// What was being asked.
        context: String,
        /// The failure.
        #[source]
        source: ClientError,
    },
    /// The agent closed its connection or exited.
    #[error("the ACP agent's connection closed")]
    Closed,
    /// A registered MCP server needs a transport the agent does not speak.
    #[error(
        "the MCP server `{server}` is reached over HTTP, which the configured agent's \
         capabilities say it cannot speak"
    )]
    UnsupportedTransport {
        /// The registered server.
        server: String,
    },
    /// A permission mode was asked for that the `[acp.modes]` table does
    /// not express.
    #[error(
        "permission mode `{mode:?}` has no `[acp.modes]` entry for this agent — it is refused \
         rather than approximated"
    )]
    ModeUnsupported {
        /// The mode that cannot be expressed.
        mode: PermissionMode,
    },
    /// An effort was configured on an agent with no effort option.
    #[error(
        "an effort was configured but this agent's `[acp]` names no `effort_option` to carry it"
    )]
    EffortUnsupported,
    /// A feature was asked for that the `[acp.methods]` table has no
    /// method for.
    #[error("this agent's `[acp.methods]` names no method for {feature}")]
    NoMethod {
        /// What was asked for.
        feature: &'static str,
    },
    /// The session's task is gone, so no command can be delivered.
    #[error("the ACP session has stopped")]
    Stopped,
    /// A second prompt was sent while a turn was still running.
    ///
    /// Flyco's transcript is serial — one turn, then the next — so a second
    /// message mid-turn is a control-plane bug, not something to queue.
    #[error("a turn is already in flight")]
    TurnInFlight,
    /// The agent's mount report could not be read as a server list.
    #[error("the agent's mount report was not a readable server list: {detail}")]
    MountReport {
        /// What the answer looked like.
        detail: String,
    },
    /// The mount check refused the session.
    #[error(transparent)]
    Mount(#[from] crate::mount::NotMounted),
    /// An approval decision arrived for a request nobody is waiting on.
    #[error("no agent permission request is waiting on {0}")]
    UnknownApproval(ApprovalId),
}

/// A configured, not-yet-started ACP harness.
#[derive(Debug)]
pub struct AcpHarness {
    config: AcpConfig,
    mount: Mount,
}

impl AcpHarness {
    /// Builds a harness from its configuration and the MCP servers the
    /// session may reach.
    #[must_use]
    pub const fn new(config: AcpConfig, mount: Mount) -> Self {
        Self { config, mount }
    }
}

impl Harness for AcpHarness {
    type Session = AcpSession;
    type Error = AcpError;

    async fn start(self, request: StartRequest) -> Result<Started<Self::Session>, AcpError> {
        let config = self.config;
        materialize_files(&config).await?;
        tokio::fs::create_dir_all(&request.workdir)
            .await
            .map_err(|source| AcpError::Workdir {
                path: request.workdir.clone(),
                source,
            })?;

        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let connected = connect(&config, &request, events_tx.clone(), &self.mount).await?;

        let replaying = connected.client.handler().replaying.clone();
        let opened = match open_session(
            &connected.client,
            &config,
            &self.mount,
            &request,
            &replaying,
            &connected.initialized,
        )
        .await
        {
            Ok(opened) => opened,
            Err(error) => {
                connected.connection_task.abort();
                return Err(error);
            }
        };
        let session_id = opened.session_id.clone();

        if let Err(error) = apply_permission_mode(
            &connected.client,
            &session_id,
            config.permission_mode,
            &config,
        )
        .await
        {
            connected.connection_task.abort();
            return Err(error);
        }
        if let Err(error) = apply_model(&connected.client, &session_id, &config).await {
            connected.connection_task.abort();
            return Err(error);
        }

        // Prove or observe the mount, exactly once, before the session can
        // produce anything. The prelude keeps whatever arrived meanwhile —
        // a replayed capability report, an early command palette — in order
        // for the driver to process first.
        let mut prelude = Vec::new();
        let mount_outcome = match &config.methods.mcp_status {
            Some(method) => verify_mount(&connected.client, &session_id, method, &self.mount).await,
            None => observe_mount(&mut events_rx, &mut prelude).await,
        };
        if let Err(error) = mount_outcome {
            connected.connection_task.abort();
            return Err(error);
        }

        let (commands, inbox) = mpsc::channel(CHANNEL_DEPTH);
        let (outputs, output_rx) = mpsc::channel(CHANNEL_DEPTH);
        tokio::spawn(
            Driver {
                client: connected.client,
                connection_task: connected.connection_task,
                session_id,
                config,
                normalizer: Normalizer::default(),
                outputs,
                events: events_tx,
                turn_seq: 0,
                active_turn: None,
                queued_messages: VecDeque::new(),
                pending_permissions: BTreeMap::new(),
                pending_calls: BTreeMap::new(),
                call_seq: 0,
                close_supported: opened.close_supported,
                stopped: false,
                mode: opened.mode.clone(),
                model: opened.model.clone(),
            }
            .run(inbox, events_rx, prelude, opened),
        );

        Ok(Started {
            session: AcpSession { commands },
            outputs: output_rx,
        })
    }
}

/// A spawned, handshaken agent — the connect phase's yield.
struct Connected {
    /// The JSON-RPC client handle.
    client: AcpClient<AgentHandler>,
    /// The transport task; its end is the agent's end.
    connection_task: tokio::task::JoinHandle<()>,
    /// The `initialize` answer.
    initialized: aither_acp::InitializeResult,
}

/// Spawns the agent, runs the handshake, and refuses mounts the agent
/// cannot speak.
///
/// The connection task owns the transport and dispatches to the handler.
/// Its end is the agent's end: when it resolves, the driver is told rather
/// than left to discover a dead channel per request.
async fn connect(
    config: &AcpConfig,
    request: &StartRequest,
    events: mpsc::UnboundedSender<AgentEvent>,
    mount: &Mount,
) -> Result<Connected, AcpError> {
    let handler = AgentHandler {
        events,
        replaying: Arc::new(AtomicBool::new(false)),
    };
    let program = config.program.to_string_lossy().into_owned();
    let args: Vec<&str> = config.args.iter().map(String::as_str).collect();
    let (client, connection) = AcpClient::spawn(
        &program,
        &args,
        config.env.clone(),
        request.workdir.clone(),
        handler.clone(),
    )
    .map_err(|error| AcpError::Spawn {
        program: program.clone(),
        detail: error.to_string(),
    })?;
    let client = client.with_client_info(Implementation {
        name: "flycod".to_owned(),
        title: None,
        version: env!("CARGO_PKG_VERSION").to_owned(),
    });
    let closed = handler.events.clone();
    let connection_task = tokio::spawn(async move {
        connection.await;
        let _ = closed.send(AgentEvent::Closed);
    });

    let initialized = match client.initialize().await {
        Ok(result) => result,
        Err(error) => {
            connection_task.abort();
            return Err(AcpError::Agent {
                context: "rejected `initialize`".to_owned(),
                source: error,
            });
        }
    };
    tracing::info!(
        agent = %config.agent,
        version = ?initialized.agent_info.as_ref().map(|info| &info.version),
        "the ACP agent completed its handshake"
    );

    // A mounted server the agent cannot even speak to fails here rather
    // than surfacing as a mysteriously absent tool mid-session.
    for server in mount.acp_servers() {
        if let aither_acp::McpServer::Http(http) = &server
            && !initialized.agent_capabilities.mcp_capabilities.http
        {
            connection_task.abort();
            return Err(AcpError::UnsupportedTransport {
                server: http.name.clone(),
            });
        }
    }
    Ok(Connected {
        client,
        connection_task,
        initialized,
    })
}

/// What a session-open call settled, before the driver task exists.
struct Opened {
    /// The ACP session id — what a later machine resumes by.
    session_id: String,
    /// Whether the agent advertised `session/close`.
    close_supported: bool,
    /// The mode the agent reported itself in, when it said.
    mode: Option<String>,
    /// The model the agent reported itself on, when it said.
    model: Option<String>,
    /// The negotiated capability tokens, reported as `Capabilities`.
    capabilities: Vec<String>,
    /// The models the agent's config options listed, if any.
    models: Vec<flyco_core::ModelOption>,
    /// The commands the agent's session-open answer listed, if any.
    commands: Vec<flyco_core::HarnessCommand>,
    /// Why a fresh session stands where a continued one was asked for —
    /// said in the room, where the gap is visible.
    notice: Option<String>,
}

/// Opens the ACP session — new, resumed, or loaded.
///
/// Resume and load differ in what the agent replays: `session/resume`
/// restores the context without re-sending history, while `session/load`
/// streams the whole transcript back as updates. Replay is suppressed at
/// the handler — a loaded history re-emitted as deltas would duplicate the
/// conversation in flyco's transcript.
///
/// A continuation that cannot happen — the agent supports neither method,
/// or rejects the one it advertised — is not a session failure. The
/// workspace was already restored from the stored patch before the
/// harness started, so the session opens fresh over it and says so in the
/// room; the alternative is a dead session standing on live work.
async fn open_session(
    client: &AcpClient<AgentHandler>,
    config: &AcpConfig,
    mount: &Mount,
    request: &StartRequest,
    replaying: &AtomicBool,
    initialized: &aither_acp::InitializeResult,
) -> Result<Opened, AcpError> {
    let capabilities = &initialized.agent_capabilities;

    let (session_id, modes, config_options, extra, notice) = match &request.resume_session_id {
        Some(id) if capabilities.session_capabilities.resume.is_some() => {
            match client
                .resume_session(
                    SessionResumeParams::new(id.clone(), request.workdir.clone())
                        .mcp_servers(mount.acp_servers()),
                )
                .await
            {
                Ok(result) => (
                    id.clone(),
                    result.modes,
                    result.config_options,
                    result.extra,
                    None,
                ),
                Err(source) => {
                    let fresh = new_conversation(client, mount, request).await?;
                    (
                        fresh.session_id,
                        fresh.modes,
                        fresh.config_options,
                        fresh.extra,
                        Some(restarted_fresh(config, "session/resume", &source)),
                    )
                }
            }
        }
        Some(id) if capabilities.load_session => {
            replaying.store(true, Ordering::SeqCst);
            let loaded = client
                .load_session(
                    aither_acp::SessionLoadParams::new(id.clone(), request.workdir.clone())
                        .mcp_servers(mount.acp_servers()),
                )
                .await;
            replaying.store(false, Ordering::SeqCst);
            match loaded {
                Ok(result) => (
                    id.clone(),
                    result.modes,
                    result.config_options,
                    result.extra,
                    None,
                ),
                Err(source) => {
                    let fresh = new_conversation(client, mount, request).await?;
                    (
                        fresh.session_id,
                        fresh.modes,
                        fresh.config_options,
                        fresh.extra,
                        Some(restarted_fresh(config, "session/load", &source)),
                    )
                }
            }
        }
        Some(_) => {
            let fresh = new_conversation(client, mount, request).await?;
            (
                fresh.session_id,
                fresh.modes,
                fresh.config_options,
                fresh.extra,
                Some(uncontinuable(config)),
            )
        }
        None => {
            let fresh = new_conversation(client, mount, request).await?;
            (
                fresh.session_id,
                fresh.modes,
                fresh.config_options,
                fresh.extra,
                None,
            )
        }
    };
    Ok(describe_opened(
        session_id,
        capabilities,
        modes,
        config_options.as_deref(),
        &extra,
        notice,
    ))
}

/// `session/new` — where a session whose conversation cannot be continued
/// lands, and where one with nothing to continue starts.
async fn new_conversation(
    client: &AcpClient<AgentHandler>,
    mount: &Mount,
    request: &StartRequest,
) -> Result<aither_acp::SessionNewResult, AcpError> {
    client
        .new_session(
            SessionNewParams::new(request.workdir.clone()).mcp_servers(mount.acp_servers()),
        )
        .await
        .map_err(|source| AcpError::Agent {
            context: "rejected `session/new`".to_owned(),
            source,
        })
}

/// The room line a fresh session carries when it stands where a continued
/// conversation was asked for: the continuation was offered and refused.
fn restarted_fresh(config: &AcpConfig, method: &str, source: &ClientError) -> String {
    tracing::warn!(agent = %config.agent, %source, "{method} was rejected; the session opens fresh");
    format!(
        "The previous conversation could not be continued — {} rejected `{method}` — so this \
         session started fresh. The workspace still holds all of its work.",
        config.agent
    )
}

/// The room line a fresh session carries on an agent that speaks no
/// continuation method at all.
fn uncontinuable(config: &AcpConfig) -> String {
    tracing::warn!(
        agent = %config.agent,
        "the agent supports neither session/resume nor session/load; the session opens fresh"
    );
    format!(
        "{} can neither resume nor load a stored conversation, so this session started fresh. \
         The workspace still holds all of the previous work.",
        config.agent
    )
}

/// What the session-open answer settles, as an [`Opened`].
fn describe_opened(
    session_id: String,
    capabilities: &aither_acp::AgentCapabilities,
    modes: Option<aither_acp::SessionModeState>,
    config_options: Option<&[aither_acp::ConfigOption]>,
    extra: &BTreeMap<String, Value>,
    notice: Option<String>,
) -> Opened {
    let mut tokens = vec!["acp".to_owned()];
    if capabilities.load_session {
        tokens.push("acp:load_session".to_owned());
    }
    for (supported, token) in [
        (
            capabilities.session_capabilities.resume.is_some(),
            "acp:resume",
        ),
        (
            capabilities.session_capabilities.close.is_some(),
            "acp:close",
        ),
        (capabilities.session_capabilities.list.is_some(), "acp:list"),
        (modes.is_some(), "acp:modes"),
        (
            config_options.is_some_and(|list| !list.is_empty()),
            "acp:config_options",
        ),
        (capabilities.mcp_capabilities.http, "acp:mcp_http"),
        (capabilities.mcp_capabilities.sse, "acp:mcp_sse"),
    ] {
        if supported {
            tokens.push(token.to_owned());
        }
    }

    let (model_list, current_model) =
        normalize::read_model_option(config_options.unwrap_or_default());
    Opened {
        session_id,
        close_supported: capabilities.session_capabilities.close.is_some(),
        mode: modes.map(|state| state.current_mode_id),
        model: current_model,
        capabilities: tokens,
        models: model_list.unwrap_or_default(),
        commands: extra
            .get("availableCommands")
            .and_then(|value| {
                serde_json::from_value::<Vec<aither_acp::AvailableCommand>>(value.clone()).ok()
            })
            .map(|commands| {
                commands
                    .into_iter()
                    .map(|command| flyco_core::HarnessCommand {
                        name: command.name,
                        description: command.description,
                        argument_hint: command.input.map(|input| input.hint),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        notice,
    }
}

/// Puts the session under the configured permission mode.
///
/// The `[acp.modes]` entry is applied as written: `set_mode` first, then
/// the option writes in order. A mode with no entry fails rather than
/// running under a neighbouring one, because "close enough" is not a
/// permission mode.
async fn apply_permission_mode(
    client: &AcpClient<AgentHandler>,
    session_id: &str,
    mode: PermissionMode,
    config: &AcpConfig,
) -> Result<(), AcpError> {
    let Some(mapping) = config.modes.get(&mode) else {
        return Err(AcpError::ModeUnsupported { mode });
    };
    if let Some(mode_id) = &mapping.set_mode {
        client
            .set_mode(SessionSetModeParams::new(session_id, mode_id.clone()))
            .await
            .map_err(|source| AcpError::Agent {
                context: format!("rejected `session/set_mode {mode_id}`"),
                source,
            })?;
    }
    for write in &mapping.options {
        client
            .set_config_option(SessionSetConfigOptionParams::new(
                session_id,
                &write.option,
                write.value.as_str(),
            ))
            .await
            .map_err(|source| AcpError::Agent {
                context: format!("rejected `session/set_config_option {}`", write.option),
                source,
            })?;
    }
    Ok(())
}

/// The model id `set_config_option` is sent.
///
/// On an agent whose ids carry the effort — `fused_effort_tails` names
/// the suffixes it hangs after the level — the chosen effort is folded
/// back into the id: `gpt-5-6-sol-priority` at `high` sends
/// `gpt-5-6-sol-high-priority`. A fused agent asked for no effort gets
/// the id as written — bare ids like `adaptive` are valid on their own.
fn model_id_to_send(config: &AcpConfig, model: &str, effort: Option<&str>) -> String {
    let Some(effort) = effort else {
        return model.to_owned();
    };
    for tail in &config.fused_effort_tails {
        if let Some(stem) = model.strip_suffix(tail.as_str()) {
            return format!("{stem}-{effort}{tail}");
        }
    }
    if config.fused_effort_tails.is_empty() {
        return model.to_owned();
    }
    format!("{model}-{effort}")
}

/// Selects the configured model and effort through the session's config
/// options.
async fn apply_model(
    client: &AcpClient<AgentHandler>,
    session_id: &str,
    config: &AcpConfig,
) -> Result<(), AcpError> {
    if let Some(model) = &config.model {
        let id = model_id_to_send(config, model, config.effort.as_deref());
        client
            .set_config_option(SessionSetConfigOptionParams::new(
                session_id,
                &config.model_option,
                id.as_str(),
            ))
            .await
            .map_err(|source| AcpError::Agent {
                context: format!(
                    "rejected `session/set_config_option {}`",
                    config.model_option
                ),
                source,
            })?;
    }
    if !config.fused_effort_tails.is_empty() {
        // The effort already rode inside the model id.
        return Ok(());
    }
    if let Some(effort) = &config.effort {
        let Some(option) = &config.effort_option else {
            return Err(AcpError::EffortUnsupported);
        };
        client
            .set_config_option(SessionSetConfigOptionParams::new(
                session_id,
                option,
                effort.as_str(),
            ))
            .await
            .map_err(|source| AcpError::Agent {
                context: format!("rejected `session/set_config_option {option}`"),
                source,
            })?;
    }
    Ok(())
}

/// Writes every `[acp.files]` entry, mode `0600`.
///
/// These files exist to carry credentials — Codex's `auth.json`, Devin's
/// `credentials.toml`, a managed MCP registry — so the mode is not a
/// choice, and the parent directories are created rather than required.
async fn materialize_files(config: &AcpConfig) -> Result<(), AcpError> {
    for file in &config.files {
        if let Some(parent) = file.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|source| AcpError::File {
                    path: parent.to_owned(),
                    source,
                })?;
        }
        tokio::fs::write(&file.path, &file.contents)
            .await
            .map_err(|source| AcpError::File {
                path: file.path.clone(),
                source,
            })?;
        tokio::fs::set_permissions(&file.path, std::fs::Permissions::from_mode(0o600))
            .await
            .map_err(|source| AcpError::File {
                path: file.path.clone(),
                source,
            })?;
    }
    Ok(())
}

/// A params template with its `<session>` placeholders filled.
///
/// Every extension method that takes the session id spells it differently —
/// `sessionId`, `threadId` — so the config names the field and the marker
/// is substituted wherever it appears as a whole string value.
fn render_params(call: &AcpMethodCall, session_id: &str) -> Value {
    fn fill(value: &Value, session_id: &str) -> Value {
        match value {
            Value::String(text) if text == "<session>" => Value::String(session_id.to_owned()),
            Value::Array(items) => {
                Value::Array(items.iter().map(|i| fill(i, session_id)).collect())
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, value)| (key.clone(), fill(value, session_id)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    fill(&call.params, session_id)
}

/// Asks the agent what it mounted, until nothing is still dialling.
async fn verify_mount(
    client: &AcpClient<AgentHandler>,
    session_id: &str,
    method: &AcpMethodCall,
    mount: &Mount,
) -> Result<(), AcpError> {
    let deadline = tokio::time::Instant::now() + MOUNT_SETTLE;
    loop {
        let servers = mounted_servers(client, session_id, method).await?;
        if crate::mount::settled(&servers) || tokio::time::Instant::now() >= deadline {
            return crate::mount::verify(&servers, &mount.required_tools())
                .map_err(AcpError::Mount);
        }
        tokio::time::sleep(MOUNT_POLL).await;
    }
}

/// One complete answer to the configured mount-introspection method.
///
/// Parsed tolerantly rather than against one vendor's schema: the answer's
/// server list is wherever a `data` array or a top-level array lives, a
/// server is an object with a `name`, its state is whichever of
/// `runtimeStatus`/`status`/`state` it carries, and its tools are the keys
/// of a `tools` object or the names/strings of a `tools` array. Codex's
/// `mcpServerStatus/list` answers exactly this; another agent's answer in
/// the same shape works unchanged.
async fn mounted_servers(
    client: &AcpClient<AgentHandler>,
    session_id: &str,
    method: &AcpMethodCall,
) -> Result<Vec<MountedServer>, AcpError> {
    let mut mounted = Vec::new();
    let mut params = render_params(method, session_id);
    loop {
        let result: Value = client
            .request(&method.call, &params)
            .await
            .map_err(|source| AcpError::Agent {
                context: format!("rejected `{}`", method.call),
                source,
            })?;
        let list = result
            .get("data")
            .or_else(|| result.get("servers"))
            .and_then(Value::as_array)
            .or_else(|| result.as_array())
            .ok_or_else(|| AcpError::MountReport {
                detail: format!("no server list in {}", truncate(&result)),
            })?;
        mounted.extend(list.iter().filter_map(mounted_server));
        let Some(cursor) = result
            .get("nextCursor")
            .or_else(|| result.get("next_cursor"))
            .and_then(Value::as_str)
        else {
            return Ok(mounted);
        };
        if let Value::Object(map) = &mut params {
            map.insert("cursor".to_owned(), Value::String(cursor.to_owned()));
        } else {
            // A configured params shape that cannot carry a cursor ends
            // the walk here — looping on the first page forever would
            // be worse than reporting the page seen.
            tracing::warn!("the mount report paged but its params take no cursor");
            return Ok(mounted);
        }
    }
}

/// One entry of a mount report, read tolerantly.
fn mounted_server(entry: &Value) -> Option<MountedServer> {
    let name = entry.get("name")?.as_str()?.to_owned();
    let status = entry
        .get("runtimeStatus")
        .or_else(|| entry.get("status"))
        .or_else(|| entry.get("state"))
        .and_then(Value::as_str)
        .unwrap_or("notStarted")
        .to_owned();
    let tools = entry
        .get("tools")
        .map(|tools| match tools {
            Value::Object(map) => map.keys().cloned().collect(),
            Value::Array(list) => list
                .iter()
                .filter_map(|tool| {
                    tool.as_str()
                        .map(str::to_owned)
                        .or_else(|| tool.get("name").and_then(Value::as_str).map(str::to_owned))
                })
                .collect(),
            _ => Vec::new(),
        })
        .unwrap_or_default();
    Some(MountedServer {
        name,
        state: match status.as_str() {
            "connected" | "running" | "ready" => MountState::Connected,
            "notStarted" | "starting" | "pending" | "connecting" => MountState::Pending,
            _ => MountState::Failed,
        },
        status,
        tools,
    })
}

/// The no-introspection mount check: watch what the agent itself reports.
///
/// An agent without a `mcp_status` method still tends to say when a server
/// fails — Devin writes it to its `_cognition.ai/output` channel under
/// `MCP: <name>` — so for a bounded window the session's early
/// notifications are scanned for a failure naming flyco's server, which is
/// fatal, or an explicit connect, which ends the watch early. Silence is
/// not evidence either way and is logged as such.
///
/// Notifications that are not about MCP — the replayed capability report,
/// an early palette — are not consumed but pushed onto `prelude`, which the
/// driver processes before anything newer, so nothing is lost to the watch.
async fn observe_mount(
    events: &mut mpsc::UnboundedReceiver<AgentEvent>,
    prelude: &mut Vec<AgentEvent>,
) -> Result<(), AcpError> {
    let deadline = tokio::time::sleep(MOUNT_OBSERVE);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => {
                tracing::warn!(
                    "this agent reports no MCP status method and said nothing about the mount; \
                     flyco's server is provisioned but unproven"
                );
                return Ok(());
            }
            event = events.recv() => {
                let Some(event) = event else { return Ok(()) };
                match event {
                    // The agent died mid-watch: not a mount answer, a dead
                    // session, and starting one is the wrong report.
                    AgentEvent::Closed => return Err(AcpError::Closed),
                    AgentEvent::Vendor(notification) => {
                        match mount_evidence(&notification) {
                            Some(Evidence::Failed(detail)) => {
                                return Err(AcpError::Mount(crate::mount::NotMounted::NotConnected {
                                    status: detail,
                                }));
                            }
                            Some(Evidence::Connected) => {
                                tracing::info!("the agent confirmed flyco's MCP server connected");
                                return Ok(());
                            }
                            None => {}
                        }
                    }
                    other => prelude.push(other),
                }
            }
        }
    }
}

/// What a notification says about flyco's MCP server, when it says
/// anything.
enum Evidence {
    /// The server connected.
    Connected,
    /// The server failed; the detail is the agent's own words.
    Failed(String),
}

/// Reads an unmodeled agent notification for flyco-server news.
///
/// Two generic shapes are recognized rather than one vendor's: a servers-
/// changed notification whose payload names the server at all (it announces
/// the live set), and a log-channel line of the `MCP: <name>` form, where
/// the words after the name decide. Anything else is not evidence.
fn mount_evidence(notification: &JsonRpcNotification) -> Option<Evidence> {
    let method = notification.method.as_str();
    let params = notification.params.as_ref()?;
    let text = params.to_string();
    if method.contains("serversChanged") || method.ends_with("/mcp") {
        return text
            .contains(&format!("\"{}\"", crate::mount::FLYCO))
            .then_some(Evidence::Connected);
    }
    if let Some(message) = params.get("message").and_then(Value::as_str)
        && params
            .get("channel")
            .and_then(Value::as_str)
            .is_some_and(|channel| channel.ends_with(crate::mount::FLYCO))
    {
        let lower = message.to_lowercase();
        if ["fail", "error", "refus", "denied", "unable"]
            .iter()
            .any(|word| lower.contains(word))
        {
            return Some(Evidence::Failed(message.to_owned()));
        }
        if ["connect", "ready", "start", "listen"]
            .iter()
            .any(|word| lower.contains(word))
        {
            return Some(Evidence::Connected);
        }
    }
    None
}

/// The sentence a [`Value`] begins with, for error messages.
fn truncate(value: &Value) -> String {
    let text = value.to_string();
    if text.len() > 200 {
        format!("{}…", &text[..200])
    } else {
        text
    }
}

/// The control handle of a running ACP session.
#[derive(Debug, Clone)]
pub struct AcpSession {
    commands: mpsc::Sender<DriverCommand>,
}

impl AcpSession {
    async fn ask(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<(), AcpError>>) -> DriverCommand,
    ) -> Result<(), AcpError> {
        let (ack, answer) = oneshot::channel();
        self.commands
            .send(make(ack))
            .await
            .map_err(|_| AcpError::Stopped)?;
        answer.await.map_err(|_| AcpError::Stopped)?
    }
}

impl HarnessSession for AcpSession {
    type Error = AcpError;

    async fn send_user_message(&self, text: String) -> Result<(), AcpError> {
        self.ask(|ack| DriverCommand::UserMessage { text, ack })
            .await
    }

    async fn interrupt(&self) -> Result<(), AcpError> {
        self.ask(|ack| DriverCommand::Interrupt { ack }).await
    }

    async fn flush(&self) -> Result<(), AcpError> {
        self.ask(|ack| DriverCommand::Flush { ack }).await
    }

    async fn compact(&self) -> Result<(), AcpError> {
        self.ask(|ack| DriverCommand::Compact { ack }).await
    }

    async fn context_usage(&self) -> Result<(), AcpError> {
        self.ask(|ack| DriverCommand::ContextUsage { ack }).await
    }

    async fn set_model(&self, model: ModelChoice) -> Result<(), AcpError> {
        self.ask(|ack| DriverCommand::SetModel { model, ack }).await
    }

    async fn set_permission_mode(&self, mode: PermissionMode) -> Result<(), AcpError> {
        self.ask(|ack| DriverCommand::SetPermissionMode { mode, ack })
            .await
    }

    async fn decide_approval(&self, approval: ToolApproval) -> Result<(), AcpError> {
        self.ask(|ack| DriverCommand::Approval { approval, ack })
            .await
    }

    async fn shutdown(self) -> Result<(), AcpError> {
        self.ask(|ack| DriverCommand::Shutdown { ack }).await
    }
}

/// Everything the driver task can be asked to do.
#[derive(Debug)]
enum DriverCommand {
    /// From the handle: push a user message and open a turn.
    UserMessage {
        /// The message text.
        text: String,
        /// Answered once the prompt is dispatched.
        ack: oneshot::Sender<Result<(), AcpError>>,
    },
    /// From the handle: end the current turn.
    Interrupt {
        /// Answered once the cancel notification is sent.
        ack: oneshot::Sender<Result<(), AcpError>>,
    },
    /// From the handle: answer once every queued agent event is processed.
    Flush {
        /// The acknowledgement.
        ack: oneshot::Sender<Result<(), AcpError>>,
    },
    /// From the handle: compact the conversation context.
    Compact {
        /// Answered with the method call's own result.
        ack: oneshot::Sender<Result<(), AcpError>>,
    },
    /// From the handle: report what the context window is spent on.
    ContextUsage {
        /// The acknowledgement.
        ack: oneshot::Sender<Result<(), AcpError>>,
    },
    /// From the handle: put the session on another model.
    SetModel {
        /// The choice.
        model: ModelChoice,
        /// Answered with the config-option writes' result.
        ack: oneshot::Sender<Result<(), AcpError>>,
    },
    /// From the handle: put the session under another permission mode.
    SetPermissionMode {
        /// The mode.
        mode: PermissionMode,
        /// Answered with the writes' result.
        ack: oneshot::Sender<Result<(), AcpError>>,
    },
    /// From the handle: answer a pending approval.
    Approval {
        /// The decision.
        approval: ToolApproval,
        /// The acknowledgement.
        ack: oneshot::Sender<Result<(), AcpError>>,
    },
    /// From the handle: stop the session.
    Shutdown {
        /// The acknowledgement.
        ack: oneshot::Sender<Result<(), AcpError>>,
    },
}

/// Everything the agent reports, routed through the handler.
#[derive(Debug)]
enum AgentEvent {
    /// A `session/update` notification.
    Update(SessionNotification),
    /// A `session/request_permission` request; the agent is blocked until
    /// `respond` is answered.
    Permission {
        /// What is being asked.
        params: RequestPermissionParams,
        /// Carries the answer back to the connection task.
        respond: oneshot::Sender<RequestPermissionResult>,
    },
    /// An agent-to-client notification ACP does not model — vendor
    /// extensions and anything newer than this build.
    Vendor(JsonRpcNotification),
    /// A spawned `session/prompt` request resolved.
    PromptFinished {
        /// Which minted turn it was.
        turn_id: String,
        /// The prompt's own result.
        result: Result<aither_acp::PromptResult, ClientError>,
    },
    /// A spawned extension-method call resolved.
    CallFinished {
        /// Which spawned call it was.
        id: u64,
        /// The call's raw result.
        result: Result<Value, ClientError>,
    },
    /// The connection task ended — the agent exited or closed the pipe.
    Closed,
}

/// A permission request waiting on the user.
struct PendingPermission {
    /// Carries the outcome back to the blocked agent.
    respond: oneshot::Sender<RequestPermissionResult>,
    /// The options the agent offered, so an Allow can pick the right id.
    options: Vec<PermissionOption>,
}

/// What a spawned extension call was for.
enum PendingCall {
    /// A compaction; its caller waits on the ack.
    Compact(oneshot::Sender<Result<(), AcpError>>),
    /// A plan-usage read; nobody waits on it, and what it answers is only
    /// read for the limit it may announce — the rings are the control
    /// plane's, read from the vendor while a page is built.
    Usage,
}

/// The task that owns the client handle and everything mutable.
struct Driver {
    client: AcpClient<AgentHandler>,
    /// The connection task, awaited on shutdown.
    connection_task: tokio::task::JoinHandle<()>,
    /// The ACP session every call is keyed by.
    session_id: String,
    config: AcpConfig,
    normalizer: Normalizer,
    outputs: mpsc::Sender<SessionOutput>,
    /// Cloned into every spawned request, so its answer lands back here.
    events: mpsc::UnboundedSender<AgentEvent>,
    /// Minted turn counter — ACP carries no turn ids, so they are invented.
    turn_seq: u64,
    /// The turn in flight, if any.
    active_turn: Option<String>,
    /// User messages accepted while a turn runs.
    ///
    /// ACP refuses a second `session/prompt` mid-turn, so what arrives
    /// during one — the user's own follow-up, an injected notice — waits
    /// here rather than erroring. The next `PromptFinished` drains one,
    /// and each message's ack answers at enqueue: "accepted" is what the
    /// wire layer needs to know, and the room log still holds the text if
    /// the daemon dies before it is sent.
    queued_messages: VecDeque<String>,
    pending_permissions: BTreeMap<ApprovalId, PendingPermission>,
    pending_calls: BTreeMap<u64, PendingCall>,
    /// Spawned-call counter.
    call_seq: u64,
    /// Whether the agent advertised `session/close`.
    close_supported: bool,
    /// Whether shutdown has already run.
    stopped: bool,
    /// The agent's current mode id, as last reported.
    mode: Option<String>,
    /// The model the session reports running on, as last reported.
    model: Option<String>,
}

impl Driver {
    async fn run(
        mut self,
        mut inbox: mpsc::Receiver<DriverCommand>,
        mut events: mpsc::UnboundedReceiver<AgentEvent>,
        prelude: Vec<AgentEvent>,
        opened: Opened,
    ) {
        if !emit(
            &self.outputs,
            SessionOutput::Started {
                session_id: self.session_id.clone(),
            },
        )
        .await
        {
            return;
        }
        if !emit(
            &self.outputs,
            SessionOutput::Capabilities {
                capabilities: opened.capabilities,
            },
        )
        .await
        {
            return;
        }
        if !opened.models.is_empty()
            && !emit(
                &self.outputs,
                SessionOutput::Models {
                    models: opened.models,
                },
            )
            .await
        {
            return;
        }
        if !opened.commands.is_empty()
            && !emit(
                &self.outputs,
                SessionOutput::Commands {
                    commands: opened.commands,
                },
            )
            .await
        {
            return;
        }
        if let Some(notice) = opened.notice
            && !emit(
                &self.outputs,
                SessionOutput::Event {
                    event: HarnessEvent::LocalCommandOutput { content: notice },
                },
            )
            .await
        {
            return;
        }
        // Asked once at open, then again after every turn — a turn is the
        // only thing that moves the number.
        self.request_usage();

        // Drained before either channel: events buffered by the mount watch
        // predate everything the channels hold.
        for event in prelude {
            if !self.on_agent(event).await {
                let _ = self.stop().await;
                return;
            }
        }
        loop {
            if inbox.is_closed() {
                break;
            }
            let input = tokio::select! {
                command = inbox.recv() => command.map(AgentOrCommand::Command),
                agent = events.recv() => agent.map(|event| AgentOrCommand::Agent(Box::new(event))),
            };
            match input {
                // `events` never ends: the driver holds a sender of its own
                // for spawned calls, so the agent's end is the `Closed`
                // event rather than the channel.
                Some(AgentOrCommand::Command(command)) => {
                    if !self.handle(command).await {
                        break;
                    }
                }
                Some(AgentOrCommand::Agent(event)) => {
                    if !self.on_agent(*event).await {
                        break;
                    }
                }
                None => break,
            }
        }
        let _ = self.stop().await;
        tracing::debug!(agent = %self.config.agent, "the ACP driver task finished");
    }

    /// One handle command.
    ///
    /// Returns `false` when the loop should end.
    async fn handle(&mut self, command: DriverCommand) -> bool {
        match command {
            DriverCommand::UserMessage { text, ack } => {
                if self.active_turn.is_some() {
                    let _ = ack.send(Ok(()));
                    self.queued_messages.push_back(text);
                    return true;
                }
                let result = self.start_turn(text).await;
                let ok = result.is_ok();
                let _ = ack.send(result);
                ok
            }
            DriverCommand::Interrupt { ack } => {
                let result = self
                    .client
                    .cancel(self.session_id.clone())
                    .await
                    .map_err(|source| AcpError::Agent {
                        context: "rejected `session/cancel`".to_owned(),
                        source,
                    });
                let _ = ack.send(result);
                true
            }
            DriverCommand::Flush { ack } => {
                let _ = ack.send(Ok(()));
                true
            }
            DriverCommand::Compact { ack } => {
                let Some(call) = self.config.methods.compact.clone() else {
                    let _ = ack.send(Err(AcpError::NoMethod {
                        feature: "context compaction",
                    }));
                    return true;
                };
                // The spawned call's `CallFinished` answers the ack — the
                // loop must keep reading the agent while it runs.
                self.spawn_call(call, PendingCall::Compact(ack));
                true
            }
            DriverCommand::ContextUsage { ack } => {
                let _ = ack.send(Ok(()));
                emit(
                    &self.outputs,
                    SessionOutput::Event {
                        event: HarnessEvent::ContextUsage {
                            usage: ContextUsage {
                                model: self.model.clone(),
                                window: self.normalizer.window(),
                                auto_compact: None,
                                categories: Vec::new(),
                                mcp_tools: Vec::new(),
                                memory_files: Vec::new(),
                                agents: Vec::new(),
                                skills: Vec::new(),
                            },
                        },
                    },
                )
                .await
            }
            DriverCommand::SetModel { model, ack } => {
                let result = self.apply_model_choice(model).await;
                let _ = ack.send(result);
                true
            }
            DriverCommand::SetPermissionMode { mode, ack } => {
                let result =
                    apply_permission_mode(&self.client, &self.session_id, mode, &self.config).await;
                let _ = ack.send(result);
                true
            }
            DriverCommand::Approval { approval, ack } => {
                let result = self.decide(&approval);
                let _ = ack.send(result);
                true
            }
            DriverCommand::Shutdown { ack } => {
                let stopped = self.stop().await;
                let _ = ack.send(stopped);
                false
            }
        }
    }

    /// One agent event.
    async fn on_agent(&mut self, event: AgentEvent) -> bool {
        match event {
            AgentEvent::Update(notification) => self.on_update(notification).await,
            AgentEvent::Permission { params, respond } => self.on_permission(params, respond).await,
            AgentEvent::Vendor(notification) => {
                tracing::debug!(
                    method = %notification.method,
                    "an unmodeled agent notification arrived"
                );
                true
            }
            AgentEvent::PromptFinished { turn_id, result } => {
                if self.active_turn.as_deref() == Some(turn_id.as_str()) {
                    self.active_turn = None;
                }
                let event = match result {
                    Ok(prompt) => self
                        .normalizer
                        .on_prompt_result(prompt.stop_reason, &turn_id),
                    Err(error) => HarnessEvent::TurnFailed {
                        turn_id,
                        error: super::describe(&error),
                    },
                };
                if !emit(&self.outputs, SessionOutput::Event { event }).await {
                    return false;
                }
                // A turn is the only thing that moves the plan's meters.
                self.request_usage();
                // What arrived mid-turn now opens the next one.
                if let Some(text) = self.queued_messages.pop_front()
                    && let Err(error) = self.start_turn(text).await
                {
                    tracing::warn!(%error, "a queued message could not open its turn");
                }
                true
            }
            AgentEvent::CallFinished { id, result } => self.on_call_finished(id, result).await,
            AgentEvent::Closed => {
                tracing::info!("the ACP agent's connection closed");
                false
            }
        }
    }

    /// A `session/update` notification, normalized and emitted.
    async fn on_update(&mut self, notification: SessionNotification) -> bool {
        let turn_id = self
            .active_turn
            .clone()
            .unwrap_or_else(|| "turn-0".to_owned());
        let normalized = self.normalizer.on_update(&notification.update, &turn_id);
        if let Some(commands) = normalized.commands
            && !emit(&self.outputs, SessionOutput::Commands { commands }).await
        {
            return false;
        }
        if let Some(models) = normalized.models
            && !models.is_empty()
            && !emit(&self.outputs, SessionOutput::Models { models }).await
        {
            return false;
        }
        if let Some(model) = normalized.model {
            self.model = Some(model);
        }
        if let Some(mode) = normalized.mode {
            self.mode = Some(mode);
        }
        for event in normalized.events {
            if !emit(&self.outputs, SessionOutput::Event { event }).await {
                return false;
            }
        }
        true
    }

    /// A `session/request_permission` request, held until the user answers.
    async fn on_permission(
        &mut self,
        params: RequestPermissionParams,
        respond: oneshot::Sender<RequestPermissionResult>,
    ) -> bool {
        let id = ApprovalId::generate();
        let input = params
            .tool_call
            .raw_input
            .clone()
            .unwrap_or_else(|| normalize::tool_call_input(&params.tool_call));
        let tool = normalize::tool_name(&params.tool_call.title, params.tool_call.kind);
        self.pending_permissions.insert(
            id,
            PendingPermission {
                respond,
                options: params.options,
            },
        );
        emit(
            &self.outputs,
            SessionOutput::ApprovalRequest {
                id,
                tool,
                input,
                suggestions: None,
            },
        )
        .await
    }

    /// A spawned extension call's answer.
    async fn on_call_finished(&mut self, id: u64, result: Result<Value, ClientError>) -> bool {
        let Some(call) = self.pending_calls.remove(&id) else {
            return true;
        };
        match call {
            PendingCall::Compact(ack) => {
                let event = match &result {
                    Ok(_) => HarnessEvent::ContextCompacted,
                    Err(error) => HarnessEvent::ContextCompactionFailed {
                        error: super::describe(error),
                    },
                };
                let _ = ack.send(result.map(|_| ()).map_err(|source| AcpError::Agent {
                    context: "rejected the compaction call".to_owned(),
                    source,
                }));
                emit(&self.outputs, SessionOutput::Event { event }).await
            }
            PendingCall::Usage => match result {
                Ok(value) => {
                    for event in self.normalizer.on_plan_usage(&value) {
                        if !emit(&self.outputs, SessionOutput::Event { event }).await {
                            return false;
                        }
                    }
                    true
                }
                Err(error) => {
                    // An agent that cannot state the plan's meters
                    // still holds a conversation; the rings stay
                    // unread rather than the session ending.
                    tracing::warn!(%error, "the agent refused to state the plan's limits");
                    true
                }
            },
        }
    }

    /// Opens a turn: mint its id, announce it, spawn the prompt.
    ///
    /// The prompt is a long-running request — it resolves when the turn
    /// ends — so it is spawned and its answer routed back through the
    /// event channel rather than awaited, keeping the loop free to read
    /// the turn's updates.
    async fn start_turn(&mut self, text: String) -> Result<(), AcpError> {
        if self.active_turn.is_some() {
            return Err(AcpError::TurnInFlight);
        }
        self.turn_seq += 1;
        let turn_id = format!("turn-{}", self.turn_seq);
        self.active_turn = Some(turn_id.clone());
        if !emit(
            &self.outputs,
            SessionOutput::Event {
                event: HarnessEvent::TurnStarted {
                    turn_id: turn_id.clone(),
                },
            },
        )
        .await
        {
            return Err(AcpError::Stopped);
        }
        let client = self.client.clone();
        let session_id = self.session_id.clone();
        let events = self.events.clone();
        let id = turn_id;
        tokio::spawn(async move {
            let result = client
                .prompt(aither_acp::PromptParams::new(
                    session_id,
                    vec![ContentBlock::Text(TextContent {
                        text,
                        annotations: None,
                        meta: None,
                    })],
                ))
                .await;
            let _ = events.send(AgentEvent::PromptFinished {
                turn_id: id,
                result,
            });
        });
        Ok(())
    }

    /// Asks the configured usage method for the plan's windows.
    ///
    /// Fire-and-forget on the same terms as the prompt: the answer lands
    /// on the event channel and the loop keeps moving. With no method
    /// configured there is nothing to ask — the rings stay empty, which is
    /// the honest report for an agent that meters nothing flyco can read.
    fn request_usage(&mut self) {
        let Some(call) = self.config.methods.usage.clone() else {
            return;
        };
        self.spawn_call(call, PendingCall::Usage);
    }

    /// Spawns an extension-method call whose answer comes back as
    /// [`AgentEvent::CallFinished`].
    fn spawn_call(&mut self, call: AcpMethodCall, pending: PendingCall) {
        self.call_seq += 1;
        let id = self.call_seq;
        self.pending_calls.insert(id, pending);
        let client = self.client.clone();
        let events = self.events.clone();
        let params = render_params(&call, &self.session_id);
        let method = call.call;
        tokio::spawn(async move {
            let result = client.request::<Value, Value>(&method, &params).await;
            let _ = events.send(AgentEvent::CallFinished { id, result });
        });
    }

    /// The model-choice writes a `set_model` command means.
    async fn apply_model_choice(&mut self, model: ModelChoice) -> Result<(), AcpError> {
        let id = model_id_to_send(&self.config, &model.model, model.effort.as_deref());
        self.client
            .set_config_option(SessionSetConfigOptionParams::new(
                &self.session_id,
                &self.config.model_option,
                id.as_str(),
            ))
            .await
            .map_err(|source| AcpError::Agent {
                context: format!(
                    "rejected `session/set_config_option {}`",
                    self.config.model_option
                ),
                source,
            })?;
        if self.config.fused_effort_tails.is_empty()
            && let Some(effort) = model.effort
        {
            let Some(option) = &self.config.effort_option else {
                return Err(AcpError::EffortUnsupported);
            };
            self.client
                .set_config_option(SessionSetConfigOptionParams::new(
                    &self.session_id,
                    option,
                    effort.as_str(),
                ))
                .await
                .map_err(|source| AcpError::Agent {
                    context: format!("rejected `session/set_config_option {option}`"),
                    source,
                })?;
        }
        self.model = Some(model.model);
        Ok(())
    }

    /// Answers a pending permission request with the user's decision.
    ///
    /// Flyco's Allow/Deny becomes the agent's own option ids: Allow picks
    /// `allow_once` before `allow_always` — a user pressing Allow approved
    /// this call — and Deny picks `reject_once` before `reject_always`.
    /// The denial's reason text has no channel in ACP, so it is logged
    /// rather than sent. An agent that offered no matching option gets
    /// `cancelled`, the one outcome that needs no id.
    fn decide(&mut self, approval: &ToolApproval) -> Result<(), AcpError> {
        let id = approval.id();
        let Some(pending) = self.pending_permissions.remove(&id) else {
            return Err(AcpError::UnknownApproval(id));
        };
        let outcome = match &approval {
            ToolApproval::Allow { .. } => pick(&pending.options, true),
            ToolApproval::Deny { message, .. } => {
                tracing::info!(%message, "a denial's reason has no ACP channel");
                pick(&pending.options, false)
            }
        };
        let outcome = outcome.unwrap_or(RequestPermissionOutcome::Cancelled);
        let _ = pending.respond.send(RequestPermissionResult {
            outcome,
            meta: None,
        });
        Ok(())
    }

    /// Closes the session and the connection, then waits the task out.
    async fn stop(&mut self) -> Result<(), AcpError> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
        self.queued_messages.clear();
        // `session/close` where the agent advertised it — a courtesy, so a
        // refusal is logged rather than fatal.
        if self.close_supported
            && let Err(error) = self
                .client
                .close_session(aither_acp::SessionCloseParams::new(&self.session_id))
                .await
        {
            tracing::debug!(%error, "the agent refused `session/close`");
        }
        self.client.clone().close();
        if tokio::time::timeout(SHUTDOWN_GRACE, &mut self.connection_task)
            .await
            .is_err()
        {
            self.connection_task.abort();
            return Err(AcpError::Agent {
                context: format!(
                    "did not stop within {}s and was killed",
                    SHUTDOWN_GRACE.as_secs()
                ),
                source: ClientError::Closed { status: None },
            });
        }
        Ok(())
    }
}

/// Selects the option id an Allow or a Deny resolves to.
fn pick(options: &[PermissionOption], allow: bool) -> Option<RequestPermissionOutcome> {
    let preferred = if allow {
        [
            PermissionOptionKind::AllowOnce,
            PermissionOptionKind::AllowAlways,
        ]
    } else {
        [
            PermissionOptionKind::RejectOnce,
            PermissionOptionKind::RejectAlways,
        ]
    };
    preferred
        .iter()
        .find_map(|kind| {
            options
                .iter()
                .find(|option| option.kind == *kind)
                .map(|option| option.option_id.clone())
        })
        .map(|option_id| RequestPermissionOutcome::Selected { option_id })
}

/// Either half of the driver loop's select.
enum AgentOrCommand {
    /// A handle command.
    Command(DriverCommand),
    /// An agent event — boxed: it dwarfs a command and is the rarer half.
    Agent(Box<AgentEvent>),
}

/// The [`ClientHandler`] the agent's traffic is dispatched to.
///
/// Thin by construction: everything it receives is forwarded to the driver
/// task as an [`AgentEvent`], because the session's state — pending
/// approvals, the active turn, the replay flag's meaning — lives there and
/// nowhere else. The one exception is `replaying`, an atomic the handler
/// itself reads so a `session/load` replay is suppressed before it ever
/// reaches the transcript.
#[derive(Clone)]
struct AgentHandler {
    /// Where agent traffic is forwarded.
    events: mpsc::UnboundedSender<AgentEvent>,
    /// Set while `session/load` replays history: transcript-content updates
    /// are the past re-sent, and forwarding them would duplicate it.
    replaying: Arc<AtomicBool>,
}

impl ClientHandler for AgentHandler {
    /// No file-system, terminal, or elicitation surface is advertised:
    /// the agents flyco drives carry their own tools, and an advertised
    /// capability would only reroute them through flycod.
    fn capabilities(&self) -> ClientCapabilities {
        ClientCapabilities::default()
    }

    async fn session_update(&self, notification: SessionNotification) {
        use aither_acp::SessionUpdate as Update;
        // Replay suppression: while `session/load` re-sends history, the
        // updates that would duplicate the transcript are dropped here.
        // Reports about the present — modes, options, commands, usage —
        // still pass, because a load's answer about them is current.
        if self.replaying.load(Ordering::SeqCst)
            && matches!(
                notification.update,
                Update::AgentMessageChunk(_)
                    | Update::AgentThoughtChunk(_)
                    | Update::UserMessageChunk(_)
                    | Update::Plan(_)
                    | Update::ToolCall(_)
                    | Update::ToolCallUpdate(_)
            )
        {
            return;
        }
        let _ = self.events.send(AgentEvent::Update(notification));
    }

    async fn request_permission(
        &self,
        params: RequestPermissionParams,
    ) -> Result<RequestPermissionResult, JsonRpcError> {
        let (respond, answer) = oneshot::channel();
        if self
            .events
            .send(AgentEvent::Permission { params, respond })
            .is_err()
        {
            return Ok(RequestPermissionResult {
                outcome: RequestPermissionOutcome::Cancelled,
                meta: None,
            });
        }
        // The agent is blocked until this resolves; a dropped sender is a
        // session that ended, which the agent reads as a cancellation.
        Ok(answer.await.unwrap_or(RequestPermissionResult {
            outcome: RequestPermissionOutcome::Cancelled,
            meta: None,
        }))
    }

    async fn notification(&self, notification: JsonRpcNotification) {
        let _ = self.events.send(AgentEvent::Vendor(notification));
    }
}

async fn emit(outputs: &mpsc::Sender<SessionOutput>, output: SessionOutput) -> bool {
    if outputs.send(output).await.is_err() {
        tracing::debug!("nothing is consuming the session's output stream");
        return false;
    }
    true
}
