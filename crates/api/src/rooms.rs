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

use core::fmt::Write as _;
#[cfg(target_arch = "wasm32")]
use core::marker::PhantomData;

use flyco_core::wire::{DaemonAttach, DaemonAttached, DaemonFrames};
use flyco_core::workdir::{WorkdirReply, WorkdirRequest};
use flyco_core::{
    ClientEvent, ControlToDaemon, DesktopInputRequest, DesktopTakeoverRequest, HostId, Problem,
    RepoStatus, SessionId, UserId, WorkdirRequestId,
};
use flyco_provider::host::{HostAttach, HostFrames};
use futures_util::StreamExt as _;
use skyzen::extract::Extractor;
use skyzen::{Body, Method, Request, StatusCode};
use skyzen_services::Db;

use crate::error::ApiError;
use crate::host_room::{HEADER_HOST, HostAttachResponse, HostStatus};
use crate::room::{AttachResponse, Emitted, HEADER_INTERNAL, HEADER_SESSION, INTERNAL};
use crate::user_events::{HEADER_USER, PublishEvents};
use flyco_core::wire::EventPage;

/// Cloudflare binding the session-room namespace is exposed under.
pub const BINDING: &str = "SESSION_ROOMS";

/// Cloudflare binding the host-room namespace is exposed under.
pub const HOST_BINDING: &str = "HOST_ROOMS";

/// Cloudflare binding the per-user event stream is exposed under.
pub const EVENTS_BINDING: &str = "USER_EVENTS";

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

/// The per-user event streams.
#[derive(Debug, Clone, Copy)]
pub struct Users;

impl RoomKind for Users {
    const BINDING: &'static str = EVENTS_BINDING;
    const ID_HEADER: &'static str = HEADER_USER;
    const ORIGIN: &'static str = "https://user-events.flyco.invalid";
    type Object = crate::user_events::UserEvents;
    type Id = UserId;
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

/// The in-process namespace native builds route user event streams through.
#[cfg(not(target_arch = "wasm32"))]
pub type NativeUserStreams = NativeRoomsOf<Users>;

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
///
/// Not the bare namespace: every route a session room answers can produce
/// events owed to the session's owner, and the room cannot publish them
/// itself — a Durable Object addresses nothing but itself. So this
/// bundles the owner's stream beside the rooms, and the methods that can
/// emit — [`Self::command`], [`Self::broadcast`], the daemon relay —
/// publish what the room made before they return. A caller that could
/// skip the fan-out would be a live event silently lost, which is why
/// the bundle exists rather than a publish step to remember.
#[derive(Debug, Clone)]
pub struct Rooms {
    rooms: RoomsOf<Sessions>,
    streams: RoomsOf<Users>,
}

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

impl Rooms {
    /// Builds session-room access around an in-process namespace.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub const fn from_native(rooms: NativeRooms, streams: NativeUserStreams) -> Self {
        Self {
            rooms: RoomsOf::from_native(rooms),
            streams: RoomsOf::from_native(streams),
        }
    }

    /// Builds session-room access for a non-HTTP Worker event.
    #[cfg(target_arch = "wasm32")]
    #[must_use]
    pub fn from_worker_env(env: skyzen::runtime::wasm::Env) -> Self {
        Self::from_wasm_env(skyzen::runtime::wasm::WasmEnv::new(env))
    }

    /// Builds session-room access from an environment this Worker already
    /// holds.
    #[cfg(target_arch = "wasm32")]
    #[must_use]
    pub fn from_wasm_env(env: skyzen::runtime::wasm::WasmEnv) -> Self {
        Self {
            rooms: RoomsOf::from_wasm_env(env.clone()),
            streams: RoomsOf::from_wasm_env(env),
        }
    }
}

impl Extractor for Rooms {
    type Error = RoomsNotConfigured;

    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        Ok(Self {
            rooms: RoomsOf::<Sessions>::extract(&mut *request).await?,
            streams: RoomsOf::<Users>::extract(&mut *request).await?,
        })
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

