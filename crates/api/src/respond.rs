//! Response shapes the handlers reach for.
//!
//! Each one is a *type* whose status is fixed by construction, and each one
//! reports that status through [`Responder::openapi`]. That is what lets the
//! exported document say `201`, `202`, `204` and `303` where the handler
//! means them: the status is derived from the return type rather than
//! declared beside it in a table that could disagree.

use skyzen::header::{HeaderValue, LOCATION};
use skyzen::{Body, Request, Responder, Response, StatusCode};
use url::Url;

/// Renders `T`, then answers `201 Created`.
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
        Some(described(
            T::openapi()?,
            StatusCode::CREATED,
            "The resource that was created.",
        ))
    }

    #[cfg(feature = "openapi")]
    fn register_openapi_schemas(
        defs: &mut std::collections::BTreeMap<String, skyzen::openapi::SchemaRef>,
    ) {
        T::register_openapi_schemas(defs);
    }
}

/// An empty `204 No Content` response.
#[derive(Debug, Clone, Copy)]
pub struct NoContent;

impl Responder for NoContent {
    type Error = core::convert::Infallible;

    fn respond_to(self, _request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        status_only(response, StatusCode::NO_CONTENT);
        Ok(())
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<Vec<skyzen::openapi::ResponseSchema>> {
        Some(vec![empty(
            StatusCode::NO_CONTENT,
            "Done. There is nothing to return.",
        )])
    }
}

/// An empty `202 Accepted` response.
///
/// What every route answers that hands work to a session's daemon or to the
/// provisioner: the control plane has recorded the request and the caller
/// watches the relay for what happens next.
#[derive(Debug, Clone, Copy)]
pub struct Accepted;

impl Responder for Accepted {
    type Error = core::convert::Infallible;

    fn respond_to(self, _request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        status_only(response, StatusCode::ACCEPTED);
        Ok(())
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<Vec<skyzen::openapi::ResponseSchema>> {
        Some(vec![empty(
            StatusCode::ACCEPTED,
            "Recorded. The outcome arrives on the session relay, not in this response.",
        )])
    }
}

/// A `303 See Other` handing the browser on.
///
/// Both places this is used return a browser from somebody else's
/// authorization page, where the answer has to be a navigation rather than a
/// document.
#[derive(Debug, Clone)]
pub struct SeeOther(pub Url);

impl Responder for SeeOther {
    type Error = core::convert::Infallible;

    fn respond_to(self, _request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        // A parsed `Url` percent-encodes everything a header value forbids,
        // so this conversion cannot fail.
        let location = HeaderValue::from_str(self.0.as_str())
            .expect("a parsed URL is always a valid header value");
        status_only(response, StatusCode::SEE_OTHER);
        response.headers_mut().insert(LOCATION, location);
        Ok(())
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<Vec<skyzen::openapi::ResponseSchema>> {
        Some(vec![empty(
            StatusCode::SEE_OTHER,
            "The browser is sent on to the flyco web app.",
        )])
    }
}

fn status_only(response: &mut Response, status: StatusCode) {
    *response.status_mut() = status;
    *response.body_mut() = Body::empty();
}

/// Restates a wrapped responder's schemas under this wrapper's status.
#[cfg(feature = "openapi")]
fn described(
    schemas: Vec<skyzen::openapi::ResponseSchema>,
    status: StatusCode,
    description: &'static str,
) -> Vec<skyzen::openapi::ResponseSchema> {
    schemas
        .into_iter()
        .map(|schema| skyzen::openapi::ResponseSchema {
            status: Some(status),
            description: Some(description),
            ..schema
        })
        .collect()
}

/// One response that carries a status and nothing else.
#[cfg(feature = "openapi")]
const fn empty(status: StatusCode, description: &'static str) -> skyzen::openapi::ResponseSchema {
    skyzen::openapi::ResponseSchema {
        status: Some(status),
        description: Some(description),
        schema: None,
        content_type: None,
    }
}
