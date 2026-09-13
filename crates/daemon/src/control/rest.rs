//! The daemon's REST client for its own session.
//!
//! Some things a daemon does over ordinary HTTP rather than over the relay
//! frame POST, each for its own reason:
//!
//! * **The `Cloning` stage** happens *before* the harness exists, and the
//!   daemon does not attach until there is a session behind it. A
//!   checkout is what the harness is started in, so the one milestone that
//!   cannot ride the relay is the one announcing it.
//! * **Approvals** must be *durable* before they are announced. The control
//!   plane assigns the id, so a decision routed back through the relay names
//!   an approval the API can actually settle. A relay frame is live state;
//!   a pending approval outlives every attachment involved.
//! * **Transcript batches** are unbounded and the relay's frame batches
//!   are sequenced envelopes, so bulk data never rides the relay.
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
use core::time::Duration;

use flyco_core::wire::{
    ApprovalPayload, DaemonAttach, DaemonAttached, DaemonCommand, DaemonFrames,
};
use flyco_core::{
    AgentMachineView, ApprovalId, ApprovalView, BudgetView, HarnessObservation, HarnessSessionView,
    MachineCatalogEntry, ModelOption, Problem, ProvisioningStage, ReportProvisioningStage,
    ReportSpotNotice, ReportStartupFailure, ReportStopping, ResizeMachine, SessionId, StopReason,
    UsageWindow,
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
        /// The problem type's slug — the last segment of its `type` URI.
        ///
        /// Kept because some refusals are instructions the caller acts on:
        /// `relay-epoch-stale` means re-attach, `protocol-mismatch` means
        /// stop trying. Parsing the detail prose would couple a client to
        /// English sentences.
        kind: String,
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

/// A request that never completed, whoever was making it.
pub(crate) fn transport(error: impl core::fmt::Display) -> ControlApiError {
    ControlApiError::Transport(error.to_string())
}

/// Turns a zenwave failure into the richest error its body supports.
///
/// zenwave answers a non-2xx with `Error::Http`, having already read the
/// body — so the control plane's own RFC 9457 explanation is right there,
/// and a refusal reaches the daemon's log saying *why* rather than showing
/// a bare status line.
pub(crate) fn refused(method: &'static str, path: &str, error: &zenwave::Error) -> ControlApiError {
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
            kind: problem.kind,
        },
    )
}

impl ControlApiError {
    /// The problem type's slug, when this failure is a typed refusal.
    ///
    /// A refusal's slug is the machine-readable half of the document: the
    /// relay distinguishes `protocol-mismatch` (fatal — this build cannot
    /// serve) from `relay-epoch-stale` (retry — attach again) by it rather
    /// than by the status both carry.
    #[must_use]
    pub fn kind(&self) -> Option<&str> {
        let Self::Refused { kind, .. } = self else {
            return None;
        };
        Some(kind.rsplit('/').next().unwrap_or(kind))
    }
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
    /// continue, and the model it must continue it on.
    ///
    /// The authority on both, and the configuration on the disk is not:
    /// that file was written when the machine was created, and a machine
    /// that was stopped and started again on the same disk — a spot
    /// reclamation recovered from — boots the same file. A daemon that
    /// trusted it would open a second conversation beside the one the user
    /// is watching, and would open it on the model the session had before
    /// the user changed it.
    ///
    /// # Errors
    ///
    /// Returns [`ControlApiError`] if the control plane could not be
    /// reached or refused the read.
    fn harness_session(
        &self,
    ) -> impl Future<Output = Result<HarnessSessionView, ControlApiError>> + Send;

    /// Reports the models this session's harness offers.
    ///
    /// Filed once, after the harness's handshake and before any turn. The
    /// control plane records the list against the account the machine was
    /// provisioned through, so the next session's picker opens on what this
    /// harness build actually accepts rather than on the list flyco shipped
    /// with.
    ///
    /// # Errors
    ///
    /// Returns [`ControlApiError`] if the control plane could not be
    /// reached or refused the report.
    fn report_models(
        &self,
        models: &[ModelOption],
    ) -> impl Future<Output = Result<(), ControlApiError>> + Send;

