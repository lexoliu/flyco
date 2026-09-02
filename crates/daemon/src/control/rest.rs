//! The daemon's REST client for its own session.
//!
//! Five things a daemon does over ordinary HTTP rather than over the relay
//! socket, each for its own reason:
//!
//! * **The `Cloning` stage** happens *before* the harness exists, and the
//!   relay socket is not opened until there is a session behind it. A
//!   checkout is what the harness is started in, so the one milestone that
//!   cannot ride the relay is the one announcing it.
//! * **Approvals** must be *durable* before they are announced. The control
//!   plane assigns the id, so a decision routed back through the relay names
//!   an approval the API can actually settle. A relay frame is live state;
//!   a pending approval outlives every socket involved.
//! * **Transcript batches** are unbounded and a Cloudflare WebSocket frame
//!   caps at 1 MiB, so bulk data never rides the relay.
//! * **Reading a transcript back** happens at resume, which is *before*
//!   there is a session to relay through.
//! * **Usage observations** are what the LLM usage panel is made of, and no
//!   vendor publishes the numbers to read instead. They belong in D1 rather
//!   than in a room's event log, which is per-session and lives as long as
//!   the session does.
//! * **A spot notice** has to reach the *Worker*: the room is a Durable
//!   Object, and marking the session interrupted and queuing its
//!   replacement need D1 and a queue, neither of which a Durable Object can
//!   touch. The relay frame beside it is what puts the countdown in front
//!   of the user; this is what survives the machine.
//!
//! Every call carries the session's `fd_` daemon token, which authorizes
//! exactly this session's daemon-scoped routes.

use core::future::Future;

use flyco_core::wire::ApprovalPayload;
use flyco_core::{
    AgentMachineView, ApprovalId, ApprovalView, BudgetView, HarnessObservation, HarnessSessionView,
    MachineCatalogEntry, Problem, ProvisioningStage, ReportProvisioningStage, ReportSpotNotice,
    ResizeMachine, SessionId,
};
use url::Url;
use zenwave::{Client as _, ResponseExt as _};

/// Header the transcript read reports its batch count in.
const BATCH_COUNT_HEADER: &str = "x-flyco-transcript-batches";

/// A call to the control plane did not produce what the daemon needed.
#[derive(Debug, thiserror::Error)]
pub enum ControlApiError {
    /// The request never completed, or the response was not what it claimed.
    #[error("control-plane request failed: {0}")]
    Transport(String),
    /// The control plane answered with an RFC 9457 problem document.
    #[error("the control plane refused {method} {path}: {status} {title} — {detail}")]
    Refused {
        /// Method that was refused.
        method: &'static str,
        /// Path that was refused.
        path: String,
        /// Status code it carried.
        status: u16,
        /// The problem type's title.
        title: String,
        /// What the control plane said about this occurrence.
        detail: String,
    },
    /// The control plane answered with a status but no problem document.
    #[error("the control plane answered {method} {path} with HTTP {status}")]
    Status {
        /// Method that failed.
        method: &'static str,
        /// Path that failed.
        path: String,
        /// Status code it carried.
        status: u16,
    },
    /// The configured control-plane URL cannot address a route.
    #[error("the control-plane URL cannot address `{0}`")]
    Unaddressable(String),
}

fn transport(error: impl core::fmt::Display) -> ControlApiError {
    ControlApiError::Transport(error.to_string())
}

/// Turns a zenwave failure into the richest error its body supports.
///
/// zenwave answers a non-2xx with `Error::Http`, having already read the
/// body — so the control plane's own RFC 9457 explanation is right there,
/// and a refusal reaches the daemon's log saying *why* rather than showing
/// a bare status line.
fn refused(method: &'static str, path: &str, error: &zenwave::Error) -> ControlApiError {
    let zenwave::Error::Http { status, .. } = error else {
        return ControlApiError::Transport(error.to_string());
    };
    error.deserialize_http_error::<Problem>().map_or_else(
        || ControlApiError::Status {
            method,
            path: path.to_owned(),
            status: status.as_u16(),
        },
        |problem| ControlApiError::Refused {
            method,
            path: path.to_owned(),
            status: status.as_u16(),
            title: problem.title,
            detail: problem.detail,
        },
    )
}

