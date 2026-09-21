//! The MCP catalog: the official MCP Registry, read for the picker.
//!
//! `registry.modelcontextprotocol.io` lists every published MCP server —
//! thirty-four thousand at the time of writing — and the picker on
//! Settings → Tools is a search over it. The registry's own read API is
//! what answers: one page per request, at most thirty entries, matched on
//! the name. That is one bounded HTTP call, not a computed catalog, which
//! is why it is made on the request path rather than mirrored the way a
//! cloud account's machine catalog is ([`crate::catalog`]): a full mirror
//! would be a multi-megabyte document read on every keystroke, and the
//! registry answers a page faster than that document could be parsed.
//!
//! Each page is kept in KV for an hour under its query, so a search typed
//! twice — or the install that follows a listing — reads the store and not
//! the registry.
//!
//! # Translation happens here
//!
//! A registry entry names remotes, `npm` and `PyPI` packages, headers with
//! `{placeholders}`, environment variables and command-line arguments. The
//! picker is shown the [`CatalogMcpServer`] this module makes of that: which
//! of those flyco can run, and the [`CatalogInput`]s the user has to fill
//! before it can. The install request then carries the entry's name, the
//! chosen kind and the values, and the same code turns them into the
//! [`McpServerConfig`] that is registered — so a browser never carries the
//! registry's schema, and the config an install produces is one function of
//! the entry rather than whatever a client assembled.

use std::collections::{BTreeMap, BTreeSet};

use flyco_core::{
    CatalogInput, CatalogInstallKind, CatalogMcpInstall, CatalogMcpServer, CurrentUser, EnvEntry,
    HeaderEntry, InstallCatalogMcpServer, McpCatalogPage, McpServerConfig, McpServerView,
    UpsertMcpServer, UserId,
};
use serde::{Deserialize, Serialize};
use skyzen::extract::Query;
use skyzen::routing::{CreateRouteNode, Route, RouteNode, Routes as _};
use skyzen::utils::{Json, State};
use skyzen_services::{Db, Kv};

use crate::error::ApiError;
use crate::expiring;
use crate::mcp;
use crate::problem::Outcome;
use crate::respond::Created;

/// The registry's read endpoint.
const SERVERS_URL: &str = "https://registry.modelcontextprotocol.io/v0/servers";

/// Entries per page.
///
/// The registry allows a hundred; thirty is what a picker shows before the
/// user narrows the search, and what a page of translated entries costs to
/// keep in the store.
const PAGE_SIZE: u32 = 30;

/// How long a page stays in the store.
///
/// The registry moves on the scale of days; an hour means a search typed
/// twice and the install that follows a listing read the store, while a
/// server published this morning is in the picker by lunch.
const TTL_SECONDS: u64 = 60 * 60;

/// The longest search the registry is asked for.
///
/// A KV key holds 512 bytes and the query is part of it; nobody searches a
/// picker with a paragraph.
const MAX_SEARCH_LEN: usize = 100;

/// The longest cursor accepted.
///
/// A registry cursor is `name:version`, which the schema bounds well under
/// this; anything longer is not one of theirs.
const MAX_CURSOR_LEN: usize = 300;

/// What the picker asks for.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
pub struct McpCatalogQuery {
    /// Substring matched against the registry name. Omitted lists the
    /// registry from its first page.
    pub search: Option<String>,
    /// Cursor from a previous page's `next_cursor`.
    pub cursor: Option<String>,
}

// ── The registry's own shape ──

/// One page as the registry serves it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryPage {
    /// The entries.
    #[serde(default)]
    pub servers: Vec<RegistryEntry>,
    /// Paging.
    #[serde(default)]
    pub metadata: RegistryMetadata,
}

/// The paging half of a registry page.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryMetadata {
    /// The cursor of the next page, absent on the last.
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// One entry: the publisher's document plus the registry's own metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    /// The publisher's `server.json`.
    pub server: RegistryServer,
}

/// A publisher's `server.json`, in the fields the translation reads.
///
/// Serde drops the rest — `$schema`, `_meta`, icons — rather than naming
/// fields nothing here looks at.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryServer {
    /// Reverse-DNS name, `io.github.owner/server`.
    pub name: String,
    /// Display name, when given.
    #[serde(default)]
    pub title: Option<String>,
    /// One-line description.
    #[serde(default)]
    pub description: String,
    /// The version this entry describes.
    #[serde(default)]
    pub version: String,
    /// Where the source lives.
    #[serde(default)]
    pub repository: Option<RegistryRepository>,
    /// The publisher's site.
    #[serde(default)]
    pub website_url: Option<String>,
    /// Endpoints the server is hosted at.
    #[serde(default)]
    pub remotes: Vec<RegistryRemote>,
    /// Packages the server is installed from.
    #[serde(default)]
    pub packages: Vec<RegistryPackage>,
}

/// The `repository` block of an entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryRepository {
    /// The repository URL.
    pub url: String,
}

