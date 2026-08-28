//! Inbound GitHub webhooks.
//!
//! This route carries no flyco credential and is reachable by anyone who
//! knows the URL, so the *only* thing that separates a real GitHub delivery
//! from a forgery is the `X-Hub-Signature-256` HMAC over the raw body. That
//! makes the order of operations part of the security boundary rather than a
//! matter of style: the signature is verified against the exact bytes
//! received, with a constant-time comparison, **before** the body is parsed,
//! before it is logged, and before anything is queued.
//!
//! Verification itself is M6's work, alongside the CI-autofix flow it feeds.
//! Until then the handler's first statement is its `todo!()`, so there is no
//! reachable path in which an unverified body is read at all — a stub that
//! parsed the payload "for now" would be exactly the shape of the bug this
//! comment exists to prevent.

use skyzen::Response;
use skyzen::routing::{CreateRouteNode, Route, RouteNode, Routes as _};
use skyzen::utils::Bytes;
use skyzen_services::Db;

use crate::extract::Headers;
use crate::problem::Outcome;

/// Header GitHub signs each delivery with, per its webhook documentation.
pub const SIGNATURE_HEADER: &str = "x-hub-signature-256";

/// Header naming which event a delivery carries.
pub const EVENT_HEADER: &str = "x-github-event";

/// Accepts a signed GitHub webhook delivery.
///
/// Answers `204`: GitHub only needs to know the delivery was accepted, and
/// the work it triggers is queued rather than done inline.
#[skyzen::openapi]
async fn receive_github_webhook(_headers: Headers, _body: Bytes, _db: Db) -> Outcome<Response> {
    todo!(
        "M6: verify the X-Hub-Signature-256 HMAC over the raw body in constant time, and only then parse and queue it"
    )
}

/// The webhook route, which authenticates itself by signature.
pub fn routes() -> Vec<RouteNode> {
    Route::new(("/v1/webhooks/github".post(receive_github_webhook),)).into_route_nodes()
}