/// Putting a decision in front of the user.
///
/// Its own trait because two unrelated things need it and neither needs the
/// other's surface: the relay routes a harness tool call to flyco's approval
/// UI, and [the MCP server](crate::mcp) asks before spending the user's
/// money on a license-bound machine. A daemon-scoped `POST
/// /v1/sessions/{id}/approvals` in both cases.
pub trait ApprovalRaiser: Send + Sync + 'static {
    /// Records a pending approval and returns the id the user will decide.
    ///
    /// # Errors
    ///
    /// Returns [`ControlApiError`] if the control plane could not be
    /// reached or refused the approval.
    fn raise_approval(
        &self,
        payload: ApprovalPayload,
    ) -> impl Future<Output = Result<ApprovalId, ControlApiError>> + Send;
}

/// What the control plane offers a session's daemon.
///
/// A trait so the wire client can be driven without a control plane, and so
/// a test can assert on the *ordering* the relay depends on — the approval
/// row exists before the frame announcing it leaves.
pub trait ControlApi: ApprovalRaiser {
    /// Stores one batch of a transcript stream.
    ///
    /// # Errors
    ///
    /// Returns [`ControlApiError`] if the control plane could not be
    /// reached or refused the batch.
    fn put_transcript_batch(
        &self,
        stream: &str,
        seq: u64,
        body: Vec<u8>,
    ) -> impl Future<Output = Result<(), ControlApiError>> + Send;

    /// Reads a transcript stream back, with the number of batches it held.
    ///
    /// # Errors
    ///
    /// Returns [`ControlApiError`] if the control plane could not be
    /// reached or refused the read.
    fn get_transcript(
        &self,
        stream: &str,
    ) -> impl Future<Output = Result<TranscriptRead, ControlApiError>> + Send;

    /// Records one thing this session saw about the harness account driving
    /// it — a turn's reported cost, or the account hitting its limit.
    ///
    /// The control plane derives *which* account from the session, so there
    /// is no account identifier here for a daemon to get wrong.
    ///
    /// # Errors
    ///
    /// Returns [`ControlApiError`] if the control plane could not be
    /// reached or refused the observation — which it does when the session's
    /// user has linked no account for this harness, and there is therefore
    /// nothing to attribute it to.
    fn record_observation(
        &self,
        observation: HarnessObservation,
    ) -> impl Future<Output = Result<(), ControlApiError>> + Send;

    /// Announces that a turn began, so the control plane can record the
    /// session as working.
    ///
    /// The turn's start rides the relay too, but a Durable Object cannot
    /// reach the database the session list is built from — so the fact the
    /// list needs comes over REST, exactly as the turn's end does.
    fn notify_turn_started(&self) -> impl Future<Output = Result<(), ControlApiError>> + Send;

    /// Announces that a turn completed so the owner can be notified.
    fn notify_turn_completed(&self) -> impl Future<Output = Result<(), ControlApiError>> + Send;

    /// Announces that a turn failed so the owner can be notified.
    fn notify_turn_failed(&self) -> impl Future<Output = Result<(), ControlApiError>> + Send;

    /// Records the harness-native session id so a later resume continues it.
    ///
    /// # Errors
    ///
    /// Returns [`ControlApiError`] if the control plane could not be
    /// reached or refused the write.
    fn record_harness_session(
        &self,
        harness_session_id: &str,
    ) -> impl Future<Output = Result<(), ControlApiError>> + Send;

    /// Stores a binary diff of uncommitted work, taken just before an
    /// automatic archive releases the disk.
    fn put_workdir_patch(
        &self,
        patch: Vec<u8>,
    ) -> impl Future<Output = Result<(), ControlApiError>> + Send;

