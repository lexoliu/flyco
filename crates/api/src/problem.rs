//! Rendering failures as RFC 9457 documents, with RFC 6750 challenges.
//!
//! Skyzen's runtime turns a propagated [`HttpError`](skyzen::HttpError) into
//! `{"error": …}` and gives no way to add response headers, so flyco never
//! propagates its own errors: handlers hand back an [`Outcome`], which
//! renders the problem document itself.

use core::fmt;

use flyco_core::Problem;
use flyco_core::problem::CONTENT_TYPE as PROBLEM_CONTENT_TYPE;
use skyzen::header::{CONTENT_TYPE, HeaderValue, WWW_AUTHENTICATE};
use skyzen::{Body, Request, Responder, Response, StatusCode};

use crate::error::ApiError;

/// The `WWW-Authenticate` challenge a 401 carries, per RFC 6750 §3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Challenge {
    /// No credential was presented: name the scheme and nothing else.
    Bearer,
    /// A credential was presented and rejected.
    InvalidToken,
}

impl Challenge {
    const fn header(self) -> HeaderValue {
        match self {
            Self::Bearer => HeaderValue::from_static("Bearer"),
            Self::InvalidToken => HeaderValue::from_static("Bearer error=\"invalid_token\""),
        }
    }
}

/// Builds a complete problem response.
///
/// Falls back to 500 if the document carries a status code HTTP does not
/// know — an impossible state that must still not panic mid-response.
#[must_use]
pub fn response(problem: &Problem, challenge: Option<Challenge>) -> Response {
    let mut response = Response::new(Body::empty());
    write(&mut response, problem, challenge);
    response
}

/// Overwrites `response` with a problem document.
pub fn write(response: &mut Response, problem: &Problem, challenge: Option<Challenge>) {
    *response.status_mut() =
        StatusCode::from_u16(problem.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(PROBLEM_CONTENT_TYPE));
    if let Some(challenge) = challenge {
        response
            .headers_mut()
            .insert(WWW_AUTHENTICATE, challenge.header());
    }
    *response.body_mut() = Body::from_json(problem).unwrap_or_else(|_| Body::empty());
}

/// A handler outcome: success renders through `T`, failure as RFC 9457.
pub struct Outcome<T>(Result<T, ApiError>);

impl<T> fmt::Debug for Outcome<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Outcome").finish_non_exhaustive()
    }
}

impl<T> From<Result<T, ApiError>> for Outcome<T> {
    fn from(result: Result<T, ApiError>) -> Self {
        Self(result)
    }
}

impl<T: Responder> Responder for Outcome<T> {
    type Error = T::Error;

    fn respond_to(self, request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        match self.0 {
            Ok(value) => value.respond_to(request, response),
            Err(error) => {
                write(response, &error.problem(), error.challenge());
                Ok(())
            }
        }
    }

    /// Describes the success response by forwarding to the wrapped responder.
    ///
    /// Without this, wrapping a handler's return type in [`Outcome`] erases
    /// its response schema: every operation in the exported document would
    /// name a path and a request body but nothing about what comes back, and
    /// a client generated from it would be untyped at exactly the boundary
    /// that matters.
    ///
    /// The failure side is deliberately not enumerated per operation. Every
    /// error this API can produce is the same RFC 9457 document — the type
    /// is registered into the components map by
    /// [`register_openapi_schemas`](Responder::register_openapi_schemas) —
    /// and listing a speculative set of statuses on each operation would
    /// assert failures a given handler cannot actually return.
    #[cfg(feature = "openapi")]
    fn openapi() -> Option<Vec<skyzen::openapi::ResponseSchema>> {
        T::openapi()
    }

    #[cfg(feature = "openapi")]
    fn register_openapi_schemas(
        defs: &mut std::collections::BTreeMap<String, skyzen::openapi::SchemaRef>,
    ) {
        T::register_openapi_schemas(defs);
        skyzen::openapi::maybe_register_schema_for::<flyco_core::Problem>(defs);
    }
}

#[cfg(test)]
mod tests {
    use flyco_core::Problem;
    use skyzen::header::HeaderValue;

    use super::{Challenge, response};

    #[test]
    fn a_problem_response_is_typed_as_problem_json() {
        let rendered = response(
            &Problem::of_type("invalid-credential", 401, "Unauthorized", "nope"),
            Some(Challenge::InvalidToken),
        );

        assert_eq!(rendered.status().as_u16(), 401);
        assert_eq!(
            rendered
                .headers()
                .get("content-type")
                .map(HeaderValue::as_bytes),
            Some(b"application/problem+json".as_slice())
        );
        assert_eq!(
            rendered
                .headers()
                .get("www-authenticate")
                .map(HeaderValue::as_bytes),
            Some(b"Bearer error=\"invalid_token\"".as_slice())
        );
    }

    #[test]
    fn a_challengeless_problem_omits_the_authenticate_header() {
        let rendered = response(&Problem::about_blank(400, "Bad Request", "nope"), None);
        assert!(rendered.headers().get("www-authenticate").is_none());
    }
}

#[cfg(all(test, feature = "openapi"))]
mod schema_forwarding {
    use skyzen::utils::Json;

    use super::Outcome;

    /// Wrapping a responder must not erase what it says about itself.
    ///
    /// Note that skyzen 0.1.2 reports no payload schema through this path at
    /// all (`maybe_schema_of` is generic, so its specialization probe cannot
    /// fire) — see [`crate::responses`], which is what describes the
    /// responses in the meantime. This test pins the forwarding itself, so
    /// the operation documents improve the moment that is fixed upstream.
    #[test]
    fn outcome_reports_whatever_the_wrapped_responder_reports() {
        let direct = <Json<flyco_core::Problem> as skyzen::Responder>::openapi();
        let wrapped = <Outcome<Json<flyco_core::Problem>> as skyzen::Responder>::openapi();
        assert_eq!(direct.is_some(), wrapped.is_some());
        assert_eq!(
            direct.map(|s| s.len()),
            wrapped.map(|s| s.len()),
            "Outcome must pass the wrapped responder's descriptions through"
        );
    }
}
