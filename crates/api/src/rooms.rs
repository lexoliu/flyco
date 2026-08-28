//! Reaching a session's room from the Worker.
//!
//! Every call from a request handler into a [`SessionRoom`] goes through
//! [`Rooms`]. It exists because the two platforms disagree about everything
//! except the shape of the call: on the Worker a room is a Cloudflare
//! Durable Object stub taking `web_sys` requests, and natively it is
//! skyzen's in-process simulator taking skyzen requests. Handlers should
//! not know which.
//!
//! A room is addressed by its session id, so `session:{id}` and the room
//! are the same identity and no mapping table exists to go stale.

use flyco_core::{ControlToDaemon, RepoStatus, SessionId};
use skyzen::extract::Extractor;
use skyzen::{Request, StatusCode};

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

#[cfg(not(target_arch = "wasm32"))]
impl Rooms {
    /// Calls one of the room's internal routes through the simulator.
    async fn call(
        &self,
        session: SessionId,
        verb: Verb,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<(StatusCode, Vec<u8>), ApiError> {
        use skyzen::{Body, Method};

        let stub = self
            .namespace
            .get_by_name(&session.to_string())
            .map_err(|error| ApiError::Room(error.to_string()))?;

        let mut request = Request::new(body.map_or_else(Body::empty, Body::from));
        *request.method_mut() = match verb {
            Verb::Get => Method::GET,
            Verb::Post => Method::POST,
        };
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
        request.headers_mut().insert(
            skyzen::header::CONTENT_TYPE,
            skyzen::header::HeaderValue::from_static("application/json"),
        );

        let response = stub
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
}

/// The `web_sys` types this module needs, reached through the crate that
/// already pins their feature set. Depending on `web-sys` directly would
/// mean keeping a second feature list in step with `worker-sys`'s.
#[cfg(target_arch = "wasm32")]
use skyzen_cloudflare::worker::send::SendFuture;
#[cfg(target_arch = "wasm32")]
use skyzen_cloudflare::worker_sys::web_sys;

#[cfg(target_arch = "wasm32")]
impl Rooms {
    /// Resolves the Durable Object stub for one session.
    fn stub(&self, session: SessionId) -> Result<skyzen_cloudflare::CfDurableObjectStub, ApiError> {
        skyzen_cloudflare::CfDurableNamespace::from_env(self.env.as_js(), BINDING)
            .and_then(|namespace| namespace.get_by_name(&session.to_string()))
            .map_err(|error| ApiError::Room(error.to_string()))
    }

    /// Calls one of the room's internal routes through the Durable Object.
    ///
    /// The whole body runs inside a [`SendFuture`]. JavaScript handles —
    /// `worker::Request`, `js_sys::Promise`, `JsFuture` — are `!Send`, and
    /// skyzen's `Handler` requires a handler\'s future to be `Send`, so a
    /// single `!Send` local held across an `await` would make every route
    /// that reaches a room fail to compile. Workers are single-threaded, so
    /// asserting `Send` over the whole block is sound; this is the same
    /// wrapper `skyzen-cloudflare` and `worker` use internally.
    async fn call(
        &self,
        session: SessionId,
        verb: Verb,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<(StatusCode, Vec<u8>), ApiError> {
        let url = format!("{ORIGIN}{path}");
        SendFuture::new(async move {
            let headers = internal_headers(session);
            let mut headers: Vec<(&str, &str)> = headers
                .iter()
                .map(|(name, value)| (*name, value.as_str()))
                .collect();
            headers.push(("content-type", "application/json"));

            let method = match verb {
                Verb::Get => skyzen_cloudflare::worker::Method::Get,
                Verb::Post => skyzen_cloudflare::worker::Method::Post,
            };
            let request = skyzen_cloudflare::http_request::bare_request(
                method,
                &url,
                &headers,
                body.as_deref(),
            )
            .map_err(|error| ApiError::Room(error.to_string()))?;

            let response = self
                .stub(session)?
                .fetch(request.inner())
                .await
                .map_err(|error| ApiError::Room(error.to_string()))?;

            let status = StatusCode::from_u16(response.status())
                .map_err(|_| ApiError::Room("the room answered with no status".to_owned()))?;
            let text = read_text(&response).await?;
            Ok((status, text.into_bytes()))
        })
        .await
    }

    /// Forwards an already-authenticated WebSocket upgrade to the room.
    ///
    /// The room answers `101` with a `webSocket` property; skyzen's runtime
    /// turns a `DurableClientWebSocket` extension back into exactly that, so
    /// the client end is handed straight back to the browser or the daemon.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Room`] if the room refused the upgrade or
    /// answered without a socket.
    pub async fn upgrade(
        &self,
        session: SessionId,
        role: Role,
    ) -> Result<skyzen::Response, ApiError> {
        SendFuture::new(self.forward_upgrade(session, role)).await
    }

    /// The `!Send` half of [`upgrade`](Self::upgrade). See [`call`](Self::call).
    async fn forward_upgrade(
        &self,
        session: SessionId,
        role: Role,
    ) -> Result<skyzen::Response, ApiError> {
        use skyzen::wasm_bindgen::JsCast as _;

        let headers = internal_headers_for(session, role);
        let headers: Vec<(&str, &str)> = headers
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        let url = format!("{ORIGIN}/relay/{}", role.tag());
        let request = skyzen_cloudflare::http_request::bare_request(
            skyzen_cloudflare::worker::Method::Get,
            &url,
            &headers,
            None,
        )
        .map_err(|error| ApiError::Room(error.to_string()))?;

        let answer = self
            .stub(session)?
            .fetch(request.inner())
            .await
            .map_err(|error| ApiError::Room(error.to_string()))?;

        if answer.status() != StatusCode::SWITCHING_PROTOCOLS.as_u16() {
            return Err(ApiError::Room(format!(
                "the room refused a relay upgrade with HTTP {}",
                answer.status()
            )));
        }

        let socket = js_sys::Reflect::get(answer.as_ref(), &"webSocket".into())
            .ok()
            .and_then(|value| value.dyn_into::<web_sys::WebSocket>().ok())
            .ok_or_else(|| {
                ApiError::Room("the room accepted an upgrade without a socket".to_owned())
            })?;

        let mut response = skyzen::Response::new(skyzen::Body::empty());
        *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        response
            .extensions_mut()
            .insert(skyzen::durable::DurableClientWebSocket(socket));
        Ok(response)
    }
}

/// Reads a Durable Object response body as text.
#[cfg(target_arch = "wasm32")]
async fn read_text(response: &web_sys::Response) -> Result<String, ApiError> {
    let promise = response
        .text()
        .map_err(|_| ApiError::Room("the room answered with an unreadable body".to_owned()))?;
    skyzen::wasm_bindgen_futures::JsFuture::from(promise)
        .await
        .ok()
        .and_then(|value| value.as_string())
        .ok_or_else(|| ApiError::Room("the room answered with a non-text body".to_owned()))
}

/// The headers that mark a call as coming from this Worker.
fn internal_headers(session: SessionId) -> [(&'static str, String); 2] {
    [
        (HEADER_INTERNAL, INTERNAL.to_owned()),
        (HEADER_SESSION, session.to_string()),
    ]
}

/// The headers a forwarded relay upgrade carries.
#[cfg(target_arch = "wasm32")]
fn internal_headers_for(session: SessionId, role: Role) -> [(&'static str, String); 3] {
    [
        (HEADER_INTERNAL, INTERNAL.to_owned()),
        (HEADER_SESSION, session.to_string()),
        (HEADER_ROLE, role.tag().to_owned()),
    ]
}