    /// Reads a previously stored workdir patch, if an automatic archive
    /// left one.
    fn get_workdir_patch(
        &self,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, ControlApiError>> + Send;

    /// Reads the conversation a daemon starting on this session must
    /// continue.
    ///
    /// The authority on it, and the configuration on the disk is not: that
    /// file was written when the machine was created, and a machine that
    /// was stopped and started again on the same disk — a spot reclamation
    /// recovered from — boots the same file. A daemon that trusted it would
    /// open a second conversation beside the one the user is watching.
    ///
    /// # Errors
    ///
    /// Returns [`ControlApiError`] if the control plane could not be
    /// reached or refused the read.
    fn harness_session_id(
        &self,
    ) -> impl Future<Output = Result<Option<String>, ControlApiError>> + Send;

    /// Reports that this machine's capacity is being reclaimed.
    ///
    /// The durable half of a spot notice, and the reason it is a REST call
    /// rather than only the relay frame beside it: a session room is a
    /// Durable Object and can reach neither D1 nor the provisioning queue,
    /// so marking the session interrupted and queuing its replacement has
    /// to arrive at the Worker over HTTP. Awaited, because the daemon is
    /// about to stop existing and "the control plane knows" is the one
    /// thing that has to be true before it does.
    ///
    /// # Errors
    ///
    /// Returns [`ControlApiError`] if the control plane could not be
    /// reached or refused the report.
    fn report_spot_notice(
        &self,
        seconds_remaining: u32,
    ) -> impl Future<Output = Result<(), ControlApiError>> + Send;

    /// Announces a provisioning milestone the machine has reached.
    ///
    /// The control plane times it, so a session VM whose clock is wrong
    /// cannot put a line of the timeline in 1970.
    ///
    /// # Errors
    ///
    /// Returns [`ControlApiError`] if the control plane could not be
    /// reached or refused the report.
    fn report_stage(
        &self,
        stage: ProvisioningStage,
    ) -> impl Future<Output = Result<(), ControlApiError>> + Send;
}

/// What the agent's own tools ask the control plane.
///
/// Separate from [`ControlApi`] because a different process asks: the relay
/// supervises a harness and never wants to know what a machine costs, and
/// [`the MCP server`](crate::mcp) answers tool calls and never writes a
/// transcript. Keeping them apart means the relay's test doubles do not have
/// to invent answers about machines they will never be asked for.
pub trait AgentApi: ApprovalRaiser {
    /// Reads the machine this session is on, and who chose it.
    ///
    /// Asked rather than remembered. A resize restarts the machine without
    /// rewriting the configuration on its disk, so the file describes the
    /// machine the session *booted* on and this describes the one it is on.
    ///
    /// # Errors
    ///
    /// Returns [`ControlApiError`] if the control plane could not be
    /// reached or the session has no machine.
    fn agent_machine(
        &self,
    ) -> impl Future<Output = Result<AgentMachineView, ControlApiError>> + Send;

    /// Reads the machine types this session can be resized to.
    ///
    /// The curated catalog of docs/ux.md §7.6, already narrowed to the
    /// account and region the session's disk lives in — the two a resize
    /// cannot cross — so every entry is a machine this one can actually
    /// become.
    ///
    /// # Errors
    ///
    /// Returns [`ControlApiError`] if the control plane could not be
    /// reached or a provider catalog could not be read.
    fn agent_machine_catalog(
        &self,
    ) -> impl Future<Output = Result<Vec<MachineCatalogEntry>, ControlApiError>> + Send;

    /// Moves this session onto another machine type.
    ///
    /// # Errors
    ///
    /// Returns [`ControlApiError`] if the control plane could not be
    /// reached, the type is not one this session can move to, or the type
    /// bills a minimum on boot — which flyco refuses on a daemon's
    /// authority and which the caller should have raised an approval for.
    fn resize_machine(
        &self,
        machine_type: &str,
    ) -> impl Future<Output = Result<(), ControlApiError>> + Send;

    /// Reads this session's compute budget.
    ///
    /// # Errors
    ///
    /// Returns [`ControlApiError`] if the control plane could not be
    /// reached.
    fn agent_budget(&self) -> impl Future<Output = Result<BudgetView, ControlApiError>> + Send;
}

/// A transcript stream as the control plane serves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptRead {
    /// Every batch concatenated, in sequence order.
    pub body: Vec<u8>,
    /// How many batches were concatenated.
    ///
    /// The resuming daemon numbers its next batch from here, so a session
    /// that moves host does not restart at zero and collide with what the
    /// previous host already wrote.
    pub batches: u64,
}

/// The production [`ControlApi`], speaking HTTP through zenwave.
#[derive(Clone)]
pub struct HttpControlApi {
    base: Url,
    session: SessionId,
    token: String,
}

