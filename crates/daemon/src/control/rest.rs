//! The daemon's REST client for its own session.
//!
//! Four things a daemon does over ordinary HTTP rather than over the relay
//! socket, each for its own reason:
//!
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
//!
//! Every call carries the session's `fd_` daemon token, which authorizes
//! exactly this session's daemon-scoped routes.

use core::future::Future;

use flyco_core::wire::ApprovalPayload;
use flyco_core::{ApprovalId, ApprovalView, HarnessObservation, Problem, SessionId};
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

/// What the control plane offers a session's daemon.
///
/// A trait so the wire client can be driven without a control plane, and so
/// a test can assert on the *ordering* the relay depends on — the approval
/// row exists before the frame announcing it leaves.
pub trait ControlApi: Send + Sync + 'static {
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

impl ControlApi for HttpControlApi {
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