/// One hosted endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryRemote {
    /// `streamable-http` or the deprecated `sse`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The endpoint.
    pub url: String,
    /// Headers every request carries.
    #[serde(default)]
    pub headers: Vec<RegistryKeyValue>,
}

/// One package.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryPackage {
    /// `npm`, `pypi`, `oci`, `nuget` or `mcpb`.
    #[serde(default)]
    pub registry_type: String,
    /// The package name in that registry.
    #[serde(default)]
    pub identifier: String,
    /// The version to install; `latest` or empty for whatever is current.
    #[serde(default)]
    pub version: String,
    /// Arguments after the package name.
    #[serde(default)]
    pub package_arguments: Vec<RegistryArgument>,
    /// Environment the process wants.
    #[serde(default)]
    pub environment_variables: Vec<RegistryKeyValue>,
}

/// A header or an environment variable: a named value the user may have to
/// supply.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryKeyValue {
    /// The header or variable name.
    pub name: String,
    /// What it is for.
    #[serde(default)]
    pub description: Option<String>,
    /// Whether the server does not work without it.
    #[serde(default)]
    pub is_required: bool,
    /// Whether it is a credential.
    #[serde(default)]
    pub is_secret: bool,
    /// A fixed value, possibly holding `{placeholders}`.
    #[serde(default)]
    pub value: Option<String>,
    /// What it is when the user leaves it blank.
    #[serde(default)]
    pub default: Option<String>,
    /// The placeholders in `value`, described.
    #[serde(default)]
    pub variables: BTreeMap<String, RegistryVariable>,
}

/// One command-line argument.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryArgument {
    /// `positional` or `named`.
    #[serde(rename = "type", default)]
    pub kind: String,
    /// The flag of a named argument, with or without its dashes.
    #[serde(default)]
    pub name: Option<String>,
    /// A fixed value, possibly holding `{placeholders}`.
    #[serde(default)]
    pub value: Option<String>,
    /// What a positional argument stands for, when it has no fixed value.
    #[serde(default)]
    pub value_hint: Option<String>,
    /// What it is when the user leaves it blank.
    #[serde(default)]
    pub default: Option<String>,
    /// What it is for.
    #[serde(default)]
    pub description: Option<String>,
    /// Whether the server does not work without it.
    #[serde(default)]
    pub is_required: bool,
    /// Whether it is a credential.
    #[serde(default)]
    pub is_secret: bool,
    /// The placeholders in `value`, described.
    #[serde(default)]
    pub variables: BTreeMap<String, RegistryVariable>,
}

/// A `{placeholder}` inside a value.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryVariable {
    /// What it is for.
    #[serde(default)]
    pub description: Option<String>,
    /// Whether the value does not work without it.
    #[serde(default)]
    pub is_required: bool,
    /// Whether it is a credential.
    #[serde(default)]
    pub is_secret: bool,
    /// What it is when the user leaves it blank.
    #[serde(default)]
    pub default: Option<String>,
}

// ── Reaching the registry ──

/// Why a page could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// The request never produced a readable answer.
    #[error("{0}")]
    Transport(String),
    /// The registry answered with a non-2xx status.
    #[error("the registry answered {status}: {reason}")]
    Status {
        /// The HTTP status it answered with.
        status: u16,
        /// What the body said, when it said anything.
        reason: String,
    },
}

/// The one call flyco makes to the registry.
///
/// Behind a trait for the same reason [`crate::github::GithubOauth`] is:
/// the picker's routes cannot be exercised without standing in for the
/// registry.
pub trait McpRegistry: Send + Sync + Clone + 'static {
    /// Reads one page, narrowed by `search` and continued from `cursor`.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] when the call fails.
    fn page(
        &self,
        search: Option<&str>,
        cursor: Option<&str>,
    ) -> impl Future<Output = Result<RegistryPage, RegistryError>> + Send;
}

/// The production [`McpRegistry`], speaking HTTP through zenwave.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, Default)]
pub struct LiveRegistry;

/// The production [`McpRegistry`], speaking HTTP through
/// `WorkerGlobalScope.fetch`.
#[cfg(target_arch = "wasm32")]
#[derive(Debug, Clone, Copy, Default)]
pub struct LiveRegistry;

/// The URL of one page.
fn page_url(search: Option<&str>, cursor: Option<&str>) -> String {
    let mut url = url::Url::parse(SERVERS_URL).expect("the registry URL is a literal");
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("version", "latest");
        query.append_pair("limit", &PAGE_SIZE.to_string());
        if let Some(search) = search {
            query.append_pair("search", search);
        }
        if let Some(cursor) = cursor {
            query.append_pair("cursor", cursor);
        }
    }
    url.into()
}

fn transport(error: impl core::fmt::Display) -> RegistryError {
    RegistryError::Transport(error.to_string())
}

/// The first non-empty line of a non-2xx body.
fn refusal_reason(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no body")
        .to_owned()
}