impl core::fmt::Debug for HttpControlApi {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HttpControlApi")
            .field("base", &self.base.as_str())
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl HttpControlApi {
    /// Builds a client for one session.
    #[must_use]
    pub const fn new(base: Url, session: SessionId, token: String) -> Self {
        Self {
            base,
            session,
            token,
        }
    }

    /// Resolves a path under `/v1/sessions/{session}/` against the base URL.
    fn url(&self, suffix: &str) -> Result<String, ControlApiError> {
        let path = format!("v1/sessions/{}/{suffix}", self.session);
        self.base
            .join(&path)
            .map(|url| url.to_string())
            .map_err(|_| ControlApiError::Unaddressable(path))
    }
}

impl ApprovalRaiser for HttpControlApi {
    async fn raise_approval(
        &self,
        payload: ApprovalPayload,
    ) -> Result<ApprovalId, ControlApiError> {
        let url = self.url("approvals")?;
        let mut client = zenwave::client();
        let response = client
            .post(&url)
            .map_err(transport)?
            .bearer_auth(self.token.clone())
            .json_body(&payload)
            .map_err(transport)?
            .await
            .map_err(|error| refused("POST", &url, &error))?;

        response
            .into_json::<ApprovalView>()
            .await
            .map(|view| view.id)
            .map_err(transport)
    }
}

impl ControlApi for HttpControlApi {
    async fn put_transcript_batch(
        &self,
        stream: &str,
        seq: u64,
        body: Vec<u8>,
    ) -> Result<(), ControlApiError> {
        let url = self.url(&format!("transcript/{stream}/batches/{seq}"))?;
        let mut client = zenwave::client();
        let response = client
            .put(&url)
            .map_err(transport)?
            .bearer_auth(self.token.clone())
            .header("Content-Type", "application/x-ndjson")
            .map_err(transport)?
            .bytes_body(body)
            .await
            .map_err(|error| refused("PUT", &url, &error))?;

        debug_assert!(response.status().is_success());
        Ok(())
    }

    async fn record_observation(
        &self,
        observation: HarnessObservation,
    ) -> Result<(), ControlApiError> {
        let url = self.url("harness-observations")?;
        let mut client = zenwave::client();
        let response = client
            .post(&url)
            .map_err(transport)?
            .bearer_auth(self.token.clone())
            .json_body(&observation)
            .map_err(transport)?
            .await
            .map_err(|error| refused("POST", &url, &error))?;

        debug_assert!(response.status().is_success());
        Ok(())
    }

    async fn harness_session_id(&self) -> Result<Option<String>, ControlApiError> {
        self.get_json::<HarnessSessionView>("harness-session")
            .await
            .map(|view| view.harness_session_id)
    }

    async fn report_spot_notice(&self, seconds_remaining: u32) -> Result<(), ControlApiError> {
        let url = self.url("spot-notice")?;
        let mut client = zenwave::client();
        let response = client
            .post(&url)
            .map_err(transport)?
            .bearer_auth(self.token.clone())
            .json_body(&ReportSpotNotice { seconds_remaining })
            .map_err(transport)?
            .await
            .map_err(|error| refused("POST", &url, &error))?;

        debug_assert!(response.status().is_success());
        Ok(())
    }

    async fn report_stage(&self, stage: ProvisioningStage) -> Result<(), ControlApiError> {
        let url = self.url("provisioning-stage")?;
        let mut client = zenwave::client();
        let response = client
            .post(&url)
            .map_err(transport)?
            .bearer_auth(self.token.clone())
            .json_body(&ReportProvisioningStage { stage })
            .map_err(transport)?
            .await
            .map_err(|error| refused("POST", &url, &error))?;

        debug_assert!(response.status().is_success());
        Ok(())
    }

    async fn notify_turn_started(&self) -> Result<(), ControlApiError> {
        self.post_empty("turn-started").await
    }

    async fn notify_turn_completed(&self) -> Result<(), ControlApiError> {
        self.post_empty("turn-completed").await
    }

    async fn notify_turn_failed(&self) -> Result<(), ControlApiError> {
        self.post_empty("turn-failed").await
    }

    async fn record_harness_session(
        &self,
        harness_session_id: &str,
    ) -> Result<(), ControlApiError> {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            harness_session_id: &'a str,
        }

