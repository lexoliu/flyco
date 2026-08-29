//! Azure Resource Manager: the URLs, the pinned API versions, and the
//! asynchronous-operation protocol every mutating call speaks.
//!
//! The API versions are pinned from the live provider manifest of a real
//! subscription rather than taken from whatever the documentation's examples
//! happened to show — see `docs/research/azure-arm.md`. They are constants so
//! a change to one is a change to a line of code with a reason beside it.

use serde::Deserialize;

use crate::ProviderError;
use crate::http::HttpResponse;

/// Azure Resource Manager's endpoint.
pub const MANAGEMENT_BASE: &str = "https://management.azure.com";

/// API versions, one per resource type, pinned from a live provider
/// manifest.
pub mod api_version {
    /// `Microsoft.Resources/resourceGroups`.
    pub const RESOURCE_GROUPS: &str = "2023-07-01";
    /// Every `Microsoft.Network` type this driver touches.
    pub const NETWORK: &str = "2024-05-01";
    /// `Microsoft.Compute/virtualMachines`. At least `2019-03-01` is needed
    /// for spot and `2021-03-01` for `deleteOption`.
    pub const COMPUTE: &str = "2024-11-01";
    /// `Microsoft.Compute/disks`.
    pub const DISKS: &str = "2025-01-02";
    /// `Microsoft.Compute/skus`, the resource-SKUs list.
    pub const SKUS: &str = "2021-07-01";
    /// `Microsoft.Compute/locations/{location}/usages`, the quota read.
    pub const USAGES: &str = "2024-11-01";
    /// `Microsoft.Authorization/policyAssignments`, the allowed-regions read.
    pub const POLICY_ASSIGNMENTS: &str = "2024-04-01";
    /// `Microsoft.CostManagement/query`, the metered-spend read.
    ///
    /// Cost Management versions independently of the resource providers
    /// above — it is not in a subscription's provider manifest — so this
    /// one is pinned from its own REST reference.
    pub const COST_MANAGEMENT: &str = "2025-03-01";
}

/// The resource-group scope every per-session resource hangs off.
#[must_use]
pub fn resource_group_scope(subscription: &str, resource_group: &str) -> String {
    format!("{MANAGEMENT_BASE}/subscriptions/{subscription}/resourceGroups/{resource_group}")
}

/// A resource URL under one resource group, with its API version attached.
#[must_use]
pub fn resource_url(
    subscription: &str,
    resource_group: &str,
    provider_path: &str,
    name: &str,
    api_version: &str,
) -> String {
    let scope = resource_group_scope(subscription, resource_group);
    format!("{scope}/providers/{provider_path}/{name}?api-version={api_version}")
}

/// A subscription-scoped URL, with its API version attached.
#[must_use]
pub fn subscription_url(subscription: &str, path: &str, api_version: &str) -> String {
    format!("{MANAGEMENT_BASE}/subscriptions/{subscription}/{path}?api-version={api_version}")
}

/// The error document ARM returns on a rejection.
#[derive(Debug, Clone, Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

/// The inner half of an ARM error.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ErrorBody {
    /// Machine-readable code, e.g. `AzureSpotIsNotSupportedForThisVMSize`.
    #[serde(default)]
    pub code: String,
    /// Human-readable explanation.
    #[serde(default)]
    pub message: String,
}

impl ErrorBody {
    /// Reads the error out of a response body, if it carries one.
    ///
    /// A rejection whose body is not an ARM error document — an outage page,
    /// a proxy's HTML — yields nothing, so the caller reports the status and
    /// the raw text instead of an empty code that reads like a real one.
    #[must_use]
    pub fn of(response: &HttpResponse) -> Option<Self> {
        response
            .json::<ErrorEnvelope>()
            .ok()
            .map(|envelope| envelope.error)
    }

    /// This error, as a provider error naming its code.
    #[must_use]
    pub fn into_provider_error(self) -> ProviderError {
        ProviderError::Rejected(format!("{}: {}", self.code, self.message))
    }
}