/// The refusal a non-2xx answer becomes.
fn refused(status: u16, body: &str) -> RegistryError {
    RegistryError::Status {
        status,
        reason: refusal_reason(body),
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl McpRegistry for LiveRegistry {
    async fn page(
        &self,
        search: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<RegistryPage, RegistryError> {
        use zenwave::{Client as _, ResponseExt as _};

        let mut client = zenwave::client();
        let response = client
            .get(page_url(search, cursor))
            .map_err(transport)?
            .header("Accept", "application/json")
            .map_err(transport)?
            .header("User-Agent", crate::github::USER_AGENT)
            .map_err(transport)?
            .await
            .map_err(transport)?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let body = response.into_string().await.map_err(transport)?;
            return Err(refused(status, &body));
        }
        response
            .into_json::<RegistryPage>()
            .await
            .map_err(transport)
    }
}

#[cfg(target_arch = "wasm32")]
impl McpRegistry for LiveRegistry {
    async fn page(
        &self,
        search: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<RegistryPage, RegistryError> {
        use skyzen_cloudflare::worker::send::IntoSendFuture as _;

        let request = skyzen_cloudflare::bare_request(
            skyzen_cloudflare::worker::Method::Get,
            &page_url(search, cursor),
            &[
                ("Accept", "application/json"),
                ("User-Agent", crate::github::USER_AGENT),
            ],
            None,
        )
        .map_err(transport)?;
        let mut response = skyzen_cloudflare::worker::Fetch::Request(request)
            .send()
            .into_send()
            .await
            .map_err(transport)?;
        let status = response.status_code();
        if !(200..300).contains(&status) {
            let body = response.text().into_send().await.map_err(transport)?;
            return Err(refused(status, &body));
        }
        response
            .json::<RegistryPage>()
            .into_send()
            .await
            .map_err(transport)
    }
}

/// Which [`McpRegistry`] the routes are assembled with.
///
/// An enum rather than a trait object because [`McpRegistry`] returns
/// `impl Future`, which is not object-safe.
#[derive(Debug, Clone)]
pub enum RegistryClient {
    /// Talks to `registry.modelcontextprotocol.io`.
    Live(LiveRegistry),
    /// Answers from fixtures, for tests.
    #[cfg(test)]
    Fake(crate::testing::TestRegistry),
}

impl Default for RegistryClient {
    fn default() -> Self {
        Self::Live(LiveRegistry)
    }
}

impl McpRegistry for RegistryClient {
    async fn page(
        &self,
        search: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<RegistryPage, RegistryError> {
        match self {
            Self::Live(client) => client.page(search, cursor).await,
            #[cfg(test)]
            Self::Fake(client) => client.page(search, cursor).await,
        }
    }
}

// ── The store in front of it ──

/// The KV key one page is kept under.
fn page_key(search: Option<&str>, cursor: Option<&str>) -> String {
    format!(
        "mcp-catalog:{}|{}",
        search.unwrap_or_default(),
        cursor.unwrap_or_default()
    )
}

/// A query the registry can be asked, or why not.
fn checked_query(query: &McpCatalogQuery) -> Result<(Option<&str>, Option<&str>), ApiError> {
    let search = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|search| !search.is_empty());
    if search.is_some_and(|search| search.len() > MAX_SEARCH_LEN) {
        return Err(ApiError::InvalidCatalogQuery(
            "a search may hold at most 100 bytes",
        ));
    }
    let cursor = query
        .cursor
        .as_deref()
        .map(str::trim)
        .filter(|cursor| !cursor.is_empty());
    if cursor.is_some_and(|cursor| cursor.len() > MAX_CURSOR_LEN) {
        return Err(ApiError::InvalidCatalogQuery(
            "the cursor is not one the registry issued",
        ));
    }
    Ok((search, cursor))
}

/// Reads one page, from the store when it is there and from the registry
/// otherwise.
async fn cached_page(
    kv: &Kv,
    registry: &impl McpRegistry,
    search: Option<&str>,
    cursor: Option<&str>,
) -> Result<RegistryPage, ApiError> {
    let key = page_key(search, cursor);
    if let Some(page) = expiring::get::<RegistryPage>(kv, &key).await? {
        return Ok(page);
    }
    let page = registry.page(search, cursor).await?;
    expiring::put(kv, &key, &page, TTL_SECONDS).await?;
    Ok(page)
}

// ── Translation ──

/// A value in a config template: what goes into the registered config once
/// the user's inputs are known.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ValueTemplate {
    /// Spelled out by the publisher.
    Fixed(String),
    /// The publisher's text with `{placeholders}` the user fills.
    Text(String),
    /// Typed whole by the user.
    Input {
        /// The input's key.
        key: String,
        /// Whether the value may be left out altogether.
        required: bool,
    },
}

/// One header or environment entry of a template.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NamedTemplate {
    name: String,
    value: ValueTemplate,
}

/// One command-line argument of a template.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ArgTemplate {
    /// The flag of a named argument, dashes included.
    flag: Option<String>,
    /// The value after it, or the positional value.
    value: Option<ValueTemplate>,
}

