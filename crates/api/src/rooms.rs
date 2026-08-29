//! Reaching a session's room from the Worker.
//!
//! Every call from a request handler into a [`SessionRoom`] goes through
//! [`Rooms`]. It exists because the two platforms disagree about which
//! object answers: on the Worker a room is a Cloudflare Durable Object stub,
//! and natively it is skyzen's in-process simulator. Both take a skyzen
//! request and answer a skyzen response, so only resolving the stub is
//! written twice. Handlers should not know which one they reached.
//!
//! A room is addressed by its session id, so `session:{id}` and the room
//! are the same identity and no mapping table exists to go stale.

use flyco_core::{ControlToDaemon, RepoStatus, SessionId};
use skyzen::extract::Extractor;
use skyzen::{Body, Method, Request, StatusCode};

use crate::error::ApiError;
#[cfg(not(target_arch = "wasm32"))]
use crate::room::SessionRoom;
use crate::room::{EventPage, HEADER_INTERNAL, HEADER_SESSION, INTERNAL};
#[cfg(target_arch = "wasm32")]
use crate::room::{HEADER_ROLE, Role};

/// Cloudflare binding the session-room namespace is exposed under.
pub const BINDING: &str = "SESSION_ROOMS";

/// Origin of the internal Worker→room URLs.
///
/// A Durable Object `fetch` needs an absolute URL and never resolves this
/// host: only the path is routed. Naming it `.invalid` (RFC 2606) makes it
/// unmistakable in a log that the request never left the Worker.
const ORIGIN: &str = "https://session-room.flyco.invalid";

/// The in-process namespace native builds route rooms through.
#[cfg(not(target_arch = "wasm32"))]
pub type NativeRooms = skyzen::durable::NativeDurableNamespace<SessionRoom>;

/// Access to the session rooms behind this control plane.
#[derive(Clone)]
pub struct Rooms {
    #[cfg(not(target_arch = "wasm32"))]
    namespace: NativeRooms,
    #[cfg(target_arch = "wasm32")]
    env: skyzen::runtime::wasm::WasmEnv,
}

impl core::fmt::Debug for Rooms {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Rooms").finish_non_exhaustive()
    }
}

/// The session-room namespace was not wired into this request.
#[skyzen::error(status = StatusCode::INTERNAL_SERVER_ERROR)]
pub enum RoomsNotConfigured {
    /// The router was built without a session-room namespace.
    #[error("session rooms are not configured for this control plane")]
    Missing,
}

impl Extractor for Rooms {
    type Error = RoomsNotConfigured;

    #[cfg(not(target_arch = "wasm32"))]
    fn extract(
        request: &mut Request,
    ) -> impl core::future::Future<Output = Result<Self, Self::Error>> + Send {
        core::future::ready(
            request
                .extensions()
                .get::<skyzen::utils::State<NativeRooms>>()
                .map(|state| Self {
                    namespace: state.0.clone(),
                })
                .ok_or(RoomsNotConfigured::Missing),
        )
    }

    #[cfg(target_arch = "wasm32")]
    fn extract(
        request: &mut Request,
    ) -> impl core::future::Future<Output = Result<Self, Self::Error>> + Send {
        core::future::ready(
            request
                .extensions()
                .get::<skyzen::runtime::wasm::WasmEnv>()
                .cloned()
                .map(|env| Self { env })
                .ok_or(RoomsNotConfigured::Missing),
        )
    }
}

impl Rooms {
    /// Runs a control-plane command against a session's room.
    ///
    /// Called only after the durable half of the same change has been
    /// written, so the live relay can never announce something the database
    /// does not agree with.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Room`] if the room could not be reached or
    /// refused the command.
    pub async fn command(
        &self,
        session: SessionId,
        command: &ControlToDaemon,
    ) -> Result<(), ApiError> {
        let body = serde_json::to_vec(command)
            .map_err(|_| ApiError::CorruptRecord("a room command failed to encode"))?;
        let (status, _) = self
            .call(session, Verb::Post, "/internal/command", Some(body))
            .await?;
        if status.is_success() {
            Ok(())
        } else {
            Err(ApiError::Room(format!(
                "the room refused a command with HTTP {status}"
            )))
        }
    }

    /// Reads a page of a session's event tail, for a browser catching up.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Room`] if the room could not be reached or its
    /// answer was not a page.
    pub async fn events(&self, session: SessionId, after: u64) -> Result<EventPage, ApiError> {
        let path = format!("/internal/events?after={after}");
        let (status, body) = self.call(session, Verb::Get, &path, None).await?;
        if !status.is_success() {
            return Err(ApiError::Room(format!(
                "the room refused an event read with HTTP {status}"
            )));
        }
        serde_json::from_slice(&body)
            .map_err(|error| ApiError::Room(format!("the room returned no event page: {error}")))
    }

