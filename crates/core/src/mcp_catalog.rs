//! The MCP catalog: the official MCP Registry, read for the picker.
//!
//! A registry entry describes a server in the registry's own terms — remote
//! endpoints, `npm` and `PyPI` packages, the headers and environment each one
//! wants. What the picker needs is narrower: whether flyco can run it, what
//! the user has to type before it can, and a name to register it under. The
//! control plane makes that translation, so a browser never has to know the
//! registry's schema and the same entry cannot be turned into two different
//! [`McpServerConfig`](crate::McpServerConfig)s by two clients.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// How a catalog server would run once added.
///
/// The registry also lists Docker images, `NuGet` packages and `.mcpb`
/// bundles; none of those runs on a session machine, so an entry offering
/// only those is not in the catalog at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CatalogInstallKind {
    /// A remote endpoint reached over streamable HTTP.
    Remote,
    /// An `npm` package run with `npx` on the session machine.
    Npm,
    /// A `PyPI` package run with `uvx` on the session machine.
    Pypi,
}

impl CatalogInstallKind {
    /// The token the API spells this kind as.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Remote => "remote",
            Self::Npm => "npm",
            Self::Pypi => "pypi",
        }
    }
}

/// A value the user supplies before a catalog server can be added: an
/// authorization header, an environment variable, a command-line argument.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CatalogInput {
    /// What the value is filed under in
    /// [`InstallCatalogMcpServer::values`]: `header:<name>`, `env:<NAME>`,
    /// `arg:<name>` or `var:<variable>`.
    pub key: String,
    /// The name shown beside the field.
    pub label: String,
    /// The registry's explanation of the value, when it gave one.
    pub description: Option<String>,
    /// Whether the server cannot be added without it.
    pub required: bool,
    /// Whether the field should hide what is typed.
    pub secret: bool,
    /// What the field starts out as, when the registry names a default.
    pub default: Option<String>,
}

/// One way of running a catalog server, and what it needs from the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CatalogMcpInstall {
    /// How it runs.
    pub kind: CatalogInstallKind,
    /// A line naming the endpoint or the package: `Remote · mcp.example`,
    /// `npx @acme/mcp`.
    pub label: String,
    /// What the user fills in first. Empty when one click is enough.
    pub inputs: Vec<CatalogInput>,
}

/// One server of `GET /v1/catalog/mcp-servers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CatalogMcpServer {
    /// The registry's reverse-DNS name, `io.github.owner/server`.
    pub name: String,
    /// The display name, when the publisher gave one.
    pub title: Option<String>,
    /// The publisher's one-line description.
    pub description: String,
    /// The version the registry lists as latest.
    pub version: String,
    /// Where the source lives, when the publisher said.
    pub repository_url: Option<String>,
    /// The publisher's site, when they named one.
    pub website_url: Option<String>,
    /// The name the server is registered under unless the user picks
    /// another: the tail of the registry name, in the characters a harness
    /// can announce a server as.
    pub suggested_name: String,
    /// Every way flyco can run it, most preferred first.
    pub installs: Vec<CatalogMcpInstall>,
}

/// One page of `GET /v1/catalog/mcp-servers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct McpCatalogPage {
    /// The servers on this page that flyco can run.
    pub servers: Vec<CatalogMcpServer>,
    /// Cursor for the next page, absent on the last.
    pub next_cursor: Option<String>,
}

/// Request body of `POST /v1/catalog/mcp-servers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct InstallCatalogMcpServer {
    /// The registry name of the server, as the catalog listed it.
    pub server: String,
    /// Which of its installs to register.
    pub kind: CatalogInstallKind,
    /// The name to register it under. Omitted uses the catalog's
    /// suggestion.
    #[serde(default)]
    pub name: Option<String>,
    /// The inputs the install asked for, by key.
    #[serde(default)]
    pub values: BTreeMap<String, String>,
}