/// Terminal and non-terminal states of an asynchronous operation.
///
/// The terminal set is exactly `{Succeeded, Failed, Canceled}`. Anything
/// else — `InProgress`, `Accepted`, `Running`, or a value a resource
/// provider invents tomorrow — means keep polling, which is why the
/// non-terminal case carries the raw string rather than being an enumerated
/// list that a new value would fall off the end of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationStatus {
    /// Finished, successfully.
    Succeeded,
    /// Finished, unsuccessfully.
    Failed,
    /// Finished, cancelled.
    Canceled,
    /// Anything else: still running.
    Running(String),
}

impl OperationStatus {
    /// Classifies a status string.
    #[must_use]
    pub fn parse(status: &str) -> Self {
        match status {
            "Succeeded" => Self::Succeeded,
            "Failed" => Self::Failed,
            "Canceled" => Self::Canceled,
            other => Self::Running(other.to_owned()),
        }
    }

    /// Whether the operation has finished, one way or another.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        !matches!(self, Self::Running(_))
    }
}

/// The body of an `Azure-AsyncOperation` poll.
#[derive(Debug, Clone, Deserialize)]
pub struct OperationBody {
    /// The operation's current state.
    pub status: String,
    /// Present only on `Failed` and `Canceled`.
    #[serde(default)]
    pub error: Option<ErrorBody>,
}

/// How to follow a long-running operation, decided from its response.
///
/// The precedence is not a preference: `Azure-AsyncOperation` is the
/// authoritative pattern and `Location` must **not** be used when it is
/// present, because the two report different things — the operation's status
/// versus the final resource — and mixing them reads a `200` on a resource
/// as success for an operation that has not finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Follow {
    /// Nothing to poll: the call already finished.
    Finished,
    /// Poll this URL and read `status` out of the body.
    Operation {
        /// Where to poll.
        url: String,
        /// `Retry-After`, in seconds, when the service stated one.
        retry_after: Option<u32>,
    },
    /// Poll this URL and read the *HTTP status*: `202` means still running,
    /// `200` means done.
    Location {
        /// Where to poll.
        url: String,
        /// `Retry-After`, in seconds, when the service stated one.
        retry_after: Option<u32>,
    },
}

/// Header naming the authoritative operation-status URL.
pub const ASYNC_OPERATION_HEADER: &str = "azure-asyncoperation";

/// Header naming the fallback polling URL.
pub const LOCATION_HEADER: &str = "location";

/// Header stating how long to wait before polling.
pub const RETRY_AFTER_HEADER: &str = "retry-after";

/// Decides how to follow a mutating call's response.
///
/// A `200` or `204` means the work is already done, whatever headers came
/// with it. A `201`/`202` with neither header is a service that started
/// something and did not say where to watch it, which is unfollowable and
/// therefore an error rather than an optimistic success.
///
/// # Errors
///
/// Returns [`ProviderError::Malformed`] when an accepted call names no way
/// to follow it.
pub fn follow(response: &HttpResponse) -> Result<Follow, ProviderError> {
    let retry_after = response
        .header_value(RETRY_AFTER_HEADER)
        .and_then(|value| value.trim().parse::<u32>().ok());

    if let Some(url) = response.header_value(ASYNC_OPERATION_HEADER) {
        return Ok(Follow::Operation {
            url: url.to_owned(),
            retry_after,
        });
    }
    if response.status == 201 || response.status == 202 {
        return response.header_value(LOCATION_HEADER).map_or(
            Err(ProviderError::Malformed(
                "Azure accepted an operation without naming a URL to follow it on",
            )),
            |url| {
                Ok(Follow::Location {
                    url: url.to_owned(),
                    retry_after,
                })
            },
        );
    }
    Ok(Follow::Finished)
}

/// Backoff between polls when the service states no `Retry-After`.
///
/// Short at first because most ARM network operations finish in single-digit
/// seconds, capped low because a VM creation is minutes and polling it every
/// ten seconds costs nothing.
pub const DEFAULT_BACKOFF_SECONDS: [u32; 4] = [1, 2, 5, 10];

