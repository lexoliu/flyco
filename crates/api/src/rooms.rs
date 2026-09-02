//! Reaching a Durable Object room from the Worker.
//!
//! Every call from a request handler into a room goes through [`RoomsOf`]. It
//! exists because the two platforms disagree about which object answers: on
//! the Worker a room is a Cloudflare Durable Object stub, and natively it is
//! skyzen's in-process simulator. Both take a skyzen request and answer a
//! skyzen response, so only resolving the stub is written twice. Handlers
//! should not know which one they reached.
//!
//! Flyco has two kinds of room and they are the same transport with
//! different rooms behind it, so [`RoomKind`] names what differs — the
//! binding, the header a call is marked with, the object, and what a room is
//! addressed by — and everything else is written once:
//!
//! | Alias | Room | Addressed by |
//! |---|---|---|
//! | [`Rooms`] | [`SessionRoom`](crate::room::SessionRoom) | the session id |
//! | [`HostRooms`] | [`HostRoom`](crate::host_room::HostRoom) | the host id |
//!
//! A room is addressed by the id of the thing it belongs to, so
//! `session:{id}` and the room are the same identity and no mapping table
//! exists to go stale.

#[cfg(target_arch = "wasm32")]
use core::marker::PhantomData;

use flyco_core::{ClientEvent, ControlToDaemon, HostId, RepoStatus, SessionId};
use skyzen::extract::Extractor;
use skyzen::{Body, Method, Request, StatusCode};

use crate::error::ApiError;
use crate::host_room::{HEADER_HOST, HostStatus};
use crate::room::{EventPage, HEADER_INTERNAL, HEADER_SESSION, INTERNAL};

/// Cloudflare binding the session-room namespace is exposed under.
pub const BINDING: &str = "SESSION_ROOMS";

/// Cloudflare binding the host-room namespace is exposed under.
pub const HOST_BINDING: &str = "HOST_ROOMS";

/// What tells one kind of room from another.
///
/// A Durable Object `fetch` needs an absolute URL and never resolves the
/// host in it: only the path is routed. Naming the origins `.invalid`
/// (RFC 2606) makes it unmistakable in a log that the request never left the
/// Worker.
pub trait RoomKind {
    /// Cloudflare binding this namespace is exposed under.
    const BINDING: &'static str;
    /// Header naming which room a Worker→room call belongs to.
    const ID_HEADER: &'static str;
    /// Origin of the internal Worker→room URLs.
    const ORIGIN: &'static str;
    /// The Durable Object behind the room.
    type Object: skyzen::durable::DurableObject + Send;
    /// What one room is addressed by.
    type Id: core::fmt::Display + Copy + Send + Sync;
}

/// The session relay rooms.
#[derive(Debug, Clone, Copy)]
pub struct Sessions;

impl RoomKind for Sessions {
    const BINDING: &'static str = BINDING;
    const ID_HEADER: &'static str = HEADER_SESSION;
    const ORIGIN: &'static str = "https://session-room.flyco.invalid";
    type Object = crate::room::SessionRoom;
    type Id = SessionId;
}

/// The enrolled-host rooms.
#[derive(Debug, Clone, Copy)]
pub struct Hosts;

impl RoomKind for Hosts {
    const BINDING: &'static str = HOST_BINDING;
    const ID_HEADER: &'static str = HEADER_HOST;
    const ORIGIN: &'static str = "https://host-room.flyco.invalid";
    type Object = crate::host_room::HostRoom;
    type Id = HostId;
}

/// The in-process namespace native builds route a kind of room through.
#[cfg(not(target_arch = "wasm32"))]
pub type NativeRoomsOf<K> = skyzen::durable::NativeDurableNamespace<<K as RoomKind>::Object>;

/// The in-process namespace native builds route session rooms through.
#[cfg(not(target_arch = "wasm32"))]
pub type NativeRooms = NativeRoomsOf<Sessions>;

/// The in-process namespace native builds route host rooms through.
#[cfg(not(target_arch = "wasm32"))]
pub type NativeHostRooms = NativeRoomsOf<Hosts>;

/// Access to one kind of room behind this control plane.
pub struct RoomsOf<K: RoomKind> {
    #[cfg(not(target_arch = "wasm32"))]
    namespace: NativeRoomsOf<K>,
    #[cfg(target_arch = "wasm32")]
    env: skyzen::runtime::wasm::WasmEnv,
    #[cfg(target_arch = "wasm32")]
    kind: PhantomData<fn() -> K>,
}

