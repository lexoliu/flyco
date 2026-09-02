//! The two things an enrolled machine does over ordinary HTTP.
//!
//! * **Enrolling.** `POST /v1/hosts/enroll` happens before this machine has
//!   an identity at all: the enrollment token is the credential, and what
//!   comes back is the host id and the long-lived token every later call
//!   carries. There is no socket yet, and there is nothing to relay through.
//! * **A job result.** `POST /v1/hosts/{id}/job-results` is the *durable*
//!   half of [`HostToControl::JobResult`](flyco_provider::host::HostToControl):
//!   the frame beside it lets the machine's room forget the job it was
//!   holding, but only the Worker can write the machine row that records
//!   what Podman actually created, because a Durable Object can reach
//!   neither D1 nor the provisioning queue.
//!
//! Both go through the same client and the same error type as the session
//! daemon's REST calls; what differs is only which credential is presented.

use core::future::Future;

use flyco_core::HostId;
use flyco_core::host::{EnrollHost, EnrolledHost, ReportJobResult};
use url::Url;
use zenwave::{Client as _, ResponseExt as _};

use crate::control::rest::{ControlApiError, refused, transport};

/// Registers this machine with the control plane, spending an enrollment
/// token.
///
/// # Errors
///
/// Returns [`ControlApiError`] if the control plane could not be reached or
/// refused the token — which it does, indistinguishably, for one that is
/// unknown, expired or already spent.
pub async fn enroll(base: &Url, request: &EnrollHost) -> Result<EnrolledHost, ControlApiError> {
    let url = join(base, "v1/hosts/enroll")?;
    let mut client = zenwave::client();
    client
        .post(&url)
        .map_err(transport)?
        .json_body(request)
        .map_err(transport)?
        .await
        .map_err(|error| refused("POST", &url, &error))?
        .into_json::<EnrolledHost>()
        .await
        .map_err(transport)
}

/// Filing what came of a container job, durably.
///
/// A trait so the relay's ordering — the result is durable before the frame
/// announcing it leaves — is assertable without a control plane.
pub trait JobResults: Send + Sync + 'static {
    /// Reports one job's outcome.
    ///
    /// # Errors
    ///
    /// Returns [`ControlApiError`] if the control plane could not be reached
    /// or refused the report.
    fn report(
        &self,
        report: ReportJobResult,
    ) -> impl Future<Output = Result<(), ControlApiError>> + Send;
}

/// The production [`JobResults`], speaking HTTP through zenwave.
#[derive(Clone)]
pub struct HttpHostApi {
    base: Url,
    host: HostId,
    token: String,
}

impl core::fmt::Debug for HttpHostApi {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HttpHostApi")
            .field("base", &self.base.as_str())
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

impl HttpHostApi {
    /// A client for one enrolled machine.
    #[must_use]
    pub const fn new(base: Url, host: HostId, token: String) -> Self {
        Self { base, host, token }
    }
}

impl JobResults for HttpHostApi {
    async fn report(&self, report: ReportJobResult) -> Result<(), ControlApiError> {
        let url = join(&self.base, &format!("v1/hosts/{}/job-results", self.host))?;
        let mut client = zenwave::client();
        let response = client
            .post(&url)
            .map_err(transport)?
            .bearer_auth(self.token.clone())
            .json_body(&report)
            .map_err(transport)?
            .await
            .map_err(|error| refused("POST", &url, &error))?;

        debug_assert!(response.status().is_success());
        Ok(())
    }
}

/// Resolves one route against the control plane's base URL.
fn join(base: &Url, path: &str) -> Result<String, ControlApiError> {
    base.join(path)
        .map(|url| url.to_string())
        .map_err(|_| ControlApiError::Unaddressable(path.to_owned()))
}

#[cfg(test)]
mod tests {
    use flyco_core::host::{EnrollHost, EnrolledHost, HostFacts, JobOutcome, ReportJobResult};
    use flyco_core::machine::CpuArchitecture;
    use flyco_core::{HostId, MachineId};

    use super::{HttpHostApi, JobResults as _, enroll};
    use crate::testing::{ControlPlane, Reply};

    fn facts() -> HostFacts {
        HostFacts {
            architecture: CpuArchitecture::Arm64,
            vcpus: 10,
            memory_mib: 32 * 1024,
            disk_free_gib: 400,
            podman_version: "5.4.0".to_owned(),
            kernel: "6.11.0-19-generic".to_owned(),
            hostname: "build.lexo.cool".to_owned(),
        }
    }

    #[tokio::test]
    async fn enrolling_spends_the_token_and_comes_back_with_this_machines_own() {
        let host = HostId::generate();
        let mut control_plane = ControlPlane::start(vec![Reply::json(&EnrolledHost {
            host_id: host,
            host_token: "fh_the-machines-own-token".to_owned(),
        })])
        .await;

        let enrolled = enroll(
            &control_plane.base,
            &EnrollHost {
                token: "fh_an-enrollment-token".to_owned(),
                facts: facts(),
            },
        )
        .await
        .expect("enroll");

        assert_eq!(enrolled.host_id, host);
        assert_eq!(enrolled.host_token, "fh_the-machines-own-token");

        let request = control_plane.received.recv().await.expect("a request");
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/v1/hosts/enroll");
        assert_eq!(
            request.authorization, None,
            "the enrollment token is the credential, and it is in the body"
        );
        let sent: EnrollHost = serde_json::from_slice(&request.body).expect("the body");
        assert_eq!(sent.token, "fh_an-enrollment-token");
        assert_eq!(sent.facts, facts());
    }

    #[tokio::test]
    async fn a_job_result_is_filed_against_this_machine_with_its_own_token() {
        let host = HostId::generate();
        let machine = MachineId::generate();
        let mut control_plane = ControlPlane::start(vec![Reply::no_content()]).await;
        let api = HttpHostApi::new(
            control_plane.base.clone(),
            host,
            "fh_the-machines-own-token".to_owned(),
        );

        api.report(ReportJobResult {
            job_id: machine,
            outcome: JobOutcome::Running {
                container: "flyco-x".to_owned(),
                volume: "flyco-x-work".to_owned(),
            },
        })
        .await
        .expect("report");

        let request = control_plane.received.recv().await.expect("a request");
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, format!("/v1/hosts/{host}/job-results"));
        assert_eq!(
            request.authorization.as_deref(),
            Some("Bearer fh_the-machines-own-token")
        );
        let sent: ReportJobResult = serde_json::from_slice(&request.body).expect("the body");
        assert_eq!(sent.job_id, machine);
        assert_eq!(sent.outcome.names(), Some(("flyco-x", "flyco-x-work")));
    }

    #[tokio::test]
    async fn a_refused_enrollment_says_what_the_control_plane_said() {
        let control_plane = ControlPlane::start(vec![Reply::problem(
            410,
            "enrollment-token-expired",
            "this enrollment token is unknown, expired or already spent",
        )])
        .await;

        let error = enroll(
            &control_plane.base,
            &EnrollHost {
                token: "fh_a-spent-token".to_owned(),
                facts: facts(),
            },
        )
        .await
        .expect_err("a spent token enrolls nothing");

        assert!(error.to_string().contains("already spent"), "{error}");
    }
}
