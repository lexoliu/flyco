//! The response half of the exported `OpenAPI` document.
//!
//! Skyzen 0.1.2 cannot describe what a handler returns. Its
//! `openapi::maybe_schema_of::<T>()` is a plain generic function with no
//! `T: ToSchema` bound, so the autoref-specialization probe behind it can
//! never fire and it answers `None` for every type; `Json<T>::openapi()`
//! therefore reports a response whose schema is missing. The only route to
//! real response content is the branch of `#[skyzen::openapi]` that
//! *syntactically* recognises the return type, and it recognises a bare
//! `Json<T>` and nothing wrapped around one. Flyco wraps almost everything in
//! [`Outcome`](crate::problem::Outcome), because that is what renders RFC
//! 9457 problems — so the export would name a path, a request body, and
//! nothing at all about what comes back. Filed upstream as
//! [zen-rs/skyzen#18](https://github.com/zen-rs/skyzen/issues/18); this
//! module is what stands in until it is fixed.
//!
//! It is two declarations and one rule:
//!
//! * [`DECLARED`] names every annotated operation's success response —
//!   status and payload — and [`describe`] writes it into the generated
//!   document.
//! * [`UNDECLARED`] names the routed operations that carry no
//!   `#[skyzen::openapi]` annotation, each with the reason it has none.
//! * Anything else in the document is a fast failure: an operation the
//!   export has no opinion about would otherwise ship as an untyped hole in
//!   the generated TypeScript client.
//!
//! A table is only worth having if it cannot drift from the handlers, which
//! is what `crate::tests::responses` is for: it reads the crate's own
//! sources, derives each annotated handler's payload from its return type,
//! and fails if this table disagrees by so much as one entry. Changing a
//! handler's return type without changing its row here does not compile past
//! the test suite.

use utoipa::openapi::path::Operation;
use utoipa::openapi::schema::{Array, ArrayBuilder};
use utoipa::openapi::{Components, Content, OpenApi, Ref, RefOr, Response, Schema};

/// Media type every flyco success document is served as.
const JSON: &str = "application/json";

/// The document one operation answers with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Payload {
    /// One document of the named component schema.
    One(&'static str),
    /// An array of the named component schema.
    Many(&'static str),
}

impl Payload {
    /// The component schema this payload is built from.
    #[must_use]
    pub const fn schema_name(self) -> &'static str {
        match self {
            Self::One(name) | Self::Many(name) => name,
        }
    }

    /// The response schema, as the document expresses it.
    fn schema(self) -> RefOr<Schema> {
        let reference = Ref::from_schema_name(self.schema_name());
        match self {
            Self::One(_) => reference.into(),
            Self::Many(_) => array_of(reference).into(),
        }
    }
}

/// Builds the array schema of a `Vec<T>` response.
fn array_of(items: Ref) -> Array {
    ArrayBuilder::new().items(items).build()
}

/// What one operation answers on success.
///
/// The status is part of the declaration rather than left at the `200` the
/// generator assumes, because most of these are not `200`: a creation is
/// `201`, a hand-off to a daemon or a provisioner is `202`, and a delete is
/// `204`. A client generated from a document that called all of them `200`
/// would treat a perfectly good `204` as an unexpected status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Success {
    /// `200 OK`, carrying a document.
    Ok(Payload),
    /// `201 Created`, carrying the resource that was created.
    Created(Payload),
    /// `202 Accepted`: the request was recorded and the outcome arrives on
    /// the relay or in a later read, not in this response.
    Accepted,
    /// `204 No Content`.
    NoContent,
    /// `303 See Other`: the browser is handed on to the SPA.
    SeeOther,
}

impl Success {
    /// The status code this response is declared under.
    #[must_use]
    pub const fn status(self) -> u16 {
        match self {
            Self::Ok(_) => 200,
            Self::Created(_) => 201,
            Self::Accepted => 202,
            Self::NoContent => 204,
            Self::SeeOther => 303,
        }
    }

    /// The document this response carries, if it carries one.
    #[must_use]
    pub const fn payload(self) -> Option<Payload> {
        match self {
            Self::Ok(payload) | Self::Created(payload) => Some(payload),
            Self::Accepted | Self::NoContent | Self::SeeOther => None,
        }
    }

    /// Prose for the status line, which `OpenAPI` requires on every response.
    const fn description(self) -> &'static str {
        match self {
            Self::Ok(_) => "Success.",
            Self::Created(_) => "Created.",
            Self::Accepted => "Accepted; the outcome arrives out of band.",
            Self::NoContent => "Done; there is nothing to return.",
            Self::SeeOther => "Redirected back to the application.",
        }
    }
}

