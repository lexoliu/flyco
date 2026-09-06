//! Mounting flyco's MCP server into the harness, and proving it landed.
//!
//! [`crate::mcp`] is the server. This module is the other half: telling the
//! harness to run it, telling it to run *only* the servers flyco named, and
//! refusing to hand the session to an agent that cannot see them.
//!
//! # One set, two declarations
//!
//! Every session's MCP surface is exactly [`Mount`]: flyco's own local
//! server plus the servers the user registered, provisioned into
//! [`DaemonConfig::mcp_servers`](crate::config::DaemonConfig::mcp_servers).
//! It is written twice, and the two writes do different jobs.
//!
//! * The **harness's own start options** — the Agent SDK's `mcpServers`,
//!   Codex's `thread/start.config` — *mount* the servers. They travel with
//!   the session, need no privilege, and work on a developer's machine.
//! * The **root-owned configuration files** — Claude Code's
//!   `/etc/claude-code/managed-settings.json` and `managed-mcp.json`,
//!   Codex's `$CODEX_HOME/config.toml` — make the set *exclusive*. They
//!   outrank every other settings source and live in directories the agent's
//!   user cannot write, which is what turns an allowlist into a fact about
//!   the filesystem rather than a request.
//!
//! Both are rendered from this one structure, so they cannot disagree about
//! which servers a session has.
//!
//! # Serialized rather than templated
//!
//! These documents are JSON and TOML, and a server name or a bearer token is
//! whatever the user typed. Escaping is therefore the serializer's problem
//! and not a template author's — the same reasoning that makes
//! `flyco_provider::flycod` a serde structure. What a template would buy
//! (a compiler that notices a dropped field) the typed structures below buy
//! too, and the snapshot tests pin the bytes.
//!
//! # Proving the mount
//!
//! A session whose agent cannot call `budget_status` is a session that will
//! spend the user's money without ever being able to look at the meter, so
//! the daemon does not run one. Each driver asks its own harness what it
//! mounted — the Agent SDK's `mcpServerStatus()`, Codex's
//! `mcpServerStatus/list` — and hands the answer to [`verify`], which fails
//! the session with a sentence naming what is missing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use flyco_core::{McpServerConfig, McpServerMount};
use serde::{Deserialize, Serialize};

use crate::mcp::{BUDGET_STATUS, MACHINE_RESIZE, MACHINE_STATUS};

/// The name every harness announces flyco's own server under.
///
/// Load-bearing rather than cosmetic: it is half of the tool identifiers the
/// model calls (`mcp__flyco__machine_status`), it is the key the managed
/// files and the allowlist agree on, and it is what [`verify`] looks for in
/// a harness's report.
pub const FLYCO: &str = "flyco";

/// The tools a mounted flyco server must expose before a session may run.
///
/// All three, not a chosen subset: an agent that can see the machine but
/// not the budget will spend past a limit it was never able to read, and
/// one that can read both but not resize is stuck on hardware it was told
/// it could change. If any of them is missing the mount is broken, not
/// partial.
pub const REQUIRED_TOOLS: [&str; 3] = [MACHINE_STATUS, BUDGET_STATUS, MACHINE_RESIZE];

/// `managed-settings.json`, inside [`ClaudeConfig::managed_dir`].
///
/// [`ClaudeConfig::managed_dir`]: crate::config::ClaudeConfig::managed_dir
pub const MANAGED_SETTINGS: &str = "managed-settings.json";

/// `managed-mcp.json`, inside [`ClaudeConfig::managed_dir`].
///
/// [`ClaudeConfig::managed_dir`]: crate::config::ClaudeConfig::managed_dir
pub const MANAGED_MCP: &str = "managed-mcp.json";

