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