/// Every `#[skyzen::openapi]`-annotated operation's success response, by the
/// operation id the export emits (`flyco_api::<module>::<function>`).
///
/// Sorted by operation id, which is the order the generated document and the
/// drift guard both read them in, so a diff here is one line per handler.
pub const DECLARED: &[(&str, Success)] = &[
    (
        "flyco_api::agents_md::get_agents_md",
        Success::Ok(Payload::One("AgentsDocument")),
    ),
    (
        "flyco_api::agents_md::put_agents_md",
        Success::Ok(Payload::One("AgentsDocument")),
    ),
    (
        "flyco_api::app::archive_session",
        Success::Ok(Payload::One("SessionDetail")),
    ),
    (
        "flyco_api::app::create_api_key",
        Success::Ok(Payload::One("CreatedApiKey")),
    ),
    (
        "flyco_api::app::create_daemon_token",
        Success::Ok(Payload::One("DaemonToken")),
    ),
    (
        "flyco_api::app::create_relay_ticket",
        Success::Ok(Payload::One("RelayTicket")),
    ),
    (
        "flyco_api::app::create_session",
        Success::Created(Payload::One("SessionDetail")),
    ),
    (
        "flyco_api::app::decide_approval",
        Success::Ok(Payload::One("ApprovalView")),
    ),
    (
        "flyco_api::app::get_repo_status",
        Success::Ok(Payload::One("RepoStatus")),
    ),
    (
        "flyco_api::app::get_session",
        Success::Ok(Payload::One("SessionDetail")),
    ),
    (
        "flyco_api::app::get_session_budget",
        Success::Ok(Payload::One("BudgetView")),
    ),
    (
        "flyco_api::app::get_session_env",
        Success::Ok(Payload::One("EnvDocument")),
    ),
    (
        "flyco_api::app::get_session_events",
        Success::Ok(Payload::One("EventPage")),
    ),
    (
        "flyco_api::app::healthz",
        Success::Ok(Payload::One("Health")),
    ),
    ("flyco_api::app::interrupt_session", Success::Accepted),
    (
        "flyco_api::app::list_api_keys",
        Success::Ok(Payload::Many("ApiKeySummary")),
    ),
    (
        "flyco_api::app::list_approvals",
        Success::Ok(Payload::Many("ApprovalView")),
    ),
    (
        "flyco_api::app::list_sessions",
        Success::Ok(Payload::Many("SessionSummary")),
    ),
    (
        "flyco_api::app::list_turns",
        Success::Ok(Payload::One("TurnPage")),
    ),
    (
        "flyco_api::app::me",
        Success::Ok(Payload::One("CurrentUser")),
    ),
    (
        "flyco_api::app::put_session_env",
        Success::Ok(Payload::One("EnvDocument")),
    ),
    (
        "flyco_api::app::raise_approval",
        Success::Created(Payload::One("ApprovalView")),
    ),
    (
        "flyco_api::app::resume_session",
        Success::Ok(Payload::One("SessionDetail")),
    ),
    ("flyco_api::app::revoke_api_key", Success::NoContent),
    ("flyco_api::app::send_message", Success::Accepted),
    (
        "flyco_api::app::update_me",
        Success::Ok(Payload::One("CurrentUser")),
    ),
    (
        "flyco_api::harness_accounts::complete_harness_link",
        Success::SeeOther,
    ),
    (
        "flyco_api::harness_accounts::list_harness_accounts",
        Success::Ok(Payload::Many("HarnessAccountView")),
    ),
    (
        "flyco_api::harness_accounts::llm_usage",
        Success::Ok(Payload::Many("LlmUsageView")),
    ),
    (
        "flyco_api::harness_accounts::start_harness_link",
        Success::Ok(Payload::One("AuthorizeUrl")),
    ),
    (
        "flyco_api::harness_accounts::unlink_harness_account",
        Success::NoContent,
    ),
    (
        "flyco_api::machines::get_catalog",
        Success::Ok(Payload::Many("MachineCatalogEntry")),
    ),
    (
        "flyco_api::machines::get_session_machine",
        Success::Ok(Payload::One("MachineView")),
    ),
    (
        "flyco_api::machines::resize_session_machine",
        Success::Accepted,
    ),
    (
        "flyco_api::machines::start_session_machine",
        Success::Accepted,
    ),
    (
        "flyco_api::machines::stop_session_machine",
        Success::Accepted,
    ),
    ("flyco_api::mcp::delete_mcp_server", Success::NoContent),
    (
        "flyco_api::mcp::get_mcp_server",
        Success::Ok(Payload::One("McpServerView")),
    ),
    (
        "flyco_api::mcp::list_mcp_servers",
        Success::Ok(Payload::Many("McpServerView")),
    ),
    (
        "flyco_api::mcp::register_mcp_server",
        Success::Created(Payload::One("McpServerView")),
    ),
    (
        "flyco_api::mcp::update_mcp_server",
        Success::Ok(Payload::One("McpServerView")),
    ),
    (
        "flyco_api::memory::create_memory_node",
        Success::Created(Payload::One("MemoryNode")),
    ),
    ("flyco_api::memory::delete_memory_node", Success::NoContent),
    (
        "flyco_api::memory::get_memory_node",
        Success::Ok(Payload::One("MemoryNode")),
    ),
    (
        "flyco_api::memory::list_memory",
        Success::Ok(Payload::Many("MemoryNode")),
    ),
    (
        "flyco_api::memory::update_memory_node",
        Success::Ok(Payload::One("MemoryNode")),
    ),
    (
        "flyco_api::oauth::start",
        Success::Ok(Payload::One("AuthorizeUrl")),
    ),
    (
        "flyco_api::provider_accounts::cloud_usage",
        Success::Ok(Payload::Many("CloudUsageView")),
    ),
    (
        "flyco_api::provider_accounts::link_provider",
        Success::Created(Payload::One("ProviderAccountView")),
    ),
    (
        "flyco_api::provider_accounts::list_providers",
        Success::Ok(Payload::Many("ProviderAccountView")),
    ),
    (
        "flyco_api::provider_accounts::provider_quickstart",
        Success::Ok(Payload::Many("ProviderBonusHint")),
    ),
    (
        "flyco_api::provider_accounts::unlink_provider",
        Success::NoContent,
    ),
    (
        "flyco_api::push::subscribe_push",
        Success::Created(Payload::One("PushSubscriptionView")),
    ),
    ("flyco_api::push::unsubscribe_push", Success::NoContent),
    (
        "flyco_api::push::vapid_public_key",
        Success::Ok(Payload::One("VapidPublicKey")),
    ),
    (
        "flyco_api::repos::list_repos",
        Success::Ok(Payload::Many("RepoSummary")),
    ),
    ("flyco_api::skills::delete_skill", Success::NoContent),
    (
        "flyco_api::skills::get_skill",
        Success::Ok(Payload::One("SkillView")),
    ),
    (
        "flyco_api::skills::list_skills",
        Success::Ok(Payload::Many("SkillView")),
    ),
    (
        "flyco_api::skills::upload_skill",
        Success::Created(Payload::One("SkillView")),
    ),
    (
        "flyco_api::webhooks::receive_github_webhook",
        Success::NoContent,
    ),
];