/// The mount could not be built, written, or proved.
#[derive(Debug, thiserror::Error)]
pub enum MountError {
    /// This process cannot say where its own executable is, so it cannot
    /// tell a harness how to launch a second copy of itself.
    #[error(
        "flycod cannot locate its own executable, so it cannot tell the harness how to run `flycod mcp`"
    )]
    Executable(#[source] std::io::Error),
    /// A managed-policy file could not be written.
    #[error("could not write the harness's managed MCP policy at {path}")]
    Write {
        /// The file being written.
        path: PathBuf,
        /// The underlying cause.
        #[source]
        source: std::io::Error,
    },
    /// The harness came up without flyco's tools.
    #[error(transparent)]
    NotMounted(#[from] NotMounted),
}

/// The harness did not mount flyco's server, or mounted a broken one.
///
/// Its own type because it is the one failure here that is *not* flycod's
/// mistake to fix at runtime: the session must stop, and the sentence has to
/// say which of the three states it is in, because they have three different
/// causes — a policy file that did not take, a server that failed to start,
/// and a `flycod mcp` that is older than the daemon supervising it.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NotMounted {
    /// The harness never heard of flyco's server.
    #[error(
        "the harness did not mount flyco's `{FLYCO}` MCP server; it reported {reported}. \
         The agent would run with no way to read its machine or its budget."
    )]
    Absent {
        /// What the harness did report, for the log line that follows.
        reported: String,
    },
    /// The server is configured but the harness could not talk to it.
    #[error(
        "the harness mounted flyco's `{FLYCO}` MCP server but is not connected to it ({status}). \
         `flycod mcp` could not be started or did not complete its handshake."
    )]
    NotConnected {
        /// The connection state the harness reported.
        status: String,
    },
    /// It answered, but not with the tools this daemon requires.
    #[error(
        "flyco's `{FLYCO}` MCP server is connected but does not expose {missing}. \
         The `flycod mcp` the harness launched is not the one this daemon expects."
    )]
    MissingTools {
        /// The required tools that were not advertised.
        missing: String,
    },
}

/// How a harness launches `flycod mcp` for one session.
///
/// The program is this daemon's own executable, read at runtime rather than
/// configured: the harness must launch *this* build, and a machine that has
/// upgraded `flycod` under a running session should not be pointed at a
/// path that no longer holds the binary the supervisor came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlycoServer {
    program: PathBuf,
    args: Vec<String>,
}

impl FlycoServer {
    /// The command that serves this session's tools over stdio.
    ///
    /// `config` is the file `flycod run` was pointed at. The second process
    /// shares nothing with this one but that file and the daemon token in
    /// it, which is the whole point: every fact it reports comes from the
    /// control plane rather than from this daemon's memory.
    ///
    /// # Errors
    ///
    /// Returns [`MountError::Executable`] if the running executable's path
    /// cannot be read.
    pub fn of(config: &Path) -> Result<Self, MountError> {
        Ok(Self {
            program: std::env::current_exe().map_err(MountError::Executable)?,
            args: vec![
                "mcp".to_owned(),
                "--config".to_owned(),
                config.to_string_lossy().into_owned(),
            ],
        })
    }

    /// Builds one from an explicit program, for a test that must not depend
    /// on where the test runner's binary lives.
    #[cfg(test)]
    fn at(program: &str, config: &str) -> Self {
        Self {
            program: PathBuf::from(program),
            args: vec!["mcp".to_owned(), "--config".to_owned(), config.to_owned()],
        }
    }

    fn program(&self) -> String {
        self.program.to_string_lossy().into_owned()
    }

    /// `[program, ...args]`, which is how Claude Code's managed allowlist
    /// identifies a stdio server it will admit.
    fn command_line(&self) -> Vec<String> {
        let mut line = vec![self.program()];
        line.extend(self.args.iter().cloned());
        line
    }
}

/// Every MCP server one session's harness is given.
///
/// Flyco's own server is a field rather than one entry among the rest,
/// because it is the one server that is not the user's to remove and the
/// only one whose absence stops the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    flyco: FlycoServer,
    registered: Vec<McpServerMount>,
}

impl Mount {
    /// The mount for one session.
    #[must_use]
    pub const fn new(flyco: FlycoServer, registered: Vec<McpServerMount>) -> Self {
        Self { flyco, registered }
    }