    /// Reads the working tree the session's daemon last reported.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::RepoStatusUnknown`] if no daemon has reported one
    /// yet, or [`ApiError::Room`] if the room could not be reached or its
    /// answer was not a working tree.
    pub async fn repo_status(&self, session: SessionId) -> Result<RepoStatus, ApiError> {
        let (status, body) = self
            .call(session, Verb::Get, "/internal/repo-status", None)
            .await?;
        if status == StatusCode::NOT_FOUND {
            return Err(ApiError::RepoStatusUnknown);
        }
        if !status.is_success() {
            return Err(ApiError::Room(format!(
                "the room refused a working-tree read with HTTP {status}"
            )));
        }
        serde_json::from_slice(&body)
            .map_err(|error| ApiError::Room(format!("the room returned no working tree: {error}")))
    }
}

/// The two verbs the Worker uses against a room.
#[derive(Debug, Clone, Copy)]
enum Verb {
    /// Read a page of events.
    Get,
    /// Run a command.
    Post,
}

/// The stub a room is reached through on this platform.
///
/// Both stubs take a skyzen [`Request`] and answer a skyzen `Response`, so
/// everything above this line is written once; only resolving the stub
/// differs between the Worker and the simulator.
#[cfg(not(target_arch = "wasm32"))]
type Stub = skyzen::durable::NativeDurableObjectStub<SessionRoom>;
/// The stub a room is reached through on this platform.
#[cfg(target_arch = "wasm32")]
type Stub = skyzen_cloudflare::CfDurableObjectStub;

impl Rooms {
    /// Resolves the stub for one session's room.
    #[cfg(not(target_arch = "wasm32"))]
    fn stub(&self, session: SessionId) -> Result<Stub, ApiError> {
        self.namespace
            .get_by_name(&session.to_string())
            .map_err(|error| ApiError::Room(error.to_string()))
    }

    /// Resolves the stub for one session's room.
    #[cfg(target_arch = "wasm32")]
    fn stub(&self, session: SessionId) -> Result<Stub, ApiError> {
        skyzen_cloudflare::CfDurableNamespace::from_env(self.env.as_js(), BINDING)
            .and_then(|namespace| namespace.get_by_name(&session.to_string()))
            .map_err(|error| ApiError::Room(error.to_string()))
    }

    /// Calls one of the room's internal routes.
    async fn call(
        &self,
        session: SessionId,
        verb: Verb,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<(StatusCode, Vec<u8>), ApiError> {
        let mut request =
            internal_request(session, path, body.map_or_else(Body::empty, Body::from))?;
        *request.method_mut() = match verb {
            Verb::Get => Method::GET,
            Verb::Post => Method::POST,
        };
        request.headers_mut().insert(
            skyzen::header::CONTENT_TYPE,
            skyzen::header::HeaderValue::from_static("application/json"),
        );

        let response = self
            .stub(session)?
            .fetch(request)
            .await
            .map_err(|error| ApiError::Room(error.to_string()))?;
        let status = response.status();
        let bytes = response
            .into_body()
            .into_bytes()
            .await
            .map_err(|error| ApiError::Room(error.to_string()))?;
        Ok((status, bytes.to_vec()))
    }

    /// Forwards an already-authenticated WebSocket upgrade to the room.
    ///
    /// The room answers `101` with its end of the socket in the response
    /// extensions, and skyzen's runtime turns that back into the platform's
    /// upgrade — so returning this response hands the client the connection.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Room`] if the room refused the upgrade.
    #[cfg(target_arch = "wasm32")]
    pub async fn upgrade(
        &self,
        session: SessionId,
        role: Role,
    ) -> Result<skyzen::Response, ApiError> {
        let mut request =
            internal_request(session, &format!("/relay/{}", role.tag()), Body::empty())?;
        *request.method_mut() = Method::GET;
        request.headers_mut().insert(
            HEADER_ROLE,
            role.tag()
                .parse()
                .map_err(|_| ApiError::CorruptRecord("a room header did not parse"))?,
        );

        let response = self
            .stub(session)?
            .fetch(request)
            .await
            .map_err(|error| ApiError::Room(error.to_string()))?;

        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            return Err(ApiError::Room(format!(
                "the room refused a relay upgrade with HTTP {}",
                response.status()
            )));
        }
        Ok(response)
    }
}

/// Builds a request marked as coming from this Worker.
fn internal_request(session: SessionId, path: &str, body: Body) -> Result<Request, ApiError> {
    let mut request = Request::new(body);
    *request.uri_mut() = format!("{ORIGIN}{path}")
        .parse()
        .map_err(|_| ApiError::CorruptRecord("a room URL did not parse"))?;
    for (name, value) in internal_headers(session) {
        request.headers_mut().insert(
            name,
            value
                .parse()
                .map_err(|_| ApiError::CorruptRecord("a room header did not parse"))?,
        );
    }
    Ok(request)
}

/// The headers that mark a call as coming from this Worker.
fn internal_headers(session: SessionId) -> [(&'static str, String); 2] {
    [
        (HEADER_INTERNAL, INTERNAL.to_owned()),
        (HEADER_SESSION, session.to_string()),
    ]
}