        let url = self.url("harness-session")?;
        let mut client = zenwave::client();
        let response = client
            .put(&url)
            .map_err(transport)?
            .bearer_auth(self.token.clone())
            .json_body(&Body { harness_session_id })
            .map_err(transport)?
            .await
            .map_err(|error| refused("PUT", &url, &error))?;

        debug_assert!(response.status().is_success());
        Ok(())
    }

    async fn put_workdir_patch(&self, patch: Vec<u8>) -> Result<(), ControlApiError> {
        let url = self.url("workdir-patch")?;
        let mut client = zenwave::client();
        let response = client
            .put(&url)
            .map_err(transport)?
            .bearer_auth(self.token.clone())
            .header("Content-Type", "application/octet-stream")
            .map_err(transport)?
            .bytes_body(patch)
            .await
            .map_err(|error| refused("PUT", &url, &error))?;

        debug_assert!(response.status().is_success());
        Ok(())
    }

    async fn get_workdir_patch(&self) -> Result<Option<Vec<u8>>, ControlApiError> {
        let url = self.url("workdir-patch")?;
        let mut client = zenwave::client();
        let response = match client
            .get(&url)
            .map_err(transport)?
            .bearer_auth(self.token.clone())
            .await
        {
            Ok(response) => response,
            Err(error) => {
                if let zenwave::Error::Http { status, .. } = &error
                    && status.as_u16() == 404
                {
                    return Ok(None);
                }
                return Err(refused("GET", &url, &error));
            }
        };
        let body = response.into_bytes().await.map_err(transport)?;
        Ok(Some(body.to_vec()))
    }

    async fn get_transcript(&self, stream: &str) -> Result<TranscriptRead, ControlApiError> {
        let url = self.url(&format!("transcript/{stream}"))?;
        let mut client = zenwave::client();
        let response = client
            .get(&url)
            .map_err(transport)?
            .bearer_auth(self.token.clone())
            .await
            .map_err(|error| refused("GET", &url, &error))?;

        let batches = response
            .headers()
            .get(BATCH_COUNT_HEADER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| {
                ControlApiError::Transport(format!(
                    "a transcript read carried no `{BATCH_COUNT_HEADER}`"
                ))
            })?;

        let body = response.into_bytes().await.map_err(transport)?;
        Ok(TranscriptRead {
            body: body.to_vec(),
            batches,
        })
    }
}

impl AgentApi for HttpControlApi {
    async fn agent_machine(&self) -> Result<AgentMachineView, ControlApiError> {
        self.get_json("agent/machine").await
    }

    async fn agent_machine_catalog(&self) -> Result<Vec<MachineCatalogEntry>, ControlApiError> {
        self.get_json("agent/machine/catalog").await
    }

    async fn resize_machine(&self, machine_type: &str) -> Result<(), ControlApiError> {
        let url = self.url("agent/machine/resize")?;
        let mut client = zenwave::client();
        let response = client
            .post(&url)
            .map_err(transport)?
            .bearer_auth(self.token.clone())
            .json_body(&ResizeMachine {
                machine_type: machine_type.to_owned(),
            })
            .map_err(transport)?
            .await
            .map_err(|error| refused("POST", &url, &error))?;

        debug_assert!(response.status().is_success());
        Ok(())
    }

    async fn agent_budget(&self) -> Result<BudgetView, ControlApiError> {
        self.get_json("agent/budget").await
    }
}

impl HttpControlApi {
    /// Reads one JSON document from a daemon-scoped route.
    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        suffix: &str,
    ) -> Result<T, ControlApiError> {
        let url = self.url(suffix)?;
        let mut client = zenwave::client();
        client
            .get(&url)
            .map_err(transport)?
            .bearer_auth(self.token.clone())
            .await
            .map_err(|error| refused("GET", &url, &error))?
            .into_json::<T>()
            .await
            .map_err(transport)
    }

    async fn post_empty(&self, suffix: &str) -> Result<(), ControlApiError> {
        let url = self.url(suffix)?;
        let mut client = zenwave::client();
        client
            .post(&url)
            .map_err(transport)?
            .bearer_auth(self.token.clone())
            .await
            .map_err(|error| refused("POST", &url, &error))?;
        Ok(())
    }
}