    /// Every server, flyco's first, as the harness-neutral pairs the
    /// renderings below iterate.
    ///
    /// A [`BTreeMap`] because both harnesses key their configuration by
    /// server name, and because a document whose key order depended on a
    /// database's `ORDER BY` would be a file that changes for no reason.
    fn servers(&self) -> BTreeMap<&str, Server<'_>> {
        let mut servers = BTreeMap::new();
        servers.insert(FLYCO, Server::Flyco(&self.flyco));
        for entry in &self.registered {
            servers.insert(entry.name.as_str(), Server::Registered(&entry.config));
        }
        servers
    }

    /// `managed-mcp.json`: the servers Claude Code loads, at the enterprise
    /// scope that outranks every other source.
    ///
    /// A machine with this file has an "enterprise MCP config present",
    /// which suppresses the project, user, local and claude.ai scopes
    /// wholesale — so the servers listed here are not merely allowed, they
    /// are the only ones auto-discovery can reach.
    #[must_use]
    pub fn claude_managed_mcp(&self) -> String {
        json(&ManagedMcp {
            mcp_servers: self
                .servers()
                .into_iter()
                .map(|(name, server)| (name.to_owned(), server.claude()))
                .collect(),
        })
    }

    /// `managed-settings.json`: the allowlist, read from managed settings
    /// and nowhere else.
    ///
    /// `allowManagedMcpServersOnly` is what makes it binding: without it a
    /// user-level settings file could widen `allowedMcpServers`, and with
    /// it the list below is the only one the CLI consults. A server the
    /// agent adds by any other route is refused at connect time because it
    /// matches no entry here.
    #[must_use]
    pub fn claude_managed_settings(&self) -> String {
        json(&ManagedSettings {
            allow_managed_mcp_servers_only: true,
            allowed_mcp_servers: self
                .servers()
                .into_iter()
                .map(|(name, server)| server.claude_allowlist_entry(name))
                .collect(),
        })
    }

    /// The `mcpServers` option the Agent SDK's session start carries.
    ///
    /// For a machine with no managed policy — a developer's own `flycod`,
    /// which is not root and writes no such file. There, this is both what
    /// mounts the servers and, with `strictMcpConfig`, what makes the set
    /// exclusive.
    ///
    /// Not for a provisioned machine. The CLI does not merge the two
    /// declarations: an enterprise MCP config is exclusive, and a server
    /// also passed here is refused as `MCP server blocked by enterprise
    /// policy` (issue #197). There the managed file is the only
    /// declaration.
    #[must_use]
    pub fn claude_sdk_servers(&self) -> BTreeMap<String, ClaudeMcpServer> {
        self.servers()
            .into_iter()
            .map(|(name, server)| (name.to_owned(), server.claude()))
            .collect()
    }

    /// The `[mcp_servers]` table of Codex's `config.toml`, and the same
    /// table as a `thread/start` config override.
    ///
    /// Codex has no separate allowlist document: `[mcp_servers.<id>]` *is*
    /// the identity, so the root-owned `config.toml` this renders is the
    /// complete registry, and `--strict-config` refuses anything it does
    /// not recognise. `required` on flyco's entry makes a server that will
    /// not start a thread that will not open.
    #[must_use]
    pub fn codex_servers(&self) -> BTreeMap<String, CodexMcpServer> {
        self.servers()
            .into_iter()
            .map(|(name, server)| (name.to_owned(), server.codex()))
            .collect()
    }

    /// Writes Claude Code's two managed-policy files into `dir`.
    ///
    /// # Errors
    ///
    /// Returns [`MountError::Write`] if the directory or either file cannot
    /// be written — which on a provisioned machine means flycod is not root,
    /// and is a session that must not start rather than one that runs
    /// unenforced.
    pub async fn write_claude_managed(&self, dir: &Path) -> Result<(), MountError> {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|source| MountError::Write {
                path: dir.to_owned(),
                source,
            })?;
        for (name, body) in [
            (MANAGED_MCP, self.claude_managed_mcp()),
            (MANAGED_SETTINGS, self.claude_managed_settings()),
        ] {
            let path = dir.join(name);
            tokio::fs::write(&path, body)
                .await
                .map_err(|source| MountError::Write { path, source })?;
        }
        tracing::info!(
            dir = %dir.display(),
            servers = self.registered.len() + 1,
            "wrote Claude Code's managed MCP policy"
        );
        Ok(())
    }
}