    /// Reports how much of this session's harness plan is spent.
    ///
    /// Filed at session start and after every turn. The control plane
    /// records the snapshot against the account the machine was
    /// provisioned through and announces it to the session's browsers, so
    /// the composer's rings and the Settings bars read the same numbers.
    ///
    /// # Errors
    ///
    /// Returns [`ControlApiError`] if the control plane could not be
    /// reached or refused the report.
    fn report_usage(
        &self,
        windows: &[UsageWindow],
    ) -> impl Future<Output = Result<(), ControlApiError>> + Send;

    /// Reports that this session's harness has run out of plan.
    ///
    /// Filed once per limit, and the one report that stops the session rather
    /// than describing it: the control plane releases the machine so the wait
    /// costs nothing, starts it again ten minutes before the window turns
    /// over, and picks the conversation back up (issue #244).
    ///
    /// Only for a window that names a reset. A limit flyco cannot place in
    /// time is nothing a pause can be scheduled around — the control plane
    /// refuses it — so the daemon does not make the call: the limit is in the
    /// conversation either way, on the relay frame beside this.
    ///
    /// # Errors
    ///
    /// Returns [`ControlApiError`] if the control plane could not be
    /// reached or refused the report.
    fn report_usage_limit(
        &self,
        window: &UsageWindow,
    ) -> impl Future<Output = Result<(), ControlApiError>> + Send;

    /// Reports why this daemon is stopping before it could report in.
    ///
    /// The last thing a dying `flycod` does. It is restarted on failure, so
    /// this is not a verdict — it is the sentence the session says if the
    /// machine never does come up, instead of the control plane guessing
    /// from silence.
    ///
    /// # Errors
    ///
    /// Returns [`ControlApiError`] if the control plane could not be
    /// reached or refused the report.
    fn report_startup_failure(
        &self,
        message: String,
    ) -> impl Future<Output = Result<(), ControlApiError>> + Send;

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

    /// Reports that this machine is stopping and its filesystem is going
    /// with it.
    ///
    /// The container counterpart of [`report_spot_notice`](Self::report_spot_notice),
    /// and a separate route for a reason rather than a flag on that one: a
    /// reclaimed virtual machine keeps its disk and is recovered onto it
    /// after the provider's countdown, while a stopping container has
    /// already handed its working tree over as the `workdir-patch` and
    /// there is nothing to schedule against a deadline.
    ///
    /// Filed *after* the patch and awaited, in that order, because it is the
    /// sentence that makes the stop true for everyone else and it must not
    /// be true before the work is safe.
    ///
    /// # Errors
    ///
    /// Returns [`ControlApiError`] if the control plane could not be
    /// reached or refused the report.
    fn report_stopping(
        &self,
        reason: StopReason,
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

    async fn harness_session(&self) -> Result<HarnessSessionView, ControlApiError> {
        self.get_json("harness-session").await
    }

    async fn report_usage(&self, windows: &[UsageWindow]) -> Result<(), ControlApiError> {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            windows: &'a [UsageWindow],
        }

        let url = self.url("usage")?;
        let mut client = zenwave::client();
        let response = client
            .put(&url)
            .map_err(transport)?
            .bearer_auth(self.token.clone())
            .json_body(&Body { windows })
            .map_err(transport)?
            .await
            .map_err(|error| refused("PUT", &url, &error))?;

        debug_assert!(response.status().is_success());
        Ok(())
    }

    async fn report_usage_limit(&self, window: &UsageWindow) -> Result<(), ControlApiError> {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            window: &'a UsageWindow,
        }

        let url = self.url("usage-limit")?;
        let mut client = zenwave::client();
        let response = client
            .post(&url)
            .map_err(transport)?
            .bearer_auth(self.token.clone())
            .json_body(&Body { window })
            .map_err(transport)?
            .await
            .map_err(|error| refused("POST", &url, &error))?;