    /// Calls one of a room's internal routes and hands the response back.
    ///
    /// The body is the caller's to finish: JSON routes read it whole,
    /// stream routes hand it to the client still running.
    async fn call_raw(
        &self,
        room: K::Id,
        verb: Verb,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<skyzen::Response, ApiError> {
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

        self.stub(room)?
            .fetch(request)
            .await
            .map_err(|error| ApiError::Room(error.to_string()))
    }

    /// Calls one of a room's internal routes and buffers its answer.
    async fn call(
        &self,
        room: K::Id,
        verb: Verb,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<(StatusCode, Vec<u8>), ApiError> {
        let response = self.call_raw(room, verb, path, body).await?;
        let status = response.status();
        let bytes = response
            .into_body()
            .into_bytes()
            .await
            .map_err(|error| ApiError::Room(error.to_string()))?;
        Ok((status, bytes.to_vec()))
    }

    /// Forwards a GET to a room and hands the response back still running.
    ///
    /// For the routes that answer with a stream: the body is the product,
    /// and buffering it would hang the request on a stream that never
    /// ends.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::RoomRefused`] carrying the room's own problem
    /// document when it refused, or [`ApiError::Room`] if the room could
    /// not be reached at all.
    async fn call_streaming(
        &self,
        room: K::Id,
        path: &str,
        what: &'static str,
    ) -> Result<skyzen::Response, ApiError> {
        let response = self.call_raw(room, Verb::Get, path, None).await?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let body = response
            .into_body()
            .into_bytes()
            .await
            .map_err(|error| ApiError::Room(error.to_string()))?;
        Err(refused(status, &body, what))
    }

    /// Posts one JSON document to a room route and ignores the answer's
    /// body.
    ///
    /// For the routes whose only payload is the status itself — a host
    /// command, a workdir question. The session room's command routes
    /// answer with the events the call produced, and callers that owe
    /// fan-out use [`Self::post_json_for`] instead.
    async fn post_json<T: serde::Serialize + Sync>(
        &self,
        room: K::Id,
        path: &'static str,
        what: &'static str,
        body: &T,
    ) -> Result<(), ApiError> {
        let encoded = serde_json::to_vec(body)
            .map_err(|_| ApiError::CorruptRecord("a room command failed to encode"))?;
        let (status, answer) = self.call(room, Verb::Post, path, Some(encoded)).await?;
        if status.is_success() {
            Ok(())
        } else {
            Err(refused(status, &answer, what))
        }
    }

    /// Posts one JSON document to a room route and decodes the answer.
    async fn post_json_for<T, R>(
        &self,
        room: K::Id,
        path: &'static str,
        what: &'static str,
        body: &T,
    ) -> Result<R, ApiError>
    where
        T: serde::Serialize + Sync,
        R: serde::de::DeserializeOwned,
    {
        let encoded = serde_json::to_vec(body)
            .map_err(|_| ApiError::CorruptRecord("a room command failed to encode"))?;
        let (status, answer) = self.call(room, Verb::Post, path, Some(encoded)).await?;
        if !status.is_success() {
            return Err(refused(status, &answer, what));
        }
        serde_json::from_slice(&answer)
            .map_err(|error| ApiError::Room(format!("the room returned no {what}: {error}")))
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
            return Err(refused(status, &body, what));
        }
        serde_json::from_slice(&body)
            .map_err(|error| ApiError::Room(format!("the room returned no {what}: {error}")))
    }
}

/// Turns a room's non-success answer into the error that reports it.
///
/// A refusal the room typed — a stale epoch, a frames gap, an offline
/// daemon — is already the document its caller should see, so it is
/// forwarded rather than wrapped: a daemon told only "502" could not
/// tell "attach again" from "the room is down". Only a room that
/// answered with no problem at all is reported as a room failure.
fn refused(status: StatusCode, body: &[u8], what: &'static str) -> ApiError {
    match serde_json::from_slice::<Problem>(body) {
        Ok(problem) if problem.status == status.as_u16() => {
            ApiError::RoomRefused(Box::new(problem))
        }
        _ => ApiError::Room(format!("the room refused {what} with HTTP {status}")),
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
    /// refused the command, or if the events the call produced could not
    /// be published to the owner's stream.
    pub async fn command(
        &self,
        db: &Db,
        session: SessionId,
        command: &ControlToDaemon,
    ) -> Result<(), ApiError> {
        let emitted = self
            .rooms
            .post_json_for(session, "/internal/command", "a command", command)
            .await?;
        self.fan_out(db, session, emitted).await
    }

    /// Records a control-plane event on a session's stream.
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
    pub async fn broadcast(
        &self,
        db: &Db,
        session: SessionId,
        event: &ClientEvent,
    ) -> Result<(), ApiError> {
        let emitted = self
            .rooms
            .post_json_for(session, "/internal/broadcast", "an event", event)
            .await?;
        self.fan_out(db, session, emitted).await
    }

    /// Attaches a session's daemon to its room.
    ///
    /// The Worker forwards the daemon's [`DaemonAttach`] untouched; the
    /// room's answer splits in two — the epoch goes to the daemon, the
    /// events the attach produced go to the owner's stream.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Room`] if the room could not be reached or
    /// refused the attach.
    pub async fn daemon_attach(
        &self,
        db: &Db,
        session: SessionId,
        attach: &DaemonAttach,
    ) -> Result<DaemonAttached, ApiError> {
        let attached: AttachResponse = self
            .rooms
            .post_json_for(session, "/internal/daemon-attach", "an attach", attach)
            .await?;
        self.fan_out(
            db,
            session,
            Emitted {
                events: attached.events,
            },
        )
        .await?;
        Ok(DaemonAttached {
            epoch: attached.epoch,
        })
    }

    /// Posts one batch of a daemon's outbound frames to its room.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Room`] if the room could not be reached or
    /// refused the batch — including a stale epoch or a sequence gap,
    /// both of which the daemon answers by re-attaching.
    pub async fn daemon_frames(
        &self,
        db: &Db,
        session: SessionId,
        batch: &DaemonFrames,
    ) -> Result<(), ApiError> {
        let emitted = self
            .rooms
            .post_json_for(session, "/internal/frames", "a frame batch", batch)
            .await?;
        self.fan_out(db, session, emitted).await
    }

    /// Opens a daemon's command stream against its room.
    ///
    /// The response is the room's SSE stream, still running; the Worker's
    /// route hands it to the daemon verbatim.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Room`] if the room could not be reached or
    /// refused the stream — including a stale epoch.
    pub async fn daemon_commands(
        &self,
        session: SessionId,
        epoch: u64,
    ) -> Result<skyzen::Response, ApiError> {
        self.rooms
            .call_streaming(
                session,
                &format!("/internal/commands?epoch={epoch}"),
                "a command stream",
            )
            .await
    }

    /// Publishes the events a room call produced onto the owner's stream.
    async fn fan_out(&self, db: &Db, session: SessionId, emitted: Emitted) -> Result<(), ApiError> {
        if emitted.events.is_empty() {
            return Ok(());
        }
        let owner = crate::sessions::owner(db, session).await?;
        self.streams
            .publish(
                owner,
                &PublishEvents {
                    session,
                    events: emitted.events,
                },
            )
            .await
    }

    /// Reads a page of a session's event tail, for a browser catching up.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Room`] if the room could not be reached or its
    /// answer was not a page.
    pub async fn events(&self, session: SessionId, after: u64) -> Result<EventPage, ApiError> {
        self.rooms
            .get_json(
                session,
                &format!("/internal/events?after={after}"),
                "an event page",
            )
            .await
    }

    /// Opens a browser's desktop stream against a session's room.
    ///
    /// The response is the room's SSE stream of encoded chunks, still
    /// running; the Worker's route hands it to the client verbatim. The
    /// room mints the caller's watcher lease as part of answering — the
    /// stream staying open is what keeps the daemon encoding.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Room`] if the room could not be reached or
    /// refused the stream.
    pub async fn desktop_stream(&self, session: SessionId) -> Result<skyzen::Response, ApiError> {
        self.rooms
            .call_streaming(session, "/internal/desktop/watch", "a desktop stream")
            .await
    }

    /// Takes or releases a session's screen on behalf of one watcher.
    ///
    /// The room decides the flip — whether anyone is driving after this
    /// call — and answers with the events it produced, which are
    /// published to the owner's stream like any command's.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::DesktopWatcherGone`] if the watcher lease has
    /// lapsed or never was, or [`ApiError::Room`] if the room could not
    /// be reached.
    pub async fn desktop_takeover(
        &self,
        db: &Db,
        session: SessionId,
        request: &DesktopTakeoverRequest,
    ) -> Result<(), ApiError> {
        let emitted = self
            .rooms
            .post_json_for(
                session,
                "/internal/desktop/takeover",
                "a desktop takeover",
                request,
            )
            .await?;
        self.fan_out(db, session, emitted).await
    }

    /// Sends one batch of a driving watcher's desktop input.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::DesktopWatcherGone`] if the watcher lease has
    /// lapsed, [`ApiError::DesktopTakeoverRequired`] if it is not driving
    /// the screen, [`ApiError::SessionDaemonOffline`] if no daemon is
    /// attached, or [`ApiError::Room`] if the room could not be reached.
    pub async fn desktop_input(
        &self,
        session: SessionId,
        request: &DesktopInputRequest,
    ) -> Result<(), ApiError> {
        self.rooms
            .post_json(
                session,
                "/internal/desktop/input",
                "a desktop input batch",
                request,
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
            .rooms
            .call(session, Verb::Get, "/internal/repo-status", None)
            .await?;
        if status == StatusCode::NOT_FOUND {
            return Err(ApiError::RepoStatusUnknown);
        }
        if !status.is_success() {
            return Err(refused(status, &body, "a working-tree read"));
        }
        serde_json::from_slice(&body)
            .map_err(|error| ApiError::Room(format!("the room returned no working tree: {error}")))
    }
}

impl Rooms {
    /// Asks a session's daemon something about its checkout, and waits for
    /// the answer.
    ///
    /// # Why the wait lives inside the room
    ///
    /// skyzen rebuilds a Durable Object around every event, so the room
    /// has no in-memory rendezvous: the `fetch` that forwards the question
    /// and the `frames` POST that carries the reply are two activations
    /// with no memory between them. What they share is the room's storage,
    /// so the room holds the question's request open and the answer stream
    /// polls the reply table inside it — the same shape every stream in
    /// the room already takes. The Worker makes exactly one room call per
    /// browser request, where coming back for the answer every 120 ms
    /// cost it up to a hundred.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::SessionDaemonOffline`] if no daemon is connected
    /// to read the checkout, [`ApiError::WorkdirTimeout`] if one is but did
    /// not answer before the room's deadline, or [`ApiError::Room`] if the
    /// room could not be reached.
    pub async fn inspect_workdir(
        &self,
        session: SessionId,
        request: WorkdirRequest,
    ) -> Result<WorkdirReply, ApiError> {
        let id = WorkdirRequestId::generate();
        let command = ControlToDaemon::InspectWorkdir { id, request };
        let encoded = serde_json::to_vec(&command)
            .map_err(|_| ApiError::CorruptRecord("a workdir question failed to encode"))?;

        let response = self
            .rooms
            .call_raw(session, Verb::Post, "/internal/workdir", Some(encoded))
            .await?;
        let status = response.status();
        if status == StatusCode::SERVICE_UNAVAILABLE {
            return Err(ApiError::SessionDaemonOffline);
        }
        if !status.is_success() {
            let body = response
                .into_body()
                .into_bytes()
                .await
                .map_err(|error| ApiError::Room(error.to_string()))?;
            return Err(refused(status, &body, "a workdir question"));
        }

        // The answer is the stream's one terminal event: `reply` carrying
        // it, or `timeout` when the room's own deadline passed. Heartbeat
        // comments are skipped over.
        let mut events = response.into_body().into_sse();
        loop {
            let Some(item) = events.next().await else {
                return Err(ApiError::Room(
                    "a workdir answer stream ended without an answer".to_owned(),
                ));
            };
            let event = item.map_err(|error| {
                ApiError::Room(format!("a workdir answer could not be read: {error}"))
            })?;
            match event.event() {
                Some("reply") => {
                    return event.data::<WorkdirReply>().map_err(|error| {
                        ApiError::Room(format!("a workdir answer did not parse: {error}"))
                    });
                }
                Some("timeout") => {
                    tracing::warn!(%session, %id, "a daemon did not answer a workdir question in time");
                    return Err(ApiError::WorkdirTimeout);
                }
                _ => {}
            }
        }
    }
}

impl HostRooms {
    /// Sends one command down a host's command stream, or holds it until
    /// the machine is back.
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
    /// The room is the authority here and D1 is not: an attachment that
    /// fell silent is a fact only the Durable Object holding its marker
    /// can see, and a Durable Object can reach neither D1 nor the Worker's
    /// KV. So the state on the `hosts` row is what the control plane last
    /// *recorded*, and this is what it is refreshed from every time the
    /// Worker looks at a host — see [`crate::hosts::refresh`].
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Room`] if the room could not be reached or its
    /// answer was not a status.
    pub async fn status(&self, host: HostId) -> Result<HostStatus, ApiError> {
        self.get_json(host, "/internal/status", "a host status")
            .await
    }

    /// Attaches a machine to its host room.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Room`] if the room could not be reached or
    /// refused the attach.
    pub async fn attach(
        &self,
        host: HostId,
        attach: &HostAttach,
    ) -> Result<HostAttachResponse, ApiError> {
        self.post_json_for(host, "/internal/attach", "a host attach", attach)
            .await
    }

    /// Posts one batch of a host's outbound frames to its room.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Room`] if the room could not be reached or
    /// refused the batch — including a stale epoch or a sequence gap.
    pub async fn frames(&self, host: HostId, batch: &HostFrames) -> Result<(), ApiError> {
        self.post_json(host, "/internal/frames", "a host frame batch", batch)
            .await
    }

    /// Opens a host's command stream against its room.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Room`] if the room could not be reached or
    /// refused the stream — including a stale epoch.
    pub async fn commands(&self, host: HostId, epoch: u64) -> Result<skyzen::Response, ApiError> {
        self.call_streaming(
            host,
            &format!("/internal/commands?epoch={epoch}"),
            "a host command stream",
        )
        .await
    }
}

/// The per-user event stream, seen from the Worker.
pub type UserStreams = RoomsOf<Users>;

impl UserStreams {
    /// Appends the events one room call produced to a user's stream.
    ///
    /// The fan-in half of the room contract: every session room answers
    /// its internal routes with the events the call made, and this is
    /// where they go to reach the browser holding `GET /v1/events`.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Room`] if the stream could not be reached or
    /// refused the events.
    pub async fn publish(&self, user: UserId, body: &PublishEvents) -> Result<(), ApiError> {
        self.post_json(user, "/internal/publish", "a publish", body)
            .await
    }

    /// Opens a user's event stream.
    ///
    /// The response is the SSE stream, still running. `after` resumes
    /// strictly past a position in the buffer — the `Last-Event-ID` a
    /// reconnecting client carried; `None` opens the stream live — and
    /// `session` narrows the stream to one session when the caller is
    /// following one.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Room`] if the stream could not be reached.
    pub async fn stream(
        &self,
        user: UserId,
        after: Option<u64>,
        session: Option<SessionId>,
    ) -> Result<skyzen::Response, ApiError> {
        let mut path = "/internal/stream".to_owned();
        let mut separator = '?';
        if let Some(after) = after {
            let _ = write!(path, "{separator}after={after}");
            separator = '&';
        }
        if let Some(session) = session {
            let _ = write!(path, "{separator}session={session}");
        }
        self.call_streaming(user, &path, "the event stream").await
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