/// One server, before it is spelled in a harness's own vocabulary.
#[derive(Debug, Clone, Copy)]
enum Server<'a> {
    /// flyco's own, launched as a second `flycod` process.
    Flyco(&'a FlycoServer),
    /// One the user registered.
    Registered(&'a McpServerConfig),
}

impl Server<'_> {
    /// As Claude Code's `managed-mcp.json` and the Agent SDK spell it.
    fn claude(self) -> ClaudeMcpServer {
        match self {
            Self::Flyco(flyco) => ClaudeMcpServer::Stdio {
                command: flyco.program(),
                args: flyco.args.clone(),
                env: BTreeMap::new(),
                // flyco's tools have to be in the turn-1 prompt: an agent
                // that only discovers `budget_status` behind a tool search
                // is an agent that never looks at the meter. It is also
                // what makes the CLI block on connecting to this server,
                // which is what the mount check then reports on.
                always_load: true,
            },
            Self::Registered(McpServerConfig::Stdio { command, args, env }) => {
                ClaudeMcpServer::Stdio {
                    command: command.clone(),
                    args: args.clone(),
                    env: env
                        .iter()
                        .map(|entry| (entry.key.clone(), entry.value.clone()))
                        .collect(),
                    always_load: false,
                }
            }
            Self::Registered(McpServerConfig::Http { url, headers }) => ClaudeMcpServer::Http {
                url: url.clone(),
                headers: headers
                    .iter()
                    .map(|header| (header.name.clone(), header.value.clone()))
                    .collect(),
            },
        }
    }

    /// One `allowedMcpServers` entry.
    ///
    /// A stdio server is pinned to its exact command line rather than to its
    /// name alone, so re-registering the name against a different executable
    /// is not a way past the allowlist.
    fn claude_allowlist_entry(self, name: &str) -> AllowedMcpServer {
        let (command, url) = match self {
            Self::Flyco(flyco) => (Some(flyco.command_line()), None),
            Self::Registered(McpServerConfig::Stdio { command, args, .. }) => {
                let mut line = vec![command.clone()];
                line.extend(args.iter().cloned());
                (Some(line), None)
            }
            Self::Registered(McpServerConfig::Http { url, .. }) => (None, Some(url.clone())),
        };
        AllowedMcpServer {
            server_name: name.to_owned(),
            server_command: command,
            server_url: url,
        }
    }

    /// As Codex's `config.toml` spells it.
    fn codex(self) -> CodexMcpServer {
        match self {
            Self::Flyco(flyco) => CodexMcpServer {
                command: Some(flyco.program()),
                args: flyco.args.clone(),
                env: BTreeMap::new(),
                url: None,
                http_headers: BTreeMap::new(),
                enabled: true,
                // A thread that opens without flyco's server is a thread
                // whose agent cannot see its budget, so Codex is told to
                // treat this one as a precondition rather than an extra.
                required: true,
            },
            Self::Registered(McpServerConfig::Stdio { command, args, env }) => CodexMcpServer {
                command: Some(command.clone()),
                args: args.clone(),
                env: env
                    .iter()
                    .map(|entry| (entry.key.clone(), entry.value.clone()))
                    .collect(),
                url: None,
                http_headers: BTreeMap::new(),
                enabled: true,
                required: false,
            },
            Self::Registered(McpServerConfig::Http { url, headers }) => CodexMcpServer {
                command: None,
                args: Vec::new(),
                env: BTreeMap::new(),
                url: Some(url.clone()),
                http_headers: headers
                    .iter()
                    .map(|header| (header.name.clone(), header.value.clone()))
                    .collect(),
                enabled: true,
                required: false,
            },
        }
    }
}

/// One MCP server, in Claude Code's own vocabulary.
///
/// Every field is written even when it is empty, which makes the rendered
/// documents a little longer and the contract exact: the SDK's own types
/// distinguish "absent" from "present and empty" under
/// `exactOptionalPropertyTypes`, and a protocol whose optionality had to be
/// reconstructed on the far side is a protocol with two spellings for one
/// server.
///
/// This is the SDK's `McpServerConfig` verbatim — `camelCase` fields and
/// all — rather than a flycod-shaped restatement of it, which is why it is
/// the one place the flycod⇄sidecar protocol does not spell a field
/// `snake_case`. The reason is the same one that makes
/// [`SidecarEvent::SdkMessage`] opaque: this value is not flycod's to
/// interpret, it is Claude Code's, and one document travels from here into
/// `managed-mcp.json` and through the sidecar into `Options.mcpServers`
/// without either end restating it.
///
/// [`SidecarEvent::SdkMessage`]: crate::harness::claude::protocol::SidecarEvent::SdkMessage
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClaudeMcpServer {
    /// A process the CLI launches and speaks to over stdio.
    #[serde(rename_all = "camelCase")]
    Stdio {
        /// Executable to run.
        command: String,
        /// Arguments passed to it.
        args: Vec<String>,
        /// Extra environment, on top of the CLI's own.
        env: BTreeMap<String, String>,
        /// Keep this server's tools in the prompt rather than behind a tool
        /// search, and block startup until it connects.
        always_load: bool,
    },
    /// A remote server over streamable HTTP.
    #[serde(rename_all = "camelCase")]
    Http {
        /// Absolute endpoint URL.
        url: String,
        /// Headers sent with every request.
        headers: BTreeMap<String, String>,
    },
}

