//! Middleware flyco supplies for itself.

use core::any::TypeId;

use flyco_core::{CurrentUser, SessionId};
use skyzen::middleware::Next;
use skyzen::middleware::auth::Authenticator;
use skyzen::routing::Params;
use skyzen::utils::State;
use skyzen::{Error, Middleware, Request, Response};
use skyzen_services::Db;

use crate::daemon_tokens;
use crate::error::ApiError;
use crate::extract::Headers;

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
    A: Authenticator<User = CurrentUser, Error = ApiError> + Send + Sync + 'static,
{
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        match self.0.authenticate(request).await {
            Ok(user) => {
                request.extensions_mut().insert(State(user));
                next.run(request).await
            }
            Err(error) => Ok(error.into_response()),
        }
    }

    fn provisions(&self) -> Vec<TypeId> {
        vec![TypeId::of::<State<CurrentUser>>()]
    }
}

/// The session a daemon-scoped request is acting for.
///
/// Injected by [`RequireDaemon`] as request state, so a daemon handler
/// takes the session it may touch as an argument rather than re-deriving it
/// from the path and hoping the check happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaemonSession(pub SessionId);

/// Refuses a request that does not carry the daemon token of the session in
/// its path.
///
/// This is a second, deliberately narrow credential path beside
/// [`RequireAuth`]. It resolves to a *session*, never to a user: an `fd_`
/// token is not an identity and must never widen into one. The scope check
/// is the lookup itself — the stored hash is read by session id, so a token
/// minted for another session simply does not match, and no scope list has
/// to be kept correct.
///
/// It runs after routing, so `{id}` is already bound; skyzen's router
/// inserts [`Params`] before the endpoint's middleware chain.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequireDaemon;

impl RequireDaemon {
    /// Creates the middleware.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Resolves the session a daemon request may act on.
    async fn authorize(request: &Request) -> Result<SessionId, ApiError> {
        // Read rather than extract: `Params::extract` removes the map, and
        // the handler behind this middleware still needs it.
        let raw = request
            .extensions()
            .get::<Params>()
            .ok_or(ApiError::CorruptRecord(
                "a daemon route ran without its path parameters",
            ))?
            .get("id")
            .map_err(|_| ApiError::CorruptRecord("a daemon route bound no session id"))?
            .to_owned();
        let session: SessionId = raw
            .parse()
            .map_err(|_| ApiError::MalformedId(raw.clone()))?;

        let presented = Headers::new(request.headers().clone())
            .bearer()
            .ok_or(ApiError::MissingCredential)?
            .to_owned();

        let db = request
            .extensions()
            .get::<Db>()
            .cloned()
            .ok_or(ApiError::ServiceMissing("main"))?;

        if daemon_tokens::authenticates(&db, session, &presented).await? {
            Ok(session)
        } else {
            Err(ApiError::InvalidDaemonCredential)
        }
    }
}

impl Middleware for RequireDaemon {
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        match Self::authorize(request).await {
            Ok(session) => {
                request
                    .extensions_mut()
                    .insert(State(DaemonSession(session)));
                next.run(request).await
            }
            Err(error) => Ok(error.into_response()),
        }
    }

    fn provisions(&self) -> Vec<TypeId> {
        vec![TypeId::of::<State<DaemonSession>>()]
    }
}