/// The routed operations that carry no `#[skyzen::openapi]` annotation, and
/// therefore no declaration here.
///
/// Skyzen exports an operation for every route, annotated or not, so these
/// appear in the document as a path and a bare `200`. Each is left that way
/// for a reason rather than by omission:
///
/// * `app::open_daemon_relay`, `app::open_client_relay` — the success of a
///   relay route is a `101` with a WebSocket attached, which the response
///   model has no way to describe.
/// * `app::put_transcript_batch`, `app::get_transcript` — daemon-scoped
///   routes whose bodies are raw bytes: a transcript batch is
///   newline-delimited JSON, not a document the response model can name.
///   (`app::raise_approval` is daemon-scoped too but answers an ordinary
///   `ApprovalView`, so it is annotated and declared like the rest.)
/// * `oauth::callback` — generic over the GitHub client, and
///   `#[skyzen::openapi]` cannot be applied to a generic handler: the macro
///   emits module-level items naming every argument type.
///
/// The id of the OAuth callback carries the type argument it was
/// monomorphized with, which is why it does not read like the others.
pub const UNDECLARED: &[&str] = &[
    "app::get_transcript",
    "app::open_client_relay",
    "app::open_daemon_relay",
    "app::put_transcript_batch",
    "oauth::callback<flyco_api::github::ZenwaveGithub>",
];