/// A config with holes where the user's inputs go.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfigTemplate {
    Http {
        url: String,
        headers: Vec<NamedTemplate>,
    },
    Stdio {
        command: &'static str,
        args: Vec<ArgTemplate>,
        env: Vec<NamedTemplate>,
    },
}

/// One install: what the picker is shown and what the install fills.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Install {
    kind: CatalogInstallKind,
    label: String,
    template: ConfigTemplate,
    inputs: Vec<CatalogInput>,
}

impl Install {
    fn view(&self) -> CatalogMcpInstall {
        CatalogMcpInstall {
            kind: self.kind,
            label: self.label.clone(),
            inputs: self.inputs.clone(),
        }
    }
}

/// Collects the inputs an install needs, once each.
#[derive(Debug, Default)]
struct Inputs {
    seen: BTreeSet<String>,
    list: Vec<CatalogInput>,
}

impl Inputs {
    fn add(&mut self, input: CatalogInput) {
        if self.seen.insert(input.key.clone()) {
            self.list.push(input);
        }
    }

    /// Every `{placeholder}` in `text`, as an input each.
    ///
    /// A placeholder the publisher did not describe in `variables` takes
    /// after the value it sits in: a secret header's placeholder is the
    /// secret, and a required header's placeholder is what makes it so.
    fn add_placeholders(
        &mut self,
        text: &str,
        variables: &BTreeMap<String, RegistryVariable>,
        within: Within,
    ) {
        for variable in placeholders(text) {
            let described = variables.get(variable).cloned().unwrap_or_default();
            self.add(CatalogInput {
                key: format!("var:{variable}"),
                label: variable.to_owned(),
                description: described.description,
                // A placeholder with no default has to be filled: the text
                // around it is meaningless without it.
                required: described.is_required || within.required || described.default.is_none(),
                secret: described.is_secret || within.secret,
                default: described.default,
            });
        }
    }
}

/// The flags of the value a placeholder sits in.
#[derive(Debug, Clone, Copy)]
struct Within {
    required: bool,
    secret: bool,
}

/// The `{placeholders}` in a value, in order, once each.
fn placeholders(text: &str) -> Vec<&str> {
    let mut found: Vec<&str> = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else { break };
        let name = &after[..close];
        if !name.is_empty() && !found.contains(&name) {
            found.push(name);
        }
        rest = &after[close + 1..];
    }
    found
}

/// A header or environment entry as a template, filing its inputs.
fn named_template(entry: &RegistryKeyValue, prefix: &str, inputs: &mut Inputs) -> NamedTemplate {
    let value = match entry.value.as_deref().filter(|value| !value.is_empty()) {
        Some(value) if placeholders(value).is_empty() => ValueTemplate::Fixed(value.to_owned()),
        Some(value) => {
            inputs.add_placeholders(
                value,
                &entry.variables,
                Within {
                    required: entry.is_required,
                    secret: entry.is_secret,
                },
            );
            ValueTemplate::Text(value.to_owned())
        }
        None => {
            let key = format!("{prefix}:{}", entry.name);
            inputs.add(CatalogInput {
                key: key.clone(),
                label: entry.name.clone(),
                description: entry.description.clone(),
                required: entry.is_required,
                secret: entry.is_secret,
                default: entry.default.clone(),
            });
            ValueTemplate::Input {
                key,
                required: entry.is_required,
            }
        }
    };
    NamedTemplate {
        name: entry.name.clone(),
        value,
    }
}

/// A named argument's flag, dashes included.
fn flag_of(name: &str) -> String {
    if name.starts_with('-') {
        name.to_owned()
    } else {
        format!("--{name}")
    }
}

/// One package argument as a template, filing its inputs.
fn arg_template(index: usize, argument: &RegistryArgument, inputs: &mut Inputs) -> ArgTemplate {
    let named = argument.kind == "named";
    let flag = if named {
        argument.name.as_deref().map(flag_of)
    } else {
        None
    };
    let value = match argument.value.as_deref().filter(|value| !value.is_empty()) {
        Some(value) if placeholders(value).is_empty() => {
            Some(ValueTemplate::Fixed(value.to_owned()))
        }
        Some(value) => {
            inputs.add_placeholders(
                value,
                &argument.variables,
                Within {
                    required: argument.is_required,
                    secret: argument.is_secret,
                },
            );
            Some(ValueTemplate::Text(value.to_owned()))
        }
        // A named flag with a fixed default and nothing to fill is a
        // switch; a positional argument always stands for a value.
        None if named && argument.default.is_none() && !argument.is_required => None,
        None => {
            let label = argument
                .name
                .clone()
                .or_else(|| argument.value_hint.clone())
                .unwrap_or_else(|| format!("argument {}", index + 1));
            let key = format!("arg:{label}");
            inputs.add(CatalogInput {
                key: key.clone(),
                label,
                description: argument.description.clone(),
                required: argument.is_required,
                secret: argument.is_secret,
                default: argument.default.clone(),
            });
            Some(ValueTemplate::Input {
                key,
                required: argument.is_required,
            })
        }
    };
    ArgTemplate { flag, value }
}