/// `managed-mcp.json`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ManagedMcp {
    mcp_servers: BTreeMap<String, ClaudeMcpServer>,
}

/// `managed-settings.json`, as far as MCP is concerned.
///
/// Deliberately not the whole settings document: the permission rules, the
/// hooks and the memory switches this file also carries are their own
/// takeovers, and a struct that claimed to render them would have to render
/// them.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ManagedSettings {
    allow_managed_mcp_servers_only: bool,
    allowed_mcp_servers: Vec<AllowedMcpServer>,
}

/// One `allowedMcpServers` entry.
///
/// The `server_` prefixes are Claude Code's own field names, not a habit:
/// this structure is a foreign document's schema and renaming its fields
/// would produce a file the CLI ignores.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[expect(
    clippy::struct_field_names,
    reason = "the field names are Claude Code's managed-settings schema"
)]
struct AllowedMcpServer {
    server_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    server_command: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    server_url: Option<String>,
}

/// One `[mcp_servers.<id>]` table of Codex's `config.toml`.
///
/// Field order is serialization order and TOML puts every scalar before the
/// first table it meets, so the maps come last or `toml` refuses to
/// serialize the document at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodexMcpServer {
    /// Executable to run, for a stdio server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Arguments passed to it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Absolute endpoint URL, for a remote server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Whether Codex loads this server at all.
    pub enabled: bool,
    /// Whether a thread may open without it.
    pub required: bool,
    /// Extra environment for a stdio server's process.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Headers sent to a remote server.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub http_headers: BTreeMap<String, String>,
}

/// Where one server has got to in the harness's own account of it.
///
/// Three states rather than a boolean because the middle one is not a
/// failure: a harness that is still dialling a server has not yet answered
/// the question, and a check that read it as "not connected" would refuse
/// sessions for being asked half a second early. Each driver waits out its
/// own [`Pending`](Self::Pending)s before it verifies anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountState {
    /// Live: the tool list beside it is the server's own answer.
    Connected,
    /// Still starting, or waiting on an authorization. Not an answer yet.
    Pending,
    /// The harness gave up on it, or was told not to load it at all.
    Failed,
}

/// What one harness reports about one server it was told to mount.
///
/// Harness-neutral on purpose: the Agent SDK answers `mcpServerStatus()`
/// with a status string and a tool list, Codex answers
/// `mcpServerStatus/list` with a runtime status and a tool map, and
/// [`verify`] should not have an opinion about which it is reading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountedServer {
    /// Name the harness knows the server by.
    pub name: String,
    /// The harness's own word for the connection state, verbatim, so a
    /// refusal quotes what the harness said rather than flycod's reading
    /// of it.
    pub status: String,
    /// What that word means to [`verify`].
    pub state: MountState,
    /// The tools it advertises. Meaningful only when
    /// [`Connected`](MountState::Connected).
    pub tools: Vec<String>,
}

/// Whether every server has stopped moving, so a report can be judged.
///
/// The condition each driver waits on before calling [`verify`]. Named here
/// rather than written twice because "wait until nothing is still dialling"
/// is one rule, not one per harness.
#[must_use]
pub fn settled(servers: &[MountedServer]) -> bool {
    servers
        .iter()
        .all(|server| server.state != MountState::Pending)
}

