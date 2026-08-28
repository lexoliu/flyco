//! Response shapes the handlers reach for.
//!
//! Skyzen 0.1.2 has no `Responder` for a bare status code, so the two cases
//! that need one — "created" and "nothing to say" — live here rather than
//! being rebuilt in each handler.

use skyzen::{Body, Request, Responder, Response, StatusCode};

/// Renders `T`, then overrides the status code.
///
/// The inner responder writes the body and headers first, so a `Json`
/// payload keeps its content type and only the status line changes.
#[derive(Debug, Clone, Copy)]
pub struct WithStatus<T>(pub StatusCode, pub T);

impl<T: Responder> Responder for WithStatus<T> {
    type Error = T::Error;

    fn respond_to(self, request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        self.1.respond_to(request, response)?;
        *response.status_mut() = self.0;
        Ok(())
    }
}

/// Wraps a freshly created resource so it answers `201 Created`.
pub const fn created<T>(value: T) -> WithStatus<T> {
    WithStatus(StatusCode::CREATED, value)
}

/// An empty `204 No Content` response.
#[must_use]
pub fn no_content() -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    response
}
