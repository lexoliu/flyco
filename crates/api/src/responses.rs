//! The parts of the exported `OpenAPI` document the derivation misses.
//!
//! Almost all of it derives itself now. A handler's return type describes
//! its own response — [`Outcome`](crate::problem::Outcome) forwards what it
//! wraps, and [`Created`](crate::respond::Created),
//! [`NoContent`](crate::respond::NoContent),
//! [`Accepted`](crate::respond::Accepted) and
//! [`SeeOther`](crate::respond::SeeOther) each report their own status — so
//! the operation-by-operation table this module used to hold, and the
//! source-scanning guard that kept it honest, are both gone.
//!
//! Two things are still this module's:
//!
//! * **Naming the element of a collection.** `Json<Vec<T>>` registers its
//!   schema under `Vec<T>`'s `utoipa` name, which is the bare string `Vec`
//!   for every element type; `T` itself never reaches `components.schemas`,
//!   and the response inlines it instead. The generated TypeScript client
//!   reads those types by name, so [`register_schemas`] puts every returned
//!   DTO in the components map and [`prune`] removes the `Vec` entry, which
//!   names nothing and is referenced by nothing.
//! * **Saying which operations answer with no document at all**, so that an
//!   operation which *should* describe a body and does not is a failing
//!   test rather than an untyped hole in the client.

use utoipa::openapi::path::Operation;
use utoipa::openapi::{Components, OpenApi};

/// The component `utoipa` names every `Vec<T>` response after.
///
/// Not a type any client can use: one entry serves every collection in the
/// document, so whichever element type happened to be registered first is
/// the one it describes.
const COLLECTION_PLACEHOLDER: &str = "Vec";

/// The operations that answer with no document, and why.
///
/// Everything else must carry response content. These are the responses a
/// schema cannot describe rather than ones nobody got round to describing:
///
/// * `204` and `202` answers — a revocation, a delete, a command handed to a
///   session's daemon — carry a status and nothing else.
/// * `oauth::callback` answers `303`: the body of a redirect is not what the
///   caller reads.
/// * `app::open_daemon_relay` and `app::open_client_relay` answer `101` with
///   a WebSocket attached, which the response model has no way to describe.
/// * `app::get_release_artifact`, `app::put_transcript_batch`,
///   `app::get_transcript`, and the workdir patch pair carry raw bytes: a
///   release is an executable or checksum, a transcript batch is
///   newline-delimited JSON, and a workdir snapshot is a `git` binary diff,
///   not a document.
/// * `repos::list_repos` is generic over the GitHub client, and
///   `#[skyzen::openapi]` cannot annotate a generic handler — the macro
///   emits module-level items naming every argument type. Its id carries the
///   type argument it was monomorphized with, as `oauth::callback`'s does.
pub const BODILESS: &[&str] = &[
    "app::get_release_artifact",
    "app::get_transcript",
    "app::open_client_relay",
    "app::open_daemon_relay",
    "app::put_transcript_batch",
    "flyco_api::app::compact_session",
    "flyco_api::app::get_workdir_patch",
    "flyco_api::app::interrupt_session",
    "flyco_api::app::notify_turn_completed",
    "flyco_api::app::notify_turn_failed",
    "flyco_api::app::put_harness_session",
    "flyco_api::app::put_workdir_patch",
    "flyco_api::app::record_harness_observation",
    "flyco_api::app::report_provisioning_stage",
    "flyco_api::app::revoke_api_key",
    "flyco_api::app::send_message",
    "flyco_api::harness_accounts::unlink_harness_account",
    "flyco_api::machines::resize_session_machine",
    "flyco_api::machines::start_session_machine",
    "flyco_api::machines::stop_session_machine",
    "flyco_api::mcp::delete_mcp_server",
    "flyco_api::memory::delete_memory_node",
    "flyco_api::oauth::callback",
    "flyco_api::provider_accounts::unlink_provider",
    "flyco_api::push::unsubscribe_push",
    "flyco_api::skills::delete_skill",
    "flyco_api::webhooks::receive_github_webhook",
];

/// Names every returned DTO in a generated document, and drops the
/// placeholder component that stands in for collections.
pub fn finish(spec: &mut OpenApi) {
    register_schemas(spec);
    prune(spec);
}

/// Removes the component that names a shape rather than a type.
fn prune(spec: &mut OpenApi) {
    if let Some(components) = spec.components.as_mut() {
        components.schemas.remove(COLLECTION_PLACEHOLDER);
    }
}

/// Every operation a path item holds.
///
/// `PathItem` is eight optional fields rather than a map, so the verbs are
/// listed once here instead of at each call site.
pub fn operations(
    item: &mut utoipa::openapi::path::PathItem,
) -> impl Iterator<Item = &mut Operation> {
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

/// Adds `T` — and everything `T` refers to — to a document's components.
///
/// `utoipa`'s derive walks a type's own references, so registering the
/// outermost DTO of a response is enough to bring the whole tree with it.
fn register<T: utoipa::ToSchema>(components: &mut Components) {
    let mut collected = vec![(T::name().into_owned(), T::schema())];
    T::schemas(&mut collected);
    for (name, schema) in collected {
        components.schemas.entry(name).or_insert(schema);
    }
}

/// Registers every DTO the control plane *returns*.
///
/// A response schema is written inline into its operation, so a type that is
/// only ever returned inside a `Vec` has no name in the document at all
/// unless it is put there. The list is maintained against the routes rather
/// than against `flyco_core`: the domain model holds types (wire frames, the
/// budget engine's internals) that deliberately never appear.
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
    register::<flyco_core::HarnessFeature>(components);
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