/// Writes flyco's response contract into a generated document.
///
/// # Panics
///
/// Panics if the document holds an operation that is in neither [`DECLARED`]
/// nor [`UNDECLARED`], or one with no operation id at all. Both mean a route
/// was added without saying what it answers, and a document that quietly
/// omitted it would ship as an untyped hole in the generated client.
pub fn describe(spec: &mut OpenApi) {
    register_schemas(spec);

    for (path, item) in &mut spec.paths.paths {
        for operation in operations(item) {
            let id = operation
                .operation_id
                .clone()
                .unwrap_or_else(|| panic!("`{path}` exports an operation with no operation id"));

            let Some(success) = declared(&id) else {
                assert!(
                    UNDECLARED.contains(&id.as_str()),
                    "`{id}` declares no success response; add it to `DECLARED`, or to \
                     `UNDECLARED` with the reason it cannot be described"
                );
                continue;
            };

            attach(operation, success);
        }
    }
}

/// The declaration for one operation id.
fn declared(id: &str) -> Option<Success> {
    DECLARED
        .iter()
        .find(|(declared, _)| *declared == id)
        .map(|(_, success)| *success)
}

/// Every operation a path item holds.
///
/// `PathItem` is eight optional fields rather than a map, so the verbs are
/// listed once here instead of at each call site.
fn operations(item: &mut utoipa::openapi::path::PathItem) -> impl Iterator<Item = &mut Operation> {
    [
        item.get.as_mut(),
        item.put.as_mut(),
        item.post.as_mut(),
        item.delete.as_mut(),
        item.options.as_mut(),
        item.head.as_mut(),
        item.patch.as_mut(),
        item.trace.as_mut(),
    ]
    .into_iter()
    .flatten()
}

/// Replaces an operation's success response with the declared one.
///
/// The generator always emits exactly one response, keyed `200`, so the
/// declared status replaces that key rather than being added beside it: two
/// success responses would be two things a client has to handle, one of
/// which never happens.
fn attach(operation: &mut Operation, success: Success) {
    let mut response = Response::new(success.description());
    if let Some(payload) = success.payload() {
        response
            .content
            .insert(JSON.to_owned(), Content::new(Some(payload.schema())));
    }

    operation.responses.responses.clear();
    operation
        .responses
        .responses
        .insert(success.status().to_string(), RefOr::T(response));
}

/// Adds `T` — and everything `T` refers to — to a document's components.
///
/// `utoipa`'s derive walks a type's own references, so registering the
/// outermost DTO of a response is enough to bring the whole tree with it.
fn register<T: utoipa::ToSchema>(components: &mut Components) {
    let mut collected = vec![(T::name().into_owned(), T::schema())];
    T::schemas(&mut collected);
    components.schemas.extend(collected);
}

/// Registers every DTO the control plane *returns*.
///
/// Request bodies and query strings reach the document on their own, through
/// the `#[skyzen::openapi]` annotation on each handler. Responses do not —
/// see this module's own documentation — so every schema [`DECLARED`] refers
/// to has to be put in `components.schemas` by hand. A type listed here that
/// no route returns is dead weight in the generated client, which is why the
/// list is maintained against the routes rather than against `flyco_core`:
/// the domain model holds types (wire frames, the budget engine's internals)
/// that deliberately never appear.
fn register_schemas(spec: &mut OpenApi) {
    let components = spec.components.get_or_insert_with(Components::new);

    register::<flyco_core::AgentsDocument>(components);
    register::<flyco_core::ApiKeySummary>(components);
    register::<flyco_core::ApprovalView>(components);
    register::<flyco_core::AuthorizeUrl>(components);
    register::<flyco_core::BudgetView>(components);
    register::<flyco_core::CloudUsageView>(components);
    register::<flyco_core::CreatedApiKey>(components);
    register::<flyco_core::DaemonToken>(components);
    register::<flyco_core::EnvDocument>(components);
    register::<flyco_core::HarnessAccountView>(components);
    register::<flyco_core::LlmUsageView>(components);
    register::<flyco_core::MachineCatalogEntry>(components);
    register::<flyco_core::MachineView>(components);
    register::<flyco_core::McpServerView>(components);
    register::<flyco_core::MemoryNode>(components);
    register::<flyco_core::Problem>(components);
    register::<flyco_core::ProviderAccountView>(components);
    register::<flyco_core::ProviderBonusHint>(components);
    register::<flyco_core::PushSubscriptionView>(components);
    register::<flyco_core::RepoStatus>(components);
    register::<flyco_core::RepoSummary>(components);
    register::<flyco_core::SessionDetail>(components);
    register::<flyco_core::SessionSummary>(components);
    register::<flyco_core::SkillView>(components);
    register::<flyco_core::TurnPage>(components);
    register::<flyco_core::VapidPublicKey>(components);
    register::<crate::relay::RelayTicket>(components);
    register::<crate::room::EventPage>(components);
}
