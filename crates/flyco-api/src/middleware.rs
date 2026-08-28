//! Middleware flyco supplies for itself.

use core::convert::Infallible;

use flyco_core::CurrentUser;
use skyzen::http_kit::middleware::MiddlewareError;
use skyzen::middleware::auth::Authenticator;
use skyzen::utils::State;
use skyzen::{Endpoint, Middleware, Request, Response};

use crate::error::ApiError;

/// Refuses a request that carries no valid credential.
///
/// Skyzen's own `AuthMiddleware` propagates the authenticator's error to the
/// runtime, which renders `{"error": …}` and offers no way to set the
/// `WWW-Authenticate` header RFC 6750 requires. This runs the same
/// [`Authenticator`] and renders the refusal itself.
#[derive(Debug, Clone, Copy)]
pub struct RequireAuth<A>(A);

impl<A> RequireAuth<A> {
    /// Wraps an authenticator.
    pub const fn new(authenticator: A) -> Self {
        Self(authenticator)
    }
}

impl<A> Middleware for RequireAuth<A>
where
    A: Authenticator<User = CurrentUser, Error = ApiError> + Send + Sync + Clone + 'static,
{
    type Error = Infallible;

    async fn handle<N: Endpoint>(
        &mut self,
        request: &mut Request,
        mut next: N,
    ) -> Result<Response, MiddlewareError<N::Error, Self::Error>> {
        match self.0.authenticate(request).await {
            Ok(user) => {
                request.extensions_mut().insert(State(user));
                next.respond(request)
                    .await
                    .map_err(MiddlewareError::Endpoint)
            }
            Err(error) => Ok(error.into_response()),
        }
    }
}