/// The package spec `npx` or `uvx` is handed: the identifier, pinned to the
/// entry's version when it names one.
fn package_spec(package: &RegistryPackage) -> String {
    match package.version.trim() {
        "" | "latest" => package.identifier.clone(),
        version => format!("{}@{version}", package.identifier),
    }
}

/// The install a hosted endpoint offers, if flyco can reach it.
fn remote_install(remote: &RegistryRemote) -> Option<Install> {
    if remote.kind != "streamable-http" {
        return None;
    }
    let host = url::Url::parse(&remote.url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))?;
    let mut inputs = Inputs::default();
    let headers = remote
        .headers
        .iter()
        .map(|header| named_template(header, "header", &mut inputs))
        .collect();
    Some(Install {
        kind: CatalogInstallKind::Remote,
        label: format!("Remote · {host}"),
        template: ConfigTemplate::Http {
            url: remote.url.clone(),
            headers,
        },
        inputs: inputs.list,
    })
}

/// The install a package offers, if a session machine can run it.
fn package_install(package: &RegistryPackage) -> Option<Install> {
    let (kind, command, mut args) = match package.registry_type.as_str() {
        "npm" => (
            CatalogInstallKind::Npm,
            "npx",
            vec![ArgTemplate {
                flag: None,
                value: Some(ValueTemplate::Fixed("-y".to_owned())),
            }],
        ),
        "pypi" => (CatalogInstallKind::Pypi, "uvx", Vec::new()),
        _ => return None,
    };
    if package.identifier.trim().is_empty() {
        return None;
    }
    args.push(ArgTemplate {
        flag: None,
        value: Some(ValueTemplate::Fixed(package_spec(package))),
    });
    let mut inputs = Inputs::default();
    args.extend(
        package
            .package_arguments
            .iter()
            .enumerate()
            .map(|(index, argument)| arg_template(index, argument, &mut inputs)),
    );
    let env = package
        .environment_variables
        .iter()
        .map(|variable| named_template(variable, "env", &mut inputs))
        .collect();
    Some(Install {
        kind,
        label: format!("{command} {}", package.identifier),
        template: ConfigTemplate::Stdio { command, args, env },
        inputs: inputs.list,
    })
}

/// The name a server is registered under unless the user picks another:
/// the tail of the registry name, in the characters a harness can announce
/// a server as.
fn suggested_name(registry_name: &str) -> String {
    let tail = registry_name
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or(registry_name);
    let cleaned: String = tail
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "mcp-server".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// A registry entry with its installs worked out.
#[derive(Debug, Clone)]
struct Translated {
    view: CatalogMcpServer,
    installs: Vec<Install>,
}

/// Translates one entry, or `None` when flyco can run none of it.
///
/// Installs come remote first: a hosted endpoint needs nothing on the
/// machine. One per kind, the publisher's first, so an entry listing the
/// same package three ways is one install rather than three.
fn translate(server: &RegistryServer) -> Option<Translated> {
    let mut installs: Vec<Install> = Vec::new();
    let candidates = server
        .remotes
        .iter()
        .filter_map(remote_install)
        .chain(server.packages.iter().filter_map(package_install));
    for install in candidates {
        if !installs.iter().any(|known| known.kind == install.kind) {
            installs.push(install);
        }
    }
    if installs.is_empty() {
        return None;
    }
    installs.sort_by_key(|install| match install.kind {
        CatalogInstallKind::Remote => 0,
        CatalogInstallKind::Npm => 1,
        CatalogInstallKind::Pypi => 2,
    });
    let view = CatalogMcpServer {
        name: server.name.clone(),
        title: server
            .title
            .clone()
            .filter(|title| !title.trim().is_empty()),
        description: server.description.clone(),
        version: server.version.clone(),
        repository_url: server.repository.as_ref().map(|repo| repo.url.clone()),
        website_url: server.website_url.clone(),
        suggested_name: suggested_name(&server.name),
        installs: installs.iter().map(Install::view).collect(),
    };
    Some(Translated { view, installs })
}

/// A page of the catalog: every entry flyco can run, translated.
fn translate_page(page: &RegistryPage) -> McpCatalogPage {
    McpCatalogPage {
        servers: page
            .servers
            .iter()
            .filter_map(|entry| translate(&entry.server))
            .map(|translated| translated.view)
            .collect(),
        next_cursor: page.metadata.next_cursor.clone(),
    }
}

// ── Filling a template ──

/// The user's answers, looked up by key.
struct Values<'a> {
    values: &'a BTreeMap<String, String>,
    inputs: &'a [CatalogInput],
}