        debug_assert!(response.status().is_success());
        Ok(())
    }

    async fn report_models(&self, models: &[ModelOption]) -> Result<(), ControlApiError> {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            models: &'a [ModelOption],
        }

        let url = self.url("models")?;
        let mut client = zenwave::client();
        let response = client
            .put(&url)
            .map_err(transport)?
            .bearer_auth(self.token.clone())
            .json_body(&Body { models })
            .map_err(transport)?
            .await
            .map_err(|error| refused("PUT", &url, &error))?;

        debug_assert!(response.status().is_success());
        Ok(())
    }

    async fn report_startup_failure(&self, message: String) -> Result<(), ControlApiError> {
        let url = self.url("startup-failure")?;
        let mut client = zenwave::client();
        let response = client
            .post(&url)
            .map_err(transport)?
            .bearer_auth(self.token.clone())
            .json_body(&ReportStartupFailure { message })
            .map_err(transport)?
            .await
            .map_err(|error| refused("POST", &url, &error))?;

        debug_assert!(response.status().is_success());
        Ok(())
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

    async fn report_stopping(&self, reason: StopReason) -> Result<(), ControlApiError> {
        let url = self.url("stopping")?;
        let mut client = zenwave::client();
        let response = client
            .post(&url)
            .map_err(transport)?
            .bearer_auth(self.token.clone())
            .json_body(&ReportStopping { reason })
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

/// The relay half of a daemon's control-plane client.
///
/// Separate from [`ControlApi`]'s durable reports because the failure
/// semantics differ: a refused report is a fact lost, while a dropped
/// command stream is only a reconnect — and because the commands route
/// answers with a *stream*, which nothing else on the API does.
///
/// The room's commands are sequenced rows the stream cursors over; a
/// daemon acknowledges what it applied on its next frames POST. A dead
/// stream therefore loses nothing: re-attaching replays every
/// unacknowledged command.
pub trait RelayTransport: Send + Sync + 'static {
    /// Attaches this daemon to its session's room.
    ///
    /// The returned epoch names the attachment: every later frames POST
    /// and the command stream it opens carry it, so a daemon that attached
    /// twice and a room that watched the first stream die agree about
    /// which attachment the traffic belongs to.
    ///
    /// # Errors
    ///
    /// Returns [`ControlApiError`] if the control plane could not be
    /// reached, refused the attach (a wrong token, a protocol version it
    /// does not speak), or the room failed.
    fn attach(&self) -> impl Future<Output = Result<DaemonAttached, ControlApiError>> + Send;

    /// Opens the command stream belonging to one attach epoch.
    ///
    /// `idle` is how long the stream may deliver no bytes at all before
    /// it is treated as a dead path: the room heartbeats well inside any
    /// sane bound, so a gap that long is a flow a NAT reclaimed rather
    /// than a room with nothing to say.
    ///
    /// # Errors
    ///
    /// Returns [`ControlApiError`] if the stream could not be opened —
    /// including `relay-epoch-stale`, when the epoch names a superseded
    /// attach.
    fn commands(
        &self,
        epoch: u64,
        idle: Duration,
    ) -> impl Future<Output = Result<CommandStream<DaemonCommand>, ControlApiError>> + Send;

    /// Posts one sequenced batch of outbound frames.
    ///
    /// `ack_through` rides every batch, so a batch of no frames at all is
    /// how a daemon acknowledges commands while it has nothing to say.
    ///
    /// # Errors
    ///
    /// Returns [`ControlApiError`] if the batch was not stored —
    /// `relay-epoch-stale` or `relay-frames-gap` among the refusals, both
    /// of which the daemon answers by re-attaching and re-sending what is
    /// still unconfirmed.
    fn frames(
        &self,
        batch: &DaemonFrames,
    ) -> impl Future<Output = Result<(), ControlApiError>> + Send;
}

/// A room's command stream, decoded.
///
/// Concrete rather than `impl Stream` at the trait boundary so test
/// doubles can fabricate one from a channel — the transport is the same
/// shape whoever produced it. Generic over the command envelope because
/// a session daemon and an enrolled host read different rooms with the
/// same machinery.
pub struct CommandStream<T> {
    inner:
        core::pin::Pin<Box<dyn futures_core::Stream<Item = Result<T, ControlApiError>> + Send>>,
}

impl<T> core::fmt::Debug for CommandStream<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CommandStream").finish_non_exhaustive()
    }
}

