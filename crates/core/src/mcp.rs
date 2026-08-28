//! The MCP registry: the servers flyco hands to every session.
//!
//! Agents may not configure MCP for themselves — the harness config is
//! root-owned and the allowlist is enforced on the machine — so this
//! registry is the only place a server is added, and it belongs to the user.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::env::EnvEntry;
use crate::id::McpServerId;

/// One HTTP header sent with every request to a remote MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HeaderEntry {
    /// Header name.
    pub name: String,
    /// Header value.
    pub value: String,
}

/// How a session reaches one MCP server.
///
/// The transport is the tag, so a stdio server can never carry a URL and a
/// remote one can never carry a command line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum McpServerConfig {
    /// A process the daemon launches on the session machine and speaks to
    /// over stdio.
    Stdio {
        /// Executable to run.
        command: String,
        /// Arguments passed to it.
        args: Vec<String>,
        /// Extra environment for the process, on top of the session's.
        env: Vec<EnvEntry>,
    },
    /// A remote server reached over streamable HTTP.
    Http {
        /// Absolute endpoint URL.
        url: String,
        /// Headers sent with every request, typically an authorization.
        headers: Vec<HeaderEntry>,
    },
}

/// Request body of `POST /v1/mcp-servers` and `PATCH /v1/mcp-servers/{id}`.
///
/// A patch carries the whole document rather than a diff: an MCP server's
/// configuration is small, and a half-applied transport change is a state
/// nobody should be able to describe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UpsertMcpServer {
    /// Name the harness announces the server under. Unique per user.
    pub name: String,
    /// How to reach it.
    pub config: McpServerConfig,
    /// Whether sessions are given this server at all.
    pub enabled: bool,
}

/// One row of `GET /v1/mcp-servers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct McpServerView {
    /// Identifier.
    pub id: McpServerId,
    /// Name the harness announces it under.
    pub name: String,
    /// How to reach it.
    pub config: McpServerConfig,
    /// Whether sessions are given it.
    pub enabled: bool,
    /// Last change, seconds since the Unix epoch.
    pub updated_at_unix: u64,
}

#[cfg(test)]
mod tests {
    use super::{HeaderEntry, McpServerConfig, UpsertMcpServer};
    use crate::env::EnvEntry;

    #[test]
    fn a_transport_is_tagged_and_round_trips() {
        for config in [
            McpServerConfig::Stdio {
                command: "bunx".to_owned(),
                args: vec![
                    "-y".to_owned(),
                    "@modelcontextprotocol/server-git".to_owned(),
                ],
                env: vec![EnvEntry {
                    key: "GIT_DIR".to_owned(),
                    value: "/workspace/.git".to_owned(),
                }],
            },
            McpServerConfig::Http {
                url: "https://mcp.deepwiki.com/mcp".to_owned(),
                headers: vec![HeaderEntry {
                    name: "authorization".to_owned(),
                    value: "Bearer token".to_owned(),
                }],
            },
        ] {
            let request = UpsertMcpServer {
                name: "git".to_owned(),
                config,
                enabled: true,
            };
            let json = serde_json::to_value(&request).expect("serialize");
            assert!(json["config"]["transport"].is_string());

            let back: UpsertMcpServer = serde_json::from_value(json).expect("deserialize");
            assert_eq!(back, request);
        }
    }
}
