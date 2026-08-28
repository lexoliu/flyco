//! Response shapes the handlers reach for.
//!
//! Skyzen 0.1.2 has no `Responder` for a bare status code, so the two cases
//! that need one — "created" and "nothing to say" — live here rather than
//! being rebuilt in each handler.

use skyzen::{Body, Request, Responder, Response, StatusCode};

/// Renders `T`, then answers `201 Created`.
///
/// The status is the *type* rather than a field, so a handler returning
/// `Created<Json<T>>` says `201` in its own signature and cannot be built
/// holding any other code. That is what lets
/// [`crate::responses`] declare an operation's status from its return type
/// instead of guessing at it, and what makes the guard over that table an
/// exact check rather than a convention.
///
/// The inner responder writes the body and headers first, so a `Json`
/// payload keeps its content type and only the status line changes.
#[derive(Debug, Clone, Copy)]
pub struct Created<T>(pub T);

impl<T: Responder> Responder for Created<T> {
    type Error = T::Error;

    fn respond_to(self, request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        self.0.respond_to(request, response)?;
        *response.status_mut() = StatusCode::CREATED;
        Ok(())
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<Vec<skyzen::openapi::ResponseSchema>> {
        T::openapi()
    }

    #[cfg(feature = "openapi")]
    fn register_openapi_schemas(
        defs: &mut std::collections::BTreeMap<String, skyzen::openapi::SchemaRef>,
    ) {
        T::register_openapi_schemas(defs);
    }
}

/// An empty `204 No Content` response.
#[must_use]
pub fn no_content() -> Response {
    status_only(StatusCode::NO_CONTENT)
}

/// An empty `202 Accepted` response.
///
/// What every route answers that hands work to a session's daemon or to the
/// provisioner: the control plane has recorded the request and the caller
/// watches the relay for what happens next.
#[must_use]
pub fn accepted() -> Response {
    status_only(StatusCode::ACCEPTED)
}

fn status_only(status: StatusCode) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    response
}