/// Access to the session rooms behind this control plane.
pub type Rooms = RoomsOf<Sessions>;

/// Access to the host rooms behind this control plane.
pub type HostRooms = RoomsOf<Hosts>;

impl<K: RoomKind> Clone for RoomsOf<K> {
    fn clone(&self) -> Self {
        Self {
            #[cfg(not(target_arch = "wasm32"))]
            namespace: self.namespace.clone(),
            #[cfg(target_arch = "wasm32")]
            env: self.env.clone(),
            #[cfg(target_arch = "wasm32")]
            kind: PhantomData,
        }
    }
}

impl<K: RoomKind> core::fmt::Debug for RoomsOf<K> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Rooms").finish_non_exhaustive()
    }
}

/// The room namespace was not wired into this request.
#[skyzen::error(status = StatusCode::INTERNAL_SERVER_ERROR)]
pub enum RoomsNotConfigured {
    /// The router was built without this room namespace.
    #[error("rooms are not configured for this control plane")]
    Missing,
}

impl<K: RoomKind + 'static> Extractor for RoomsOf<K> {
    type Error = RoomsNotConfigured;

    #[cfg(not(target_arch = "wasm32"))]
    fn extract(
        request: &mut Request,
    ) -> impl core::future::Future<Output = Result<Self, Self::Error>> + Send {
        core::future::ready(
            request
                .extensions()
                .get::<skyzen::utils::State<NativeRoomsOf<K>>>()
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
                .map(|env| Self {
                    env,
                    kind: PhantomData,
                })
                .ok_or(RoomsNotConfigured::Missing),
        )
    }
}

impl<K: RoomKind> RoomsOf<K> {
    /// Builds room access from an in-process namespace.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub const fn from_native(namespace: NativeRoomsOf<K>) -> Self {
        Self { namespace }
    }

    /// Builds room access for a non-HTTP Worker event.
    #[cfg(target_arch = "wasm32")]
    #[must_use]
    pub fn from_worker_env(env: skyzen::runtime::wasm::Env) -> Self {
        Self {
            env: skyzen::runtime::wasm::WasmEnv::new(env),
            kind: PhantomData,
        }
    }

    /// Builds room access from an environment this Worker already holds.
    #[cfg(target_arch = "wasm32")]
    #[must_use]
    pub const fn from_wasm_env(env: skyzen::runtime::wasm::WasmEnv) -> Self {
        Self {
            env,
            kind: PhantomData,
        }
    }
}

/// The two verbs the Worker uses against a room.
#[derive(Debug, Clone, Copy)]
enum Verb {
    /// Read.
    Get,
    /// Run a command.
    Post,
}

impl<K: RoomKind> RoomsOf<K> {
    /// Resolves the stub for one room.
    ///
    /// The two stubs take a skyzen [`Request`] and answer a skyzen
    /// `Response`, so this is the only thing written twice; everything that
    /// calls it is written once.
    #[cfg(not(target_arch = "wasm32"))]
    fn stub(
        &self,
        room: K::Id,
    ) -> Result<skyzen::durable::NativeDurableObjectStub<K::Object>, ApiError> {
        self.namespace
            .get_by_name(&room.to_string())
            .map_err(|error| ApiError::Room(error.to_string()))
    }

    /// See the native counterpart above.
    #[cfg(target_arch = "wasm32")]
    fn stub(&self, room: K::Id) -> Result<skyzen_cloudflare::CfDurableObjectStub, ApiError> {
        skyzen_cloudflare::CfDurableNamespace::from_env(self.env.as_js(), K::BINDING)
            .and_then(|namespace| namespace.get_by_name(&room.to_string()))
            .map_err(|error| ApiError::Room(error.to_string()))
    }