impl Values<'_> {
    /// The value for one input: what the user typed, else the default.
    ///
    /// A blank answer counts as none: a required field left empty is
    /// missing, and an optional one left empty is omitted rather than sent
    /// as an empty string.
    fn get(&self, key: &str) -> Option<String> {
        let typed = self
            .values
            .get(key)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        typed.or_else(|| {
            self.inputs
                .iter()
                .find(|input| input.key == key)
                .and_then(|input| input.default.clone())
        })
    }

    /// The value for an input that has to be there.
    fn require(&self, key: &str) -> Result<String, ApiError> {
        self.get(key).ok_or_else(|| ApiError::CatalogInputMissing {
            key: key.to_owned(),
        })
    }

    /// Fills the `{placeholders}` of a text.
    fn fill(&self, text: &str) -> Result<String, ApiError> {
        let mut filled = text.to_owned();
        for variable in placeholders(text) {
            let value = self.require(&format!("var:{variable}"))?;
            filled = filled.replace(&format!("{{{variable}}}"), &value);
        }
        Ok(filled)
    }

    /// One template value, or `None` for an optional input left blank.
    fn resolve(&self, template: &ValueTemplate) -> Result<Option<String>, ApiError> {
        Ok(match template {
            ValueTemplate::Fixed(value) => Some(value.clone()),
            ValueTemplate::Text(text) => Some(self.fill(text)?),
            ValueTemplate::Input { key, required } => {
                if *required {
                    Some(self.require(key)?)
                } else {
                    self.get(key)
                }
            }
        })
    }

    /// Refuses a key the install never asked for: a value with nowhere to
    /// go is a request built against a different entry.
    fn check_known(&self) -> Result<(), ApiError> {
        for key in self.values.keys() {
            if !self.inputs.iter().any(|input| &input.key == key) {
                return Err(ApiError::CatalogInputUnknown { key: key.clone() });
            }
        }
        Ok(())
    }
}

/// The config an install produces from the user's answers.
fn fill(install: &Install, values: &BTreeMap<String, String>) -> Result<McpServerConfig, ApiError> {
    let values = Values {
        values,
        inputs: &install.inputs,
    };
    values.check_known()?;
    Ok(match &install.template {
        ConfigTemplate::Http { url, headers } => {
            let mut filled = Vec::new();
            for header in headers {
                if let Some(value) = values.resolve(&header.value)? {
                    filled.push(HeaderEntry {
                        name: header.name.clone(),
                        value,
                    });
                }
            }
            McpServerConfig::Http {
                url: url.clone(),
                headers: filled,
            }
        }
        ConfigTemplate::Stdio { command, args, env } => {
            let mut filled_args = Vec::new();
            for arg in args {
                let value = match &arg.value {
                    Some(template) => values.resolve(template)?,
                    None => None,
                };
                match (&arg.flag, value) {
                    (Some(flag), Some(value)) => {
                        filled_args.push(flag.clone());
                        filled_args.push(value);
                    }
                    // A switch: the flag alone.
                    (Some(flag), None) if arg.value.is_none() => filled_args.push(flag.clone()),
                    (None, Some(value)) => filled_args.push(value),
                    // An optional value left blank is left out, and a
                    // named one takes its flag with it.
                    (_, None) => {}
                }
            }
            let mut filled_env = Vec::new();
            for entry in env {
                if let Some(value) = values.resolve(&entry.value)? {
                    filled_env.push(EnvEntry {
                        key: entry.name.clone(),
                        value,
                    });
                }
            }
            McpServerConfig::Stdio {
                command: (*command).to_owned(),
                args: filled_args,
                env: filled_env,
            }
        }
    })
}

// ── Routes ──

/// Lists one page of the catalog.
#[skyzen::openapi]
async fn list_catalog_mcp_servers(
    State(_user): State<CurrentUser>,
    State(registry): State<RegistryClient>,
    Query(query): Query<McpCatalogQuery>,
    kv: Kv,
) -> Outcome<Json<McpCatalogPage>> {
    list(&kv, &registry, &query).await.map(Json).into()
}

async fn list(
    kv: &Kv,
    registry: &impl McpRegistry,
    query: &McpCatalogQuery,
) -> Result<McpCatalogPage, ApiError> {
    let (search, cursor) = checked_query(query)?;
    let page = cached_page(kv, registry, search, cursor).await?;
    Ok(translate_page(&page))
}

/// Registers a catalog server with the user's answers filled in.
#[skyzen::openapi]
async fn install_catalog_mcp_server(
    State(user): State<CurrentUser>,
    State(registry): State<RegistryClient>,
    Json(request): Json<InstallCatalogMcpServer>,
    kv: Kv,
    db: Db,
) -> Outcome<Created<Json<McpServerView>>> {
    install(&db, &kv, &registry, user.id, request)
        .await
        .map(|view| Created(Json(view)))
        .into()
}