/// The delay before the `attempt`-th poll, honouring `Retry-After` first.
#[must_use]
pub fn poll_delay(retry_after: Option<u32>, attempt: usize) -> u32 {
    retry_after.unwrap_or_else(|| {
        let last = DEFAULT_BACKOFF_SECONDS.len() - 1;
        DEFAULT_BACKOFF_SECONDS[attempt.min(last)]
    })
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_BACKOFF_SECONDS, ErrorBody, Follow, OperationStatus, api_version, follow,
        poll_delay, resource_url,
    };
    use crate::http::HttpResponse;

    const SUB: &str = "e47d07d8-2715-4909-aa56-1bfde801bdf0";

    #[test]
    fn a_resource_url_names_its_group_and_pins_its_api_version() {
        assert_eq!(
            resource_url(
                SUB,
                "flyco-rg",
                "Microsoft.Network/publicIPAddresses",
                "flyco-abc-pip",
                api_version::NETWORK,
            ),
            format!(
                "https://management.azure.com/subscriptions/{SUB}/resourceGroups/flyco-rg\
                 /providers/Microsoft.Network/publicIPAddresses/flyco-abc-pip\
                 ?api-version=2024-05-01"
            )
        );
    }

    #[test]
    fn the_async_operation_header_wins_over_location() {
        let response = HttpResponse::new(201, Vec::new())
            .header("Location", "https://management.azure.com/location")
            .header("Azure-AsyncOperation", "https://management.azure.com/op")
            .header("Retry-After", "17");

        assert_eq!(
            follow(&response).expect("followable"),
            Follow::Operation {
                url: "https://management.azure.com/op".to_owned(),
                retry_after: Some(17),
            }
        );
    }

    #[test]
    fn a_location_only_response_is_followed_by_http_status() {
        let response =
            HttpResponse::new(202, Vec::new()).header("Location", "https://management.azure.com/l");

        assert_eq!(
            follow(&response).expect("followable"),
            Follow::Location {
                url: "https://management.azure.com/l".to_owned(),
                retry_after: None,
            }
        );
    }

    #[test]
    fn a_synchronous_success_has_nothing_to_follow() {
        assert_eq!(
            follow(&HttpResponse::new(200, Vec::new())).expect("followable"),
            Follow::Finished
        );
        assert_eq!(
            follow(&HttpResponse::new(204, Vec::new())).expect("followable"),
            Follow::Finished
        );
    }

    #[test]
    fn an_accepted_operation_with_no_follow_url_is_malformed() {
        follow(&HttpResponse::new(202, Vec::new()))
            .expect_err("an unfollowable operation must not read as success");
    }

    #[test]
    fn only_three_statuses_are_terminal() {
        for terminal in ["Succeeded", "Failed", "Canceled"] {
            assert!(OperationStatus::parse(terminal).is_terminal());
        }
        // Resource providers return their own in-flight values; anything
        // unrecognised means keep polling, never "done".
        for running in ["InProgress", "Accepted", "Running", "Deleting", "Cancelled"] {
            assert!(
                !OperationStatus::parse(running).is_terminal(),
                "`{running}` must not be read as terminal"
            );
        }
    }

    #[test]
    fn a_retry_after_beats_the_default_backoff() {
        assert_eq!(poll_delay(Some(30), 0), 30);
        assert_eq!(poll_delay(None, 0), DEFAULT_BACKOFF_SECONDS[0]);
        assert_eq!(poll_delay(None, 2), DEFAULT_BACKOFF_SECONDS[2]);
        // The backoff is capped rather than unbounded.
        assert_eq!(poll_delay(None, 99), 10);
    }

    #[test]
    fn an_arm_error_document_yields_its_code() {
        let response = HttpResponse::new(
            400,
            br#"{"error":{"code":"AzureSpotIsNotSupportedForThisVMSize","message":"nope"}}"#
                .to_vec(),
        );
        let error = ErrorBody::of(&response).expect("an ARM error document");
        assert_eq!(error.code, "AzureSpotIsNotSupportedForThisVMSize");
    }

    #[test]
    fn a_rejection_that_is_not_an_arm_document_yields_no_code() {
        assert!(ErrorBody::of(&HttpResponse::new(502, b"<html>gateway</html>".to_vec())).is_none());
    }
}