    /// Calls one of a room's internal routes.
    async fn call(
        &self,
        room: K::Id,
        verb: Verb,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<(StatusCode, Vec<u8>), ApiError> {
        let mut request =
            internal_request::<K>(room, path, body.map_or_else(Body::empty, Body::from))?;
        *request.method_mut() = match verb {
            Verb::Get => Method::GET,
            Verb::Post => Method::POST,
        };
        request.headers_mut().insert(
            skyzen::header::CONTENT_TYPE,
            skyzen::header::HeaderValue::from_static("application/json"),
        );

        let response = self
            .stub(room)?
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

    /// Posts one JSON document to a room route that answers with nothing.
    async fn post_json<T: serde::Serialize + Sync>(
        &self,
        room: K::Id,
        path: &'static str,
        what: &'static str,
        body: &T,
    ) -> Result<(), ApiError> {
        let encoded = serde_json::to_vec(body)
            .map_err(|_| ApiError::CorruptRecord("a room command failed to encode"))?;
        let (status, _) = self.call(room, Verb::Post, path, Some(encoded)).await?;
        if status.is_success() {
            Ok(())
        } else {
            Err(ApiError::Room(format!(
                "the room refused {what} with HTTP {status}"
            )))
        }
    }

    /// Reads one JSON document back from a room route.
    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        room: K::Id,
        path: &str,
        what: &'static str,
    ) -> Result<T, ApiError> {
        let (status, body) = self.call(room, Verb::Get, path, None).await?;
        if !status.is_success() {
            return Err(ApiError::Room(format!(
                "the room refused {what} with HTTP {status}"
            )));
        }
        serde_json::from_slice(&body)
            .map_err(|error| ApiError::Room(format!("the room returned no {what}: {error}")))
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
        room: K::Id,
        role: &'static str,
    ) -> Result<skyzen::Response, ApiError> {
        let mut request = internal_request::<K>(room, &format!("/relay/{role}"), Body::empty())?;
        *request.method_mut() = Method::GET;
        request.headers_mut().insert(
            crate::room::HEADER_ROLE,
            role.parse()
                .map_err(|_| ApiError::CorruptRecord("a room header did not parse"))?,
        );

        let response = self
            .stub(room)?
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
        self.post_json(session, "/internal/command", "a command", command)
            .await
    }

    /// Records a control-plane event on a session's stream and shows it to
    /// every browser watching.
    ///
    /// [`Self::command`] is for facts the daemon has to act on; this is for
    /// facts only the user needs to see, and the provisioning timeline is
    /// the one that needs it — the queue knows a machine was reserved
    /// minutes before any daemon exists to report it.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Room`] if the room could not be reached or
    /// refused the event.
    pub async fn broadcast(&self, session: SessionId, event: &ClientEvent) -> Result<(), ApiError> {
        self.post_json(session, "/internal/broadcast", "an event", event)
            .await
    }

    /// Reads a page of a session's event tail, for a browser catching up.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Room`] if the room could not be reached or its
    /// answer was not a page.
    pub async fn events(&self, session: SessionId, after: u64) -> Result<EventPage, ApiError> {
        self.get_json(
            session,
            &format!("/internal/events?after={after}"),
            "an event page",
        )
        .await
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

impl HostRooms {
    /// Sends one command down a host's socket, or holds it until the machine
    /// is back.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Room`] if the room could not be reached or
    /// refused the command.
    pub async fn command(
        &self,
        host: HostId,
        command: &flyco_provider::host::ControlToHost,
    ) -> Result<(), ApiError> {
        self.post_json(host, "/internal/command", "a host command", command)
            .await
    }

    /// Asks a host's room whether the machine is actually connected, and
    /// what it last said about itself.
    ///
    /// The room is the authority here and D1 is not: a socket that dropped
    /// is a fact only the Durable Object holding it can see, and a Durable
    /// Object can reach neither D1 nor the Worker's KV. So the state on the
    /// `hosts` row is what the control plane last *recorded*, and this is
    /// what it is refreshed from every time the Worker looks at a host —
    /// see [`crate::hosts::refresh`].
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Room`] if the room could not be reached or its
    /// answer was not a status.
    pub async fn status(&self, host: HostId) -> Result<HostStatus, ApiError> {
        self.get_json(host, "/internal/status", "a host status")
            .await
    }
}

/// Builds a request marked as coming from this Worker.
fn internal_request<K: RoomKind>(room: K::Id, path: &str, body: Body) -> Result<Request, ApiError> {
    let mut request = Request::new(body);
    *request.uri_mut() = format!("{}{path}", K::ORIGIN)
        .parse()
        .map_err(|_| ApiError::CorruptRecord("a room URL did not parse"))?;
    for (name, value) in [
        (HEADER_INTERNAL, INTERNAL.to_owned()),
        (K::ID_HEADER, room.to_string()),
    ] {
        request.headers_mut().insert(
            name,
            value
                .parse()
                .map_err(|_| ApiError::CorruptRecord("a room header did not parse"))?,
        );
    }
    Ok(request)
}