/// Finds the entry again, fills the chosen install and registers it.
///
/// The entry is read back from the registry rather than trusted from the
/// request — the request names it, and the page a search for that name
/// returns is the same page the listing cached — so what gets registered is
/// what the registry says, filled with what the user typed, and nothing a
/// client assembled.
async fn install(
    db: &Db,
    kv: &Kv,
    registry: &impl McpRegistry,
    user: UserId,
    request: InstallCatalogMcpServer,
) -> Result<McpServerView, ApiError> {
    let name = request.server.trim();
    if name.is_empty() || name.len() > MAX_SEARCH_LEN {
        return Err(ApiError::InvalidCatalogQuery(
            "the server name is not one the registry lists",
        ));
    }
    let page = cached_page(kv, registry, Some(name), None).await?;
    let translated = page
        .servers
        .iter()
        .find(|entry| entry.server.name == name)
        .and_then(|entry| translate(&entry.server))
        .ok_or_else(|| ApiError::CatalogServerNotFound {
            name: name.to_owned(),
        })?;
    let chosen = translated
        .installs
        .iter()
        .find(|install| install.kind == request.kind)
        .ok_or_else(|| ApiError::CatalogInstallUnavailable {
            name: name.to_owned(),
            kind: request.kind.as_str(),
        })?;
    let config = fill(chosen, &request.values)?;
    let registered = UpsertMcpServer {
        name: request
            .name
            .filter(|chosen| !chosen.trim().is_empty())
            .unwrap_or(translated.view.suggested_name),
        config,
        enabled: true,
    };
    let view = mcp::register(db, user, registered).await?;
    tracing::info!(server = %name, kind = request.kind.as_str(), registered = %view.name, "added an MCP server from the catalog");
    Ok(view)
}