impl<T> CommandStream<T> {
    /// Wraps any stream of decoded commands.
    pub fn new<S>(stream: S) -> Self
    where
        S: futures_core::Stream<Item = Result<T, ControlApiError>> + Send + 'static,
    {
        Self {
            inner: Box::pin(stream),
        }
    }
}

impl<T> futures_core::Stream for CommandStream<T> {
    type Item = Result<T, ControlApiError>;

    fn poll_next(
        mut self: core::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl<T> Unpin for CommandStream<T> {}

/// Parses a streaming response body into decoded commands.
///
/// The idle timeout applies to *bytes*, not events: the room's heartbeat
/// is a comment line the SSE parser drops, and only a byte-level watch
/// sees that a quiet stream is still alive. One helper for both relay
/// clients — a session daemon's and an enrolled host's differ only in
/// the envelope they decode.
pub(crate) fn command_stream<T>(
    body: zenwave::Body,
    idle: Duration,
) -> CommandStream<T>
where
    T: serde::de::DeserializeOwned + Send + 'static,
{
    use eventsource_stream::Eventsource as _;
    use futures_util::StreamExt as _;

    let chunks = futures_util::stream::try_unfold(Box::pin(body), move |mut body| async move {
        match tokio::time::timeout(idle, body.next()).await {
            Ok(Some(Ok(bytes))) => Ok(Some((bytes, body))),
            Ok(Some(Err(error))) => Err(ControlApiError::Transport(error.to_string())),
            Ok(None) => Ok(None),
            Err(_elapsed) => Err(ControlApiError::Transport(format!(
                "the command stream went silent for {idle:?}"
            ))),
        }
    });
    let stream = chunks.eventsource().map(|event| match event {
        Ok(event) => serde_json::from_str::<T>(&event.data).map_err(|error| {
            ControlApiError::Transport(format!("a command could not be decoded: {error}"))
        }),
        Err(error) => Err(ControlApiError::Transport(error.to_string())),
    });
    CommandStream::new(stream)
}

impl RelayTransport for HttpControlApi {
    async fn attach(&self) -> Result<DaemonAttached, ControlApiError> {
        let url = self.url("relay/attach")?;
        let mut client = zenwave::client();
        let response = client
            .post(&url)
            .map_err(transport)?
            .bearer_auth(self.token.clone())
            .json_body(&DaemonAttach {
                protocol_version: flyco_core::WIRE_PROTOCOL_VERSION,
            })
            .map_err(transport)?
            .await
            .map_err(|error| refused("POST", &url, &error))?;

        response.into_json::<DaemonAttached>().await.map_err(transport)
    }

    async fn commands(
        &self,
        epoch: u64,
        idle: Duration,
    ) -> Result<CommandStream<DaemonCommand>, ControlApiError> {
        let url = self.url(&format!("relay/commands?epoch={epoch}"))?;
        let mut client = zenwave::client();
        let response = client
            .get(&url)
            .map_err(transport)?
            .bearer_auth(self.token.clone())
            .await
            .map_err(|error| refused("GET", &url, &error))?;

        Ok(command_stream(response.into_body(), idle))
    }

    async fn frames(&self, batch: &DaemonFrames) -> Result<(), ControlApiError> {
        let url = self.url("relay/frames")?;
        let mut client = zenwave::client();
        client
            .post(&url)
            .map_err(transport)?
            .bearer_auth(self.token.clone())
            .json_body(batch)
            .map_err(transport)?
            .await
            .map_err(|error| refused("POST", &url, &error))?;
        Ok(())
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