/// Whether a reported tool is the one flyco is looking for.
///
/// The harnesses do not agree on which name they report: some answer with
/// the server's own tool name and some with the namespaced identifier the
/// model calls it by (`mcp__flyco__machine_status`). Both are the same tool,
/// and a check that insisted on one spelling would be a check that passes on
/// one harness and fails on the other for no reason.
fn is_tool(reported: &str, wanted: &str) -> bool {
    reported == wanted || reported.ends_with(&format!("__{wanted}"))
}

/// Refuses a session whose harness did not mount flyco's tools.
///
/// # Errors
///
/// Returns [`NotMounted`] naming which of the three states the harness is
/// in: no flyco server, one it could not connect to, or one missing tools
/// this daemon requires.
pub fn verify(servers: &[MountedServer]) -> Result<(), NotMounted> {
    let Some(flyco) = servers.iter().find(|server| server.name == FLYCO) else {
        let reported = if servers.is_empty() {
            "no MCP servers at all".to_owned()
        } else {
            format!(
                "only {}",
                servers
                    .iter()
                    .map(|server| server.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        return Err(NotMounted::Absent { reported });
    };
    if flyco.state != MountState::Connected {
        return Err(NotMounted::NotConnected {
            status: flyco.status.clone(),
        });
    }
    let missing: Vec<&str> = REQUIRED_TOOLS
        .into_iter()
        .filter(|wanted| !flyco.tools.iter().any(|reported| is_tool(reported, wanted)))
        .collect();
    if missing.is_empty() {
        tracing::info!(
            tools = flyco.tools.len(),
            "flyco's MCP server is mounted and the session's tools are live"
        );
        Ok(())
    } else {
        Err(NotMounted::MissingTools {
            missing: missing.join(", "),
        })
    }
}

/// A managed-policy document, as it is written to disk.
///
/// Pretty-printed with a trailing newline because these are files an
/// operator reads over a session's shoulder when the agent says it cannot
/// see its budget.
fn json<T: Serialize>(document: &T) -> String {
    let mut text = serde_json::to_string_pretty(document)
        .expect("every managed-policy document serializes to JSON");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use flyco_core::{EnvEntry, HeaderEntry, McpServerConfig, McpServerMount};

    use super::{FlycoServer, Mount, MountedServer, NotMounted, verify};

    const FLYCOD: &str = "/usr/local/bin/flycod";
    const CONFIG: &str = "/etc/flyco/flycod.toml";

    fn registered() -> Vec<McpServerMount> {
        vec![
            McpServerMount {
                name: "deepwiki".to_owned(),
                config: McpServerConfig::Http {
                    url: "https://mcp.deepwiki.com/mcp".to_owned(),
                    headers: vec![HeaderEntry {
                        name: "authorization".to_owned(),
                        value: "Bearer a-registered-token".to_owned(),
                    }],
                },
            },
            McpServerMount {
                name: "git".to_owned(),
                config: McpServerConfig::Stdio {
                    command: "bunx".to_owned(),
                    args: vec![
                        "-y".to_owned(),
                        "@modelcontextprotocol/server-git".to_owned(),
                    ],
                    env: vec![EnvEntry {
                        key: "GIT_DIR".to_owned(),
                        value: "/srv/flyco/work/.git".to_owned(),
                    }],
                },
            },
        ]
    }

    fn alone() -> Mount {
        Mount::new(FlycoServer::at(FLYCOD, CONFIG), Vec::new())
    }

    fn with_servers() -> Mount {
        Mount::new(FlycoServer::at(FLYCOD, CONFIG), registered())
    }

    fn codex_toml(mount: &Mount) -> String {
        #[derive(serde::Serialize)]
        struct Document {
            mcp_servers: std::collections::BTreeMap<String, super::CodexMcpServer>,
        }
        toml::to_string_pretty(&Document {
            mcp_servers: mount.codex_servers(),
        })
        .expect("the Codex MCP tables serialize")
    }

    #[test]
    fn claudes_managed_mcp_declares_flyco_alone_when_the_user_registered_nothing() {
        assert_eq!(
            alone().claude_managed_mcp(),
            include_str!("../fixtures/mount/claude-managed-mcp-flyco-only.json")
        );
    }

    #[test]
    fn claudes_managed_mcp_declares_flyco_beside_the_users_own_servers() {
        assert_eq!(
            with_servers().claude_managed_mcp(),
            include_str!("../fixtures/mount/claude-managed-mcp.json")
        );
    }

    #[test]
    fn claudes_managed_settings_allowlist_is_the_same_set_and_only_that_set() {
        assert_eq!(
            alone().claude_managed_settings(),
            include_str!("../fixtures/mount/claude-managed-settings-flyco-only.json")
        );
        assert_eq!(
            with_servers().claude_managed_settings(),
            include_str!("../fixtures/mount/claude-managed-settings.json")
        );
    }

    #[test]
    fn codex_declares_the_same_set_in_its_own_vocabulary() {
        assert_eq!(
            codex_toml(&alone()),
            include_str!("../fixtures/mount/codex-mcp-flyco-only.toml")
        );
        assert_eq!(
            codex_toml(&with_servers()),
            include_str!("../fixtures/mount/codex-mcp.toml")
        );
    }

    #[test]
    fn the_sdk_option_and_the_managed_file_name_one_set() {
        // Two renderings of one structure; if they could disagree, a
        // session would mount servers its own allowlist refuses.
        let mount = with_servers();
        let sdk: Vec<String> = mount.claude_sdk_servers().into_keys().collect();
        let codex: Vec<String> = mount.codex_servers().into_keys().collect();
        assert_eq!(sdk, ["deepwiki", "flyco", "git"]);
        assert_eq!(sdk, codex);
    }

    fn mounted(name: &str, connected: bool, tools: &[&str]) -> MountedServer {
        MountedServer {
            name: name.to_owned(),
            status: if connected { "connected" } else { "failed" }.to_owned(),
            state: if connected {
                super::MountState::Connected
            } else {
                super::MountState::Failed
            },
            tools: tools.iter().map(|tool| (*tool).to_owned()).collect(),
        }
    }

    #[test]
    fn a_report_with_anything_still_dialling_is_not_yet_an_answer() {
        let dialling = MountedServer {
            name: "flyco".to_owned(),
            status: "pending".to_owned(),
            state: super::MountState::Pending,
            tools: Vec::new(),
        };
        assert!(!super::settled(std::slice::from_ref(&dialling)));
        assert!(super::settled(&[mounted(
            "flyco",
            true,
            &super::REQUIRED_TOOLS
        )]));
        assert!(super::settled(&[]));

        // Judged anyway — because a driver's wait ran out — it is refused
        // in the harness's own words rather than in flycod's.
        assert_eq!(
            verify(&[dialling]).expect_err("a server still dialling is not a mount"),
            NotMounted::NotConnected {
                status: "pending".to_owned()
            }
        );
    }

    #[test]
    fn a_harness_that_mounted_flycos_tools_may_run_a_session() {
        verify(&[mounted("flyco", true, &super::REQUIRED_TOOLS)])
            .expect("a complete mount is accepted");
    }

    #[test]
    fn the_namespaced_spelling_of_a_tool_is_the_same_tool() {
        verify(&[mounted(
            "flyco",
            true,
            &[
                "mcp__flyco__machine_status",
                "mcp__flyco__budget_status",
                "mcp__flyco__machine_resize",
            ],
        )])
        .expect("a harness that reports the namespaced identifier mounted the same tools");
    }

    #[test]
    fn a_harness_with_no_flyco_server_refuses_the_session() {
        let error = verify(&[mounted("git", true, &["git_status"])])
            .expect_err("a session without flyco's tools must not run");
        assert!(matches!(error, NotMounted::Absent { .. }));
        assert!(error.to_string().contains("only git"));

        let empty = verify(&[]).expect_err("nothing mounted is nothing mounted");
        assert!(empty.to_string().contains("no MCP servers at all"));
    }

    #[test]
    fn a_flyco_server_that_did_not_connect_refuses_the_session() {
        let error = verify(&[mounted("flyco", false, &[])])
            .expect_err("a server that never started is not a mount");
        assert_eq!(
            error,
            NotMounted::NotConnected {
                status: "failed".to_owned()
            }
        );
    }

    #[test]
    fn a_tool_list_without_machine_status_refuses_the_session() {
        let error = verify(&[mounted("flyco", true, &["budget_status", "machine_resize"])])
            .expect_err("a partial mount is a broken one");
        assert_eq!(
            error,
            NotMounted::MissingTools {
                missing: "machine_status".to_owned()
            }
        );
        assert!(error.to_string().contains("machine_status"));
    }
}