/// The user-scoped catalog routes.
pub fn routes() -> Vec<RouteNode> {
    Route::new(("/v1/catalog/mcp-servers"
        .at(list_catalog_mcp_servers)
        .post(install_catalog_mcp_server),))
    .into_route_nodes()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use flyco_core::{CatalogInstallKind, McpServerConfig};

    use super::{
        McpCatalogQuery, RegistryPage, checked_query, fill, page_key, page_url, placeholders,
        suggested_name, translate, translate_page,
    };

    /// A real page, captured from the registry on 2026-09-20.
    fn github_page() -> RegistryPage {
        serde_json::from_str(include_str!("../fixtures/mcp-registry/search-github.json"))
            .expect("the fixture is a registry page")
    }

    fn filesystem_page() -> RegistryPage {
        serde_json::from_str(include_str!(
            "../fixtures/mcp-registry/search-filesystem.json"
        ))
        .expect("the fixture is a registry page")
    }

    #[test]
    fn placeholders_are_found_once_each_in_order() {
        assert_eq!(placeholders("Bearer {token}"), vec!["token"]);
        assert_eq!(placeholders("{a}/{b}/{a}"), vec!["a", "b"]);
        assert!(placeholders("no holes {").is_empty());
    }

    #[test]
    fn a_suggested_name_is_the_tail_in_harness_characters() {
        assert_eq!(suggested_name("io.github.owner/server-name"), "server-name");
        assert_eq!(suggested_name("com.example/my.server"), "my-server");
        assert_eq!(suggested_name("weird/"), "weird");
    }

    #[test]
    fn a_page_url_carries_the_query() {
        assert_eq!(
            page_url(Some("git hub"), Some("a:1")),
            "https://registry.modelcontextprotocol.io/v0/servers?version=latest&limit=30&search=git+hub&cursor=a%3A1"
        );
        assert_eq!(page_key(None, None), "mcp-catalog:|");
    }

    #[test]
    fn a_query_is_trimmed_and_bounded() {
        let query = McpCatalogQuery {
            search: Some("  github ".to_owned()),
            cursor: Some(String::new()),
        };
        assert_eq!(
            checked_query(&query).expect("valid"),
            (Some("github"), None)
        );
        let long = McpCatalogQuery {
            search: Some("x".repeat(101)),
            cursor: None,
        };
        assert!(checked_query(&long).is_err());
    }

    #[test]
    fn a_page_keeps_only_what_a_machine_can_run() {
        let page = translate_page(&github_page());
        let names: Vec<&str> = page.servers.iter().map(|s| s.name.as_str()).collect();
        // Six entries in the fixture: one offers neither a remote nor a
        // runnable package and is gone; the one that is `PyPI` plus a Docker
        // image is here once, as `PyPI`.
        assert_eq!(names.len(), 5, "{names:?}");
        assert!(!names.contains(&"io.github.0spoon/seamless"));
        let armory = page
            .servers
            .iter()
            .find(|s| s.name == "com.mcparmory/github")
            .expect("listed");
        assert_eq!(
            armory.installs.iter().map(|i| i.kind).collect::<Vec<_>>(),
            vec![CatalogInstallKind::Pypi]
        );
        let both = page
            .servers
            .iter()
            .find(|s| s.name == "io.github.0nork/0nMCP")
            .expect("listed");
        assert_eq!(
            both.installs.iter().map(|i| i.kind).collect::<Vec<_>>(),
            vec![CatalogInstallKind::Remote, CatalogInstallKind::Npm]
        );
        assert_eq!(both.suggested_name, "0nMCP");
        assert_eq!(
            page.next_cursor.as_deref(),
            github_page().metadata.next_cursor.as_deref()
        );
    }

    #[test]
    fn a_remote_with_a_placeholder_header_asks_for_the_variable() {
        let page = github_page();
        let entry = &page
            .servers
            .iter()
            .find(|e| e.server.name == "ai.smithery/Hint-Services-obsidian-github-mcp")
            .expect("listed")
            .server;
        let translated = translate(entry).expect("runnable");
        let install = &translated.installs[0];
        assert_eq!(install.kind, CatalogInstallKind::Remote);
        assert_eq!(install.label, "Remote · server.smithery.ai");
        assert_eq!(install.inputs.len(), 1);
        let input = &install.inputs[0];
        assert_eq!(input.key, "var:smithery_api_key");
        assert!(input.required);
        assert!(input.secret);

        let missing = fill(install, &BTreeMap::new()).expect_err("the key is required");
        assert!(
            matches!(missing, crate::error::ApiError::CatalogInputMissing { ref key } if key == "var:smithery_api_key")
        );

        let mut values = BTreeMap::new();
        values.insert("var:smithery_api_key".to_owned(), "sk-1".to_owned());
        let config = fill(install, &values).expect("filled");
        match config {
            McpServerConfig::Http { url, headers } => {
                assert_eq!(
                    url,
                    "https://server.smithery.ai/@Hint-Services/obsidian-github-mcp/mcp"
                );
                assert_eq!(headers.len(), 1);
                assert_eq!(headers[0].name, "Authorization");
                assert_eq!(headers[0].value, "Bearer sk-1");
            }
            other @ McpServerConfig::Stdio { .. } => {
                panic!("expected an http config, got {other:?}")
            }
        }

        let mut stray = values.clone();
        stray.insert("env:NOPE".to_owned(), "x".to_owned());
        assert!(matches!(
            fill(install, &stray).expect_err("unknown key"),
            crate::error::ApiError::CatalogInputUnknown { .. }
        ));
    }

    #[test]
    fn an_npm_package_runs_through_npx_with_its_named_argument() {
        let page = filesystem_page();
        let translated = translate(&page.servers[0].server).expect("runnable");
        let install = translated
            .installs
            .iter()
            .find(|i| i.kind == CatalogInstallKind::Npm)
            .expect("npm");
        assert_eq!(install.label, "npx @agent-infra/mcp-server-filesystem");
        assert_eq!(install.inputs.len(), 1);
        assert_eq!(install.inputs[0].key, "arg:allowed-directories");
        assert!(install.inputs[0].required);

        let mut values = BTreeMap::new();
        values.insert(
            "arg:allowed-directories".to_owned(),
            "/workspace".to_owned(),
        );
        match fill(install, &values).expect("filled") {
            McpServerConfig::Stdio { command, args, env } => {
                assert_eq!(command, "npx");
                assert_eq!(
                    args,
                    vec![
                        "-y",
                        "@agent-infra/mcp-server-filesystem",
                        "--allowed-directories",
                        "/workspace"
                    ]
                );
                assert!(env.is_empty());
            }
            other @ McpServerConfig::Http { .. } => {
                panic!("expected a stdio config, got {other:?}")
            }
        }
    }

    #[test]
    fn a_pypi_package_with_environment_asks_for_each_variable() {
        let page = github_page();
        let entry = &page
            .servers
            .iter()
            .find(|e| e.server.name == "io.github.06ketan/medium-ops")
            .expect("listed")
            .server;
        let translated = translate(entry).expect("runnable");
        let install = &translated.installs[0];
        assert_eq!(install.kind, CatalogInstallKind::Pypi);
        let env_inputs = install
            .inputs
            .iter()
            .filter(|input| input.key.starts_with("env:"))
            .count();
        assert_eq!(env_inputs, entry.packages[0].environment_variables.len());
        // Every required input filled with its own key makes a config
        // whose env names each variable once.
        let values: BTreeMap<String, String> = install
            .inputs
            .iter()
            .map(|input| (input.key.clone(), format!("value-of-{}", input.label)))
            .collect();
        match fill(install, &values).expect("filled") {
            McpServerConfig::Stdio { command, env, .. } => {
                assert_eq!(command, "uvx");
                assert_eq!(env.len(), env_inputs);
            }
            other @ McpServerConfig::Http { .. } => {
                panic!("expected a stdio config, got {other:?}")
            }
        }
    }
}
