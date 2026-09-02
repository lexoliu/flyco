//! What the GCP driver actually puts on the wire.
//!
//! No credentials exist here and no test may create a cloud resource, so
//! every exchange is recorded: the driver is handed a scripted list of
//! responses and the assertions are on the requests that come back out —
//! the exact URL, the exact bearer token, the exact JSON body. The signed
//! assertion is reproducible for the same reason the AWS signature is: the
//! instant it claims comes from a
//! [`ManualWallClock`](crate::clock::ManualWallClock) rather than the host's
//! clock.
//!
//! The service-account key in `fixtures/gcp/service_account.json` is a
//! throwaway 2048-bit RSA key generated for these tests and used nowhere
//! else. It has to be a key that actually signs, because the assertion is
//! what is being asserted.
//!
//! # Running it against a real project
//!
//! There is a live test, `live_provision_and_destroy`, behind the
//! `gcp-live` feature, which is off by default and which CI never enables.
//! It creates and destroys real, billable resources — an instance and a
//! persistent disk. To run it:
//!
//! ```text
//! export FLYCO_GCP_SERVICE_ACCOUNT_JSON="$(cat ~/flyco-provisioner.json)"
//! export FLYCO_GCP_ZONE=us-central1-a
//! cargo test -p flyco-provider --features gcp-live -- --ignored --nocapture
//! ```
//!
//! The service account needs `roles/compute.instanceAdmin.v1` on the project
//! and `roles/billing.viewer` on its billing account (the SKU catalog is a
//! billing read, not a compute one), and the project must still have its
//! default VPC network.

use flyco_core::machine::{CloudProviderKind, MachineSpec, MachineState};
use flyco_core::{HarnessKind, MachineId, PermissionMode, SessionId};
use serde_json::Value;

use super::{
    ExclusionReason, GcpProvider, GcpWorkspace, IMAGE_FAMILY_X86_64, SPOT_UNSUPPORTED_CODES, names,
};
use crate::clock::{ManualClock, ManualWallClock};
use crate::cloud_init::CONFIG_PATH;
use crate::gcp::auth::ServiceAccountKey;
use crate::http::{HttpRequest, HttpResponse, Method};
use crate::testing::{RecordedTransport, RecordingTimer};
use crate::{
    CapacityMode, ClaudeCredential, CloudProvider, DaemonBootstrap, Machine, ProviderError,
    ProvisionRequest,
};

const PROJECT: &str = "flyco-sessions";
const ZONE: &str = "us-central1-a";
const REGION: &str = "us-central1";
const BASE: &str = "https://compute.googleapis.com/compute/v1";

const MACHINE_TYPE: &str = "e2-standard-2";
const LARGE_TYPE: &str = "e2-standard-32";
const OBSOLETE_TYPE: &str = "f1-micro";
const UNPRICED_TYPE: &str = "n2-standard-2";

const ADDRESS: &str = "34.72.118.204";

/// 2026-08-29T12:00:00Z — the instant every assertion in these tests claims.
const SIGNED_AT: u64 = 1_788_004_800;

const KEY: &str = include_str!("../../fixtures/gcp/service_account.json");
const MACHINE_TYPES: &str = include_str!("../../fixtures/gcp/machine_types.json");
const REGION_INFO: &str = include_str!("../../fixtures/gcp/region.json");
const REGION_DOWN: &str = include_str!("../../fixtures/gcp/region_down.json");
const REGION_PREEMPTIBLE_SPENT: &str =
    include_str!("../../fixtures/gcp/region_preemptible_spent.json");
const REGION_NO_ON_DEMAND: &str = include_str!("../../fixtures/gcp/region_no_on_demand.json");
const SKUS: &str = include_str!("../../fixtures/gcp/skus.json");
const OPERATION_DONE: &str = include_str!("../../fixtures/gcp/operation_done.json");
const OPERATION_RUNNING: &str = include_str!("../../fixtures/gcp/operation_running.json");
const OPERATION_FAILED: &str = include_str!("../../fixtures/gcp/operation_failed.json");
const OPERATION_QUOTA: &str = include_str!("../../fixtures/gcp/operation_failed_quota.json");
const OPERATION_PERMISSION: &str =
    include_str!("../../fixtures/gcp/operation_failed_permission.json");
const INSTANCE_RUNNING: &str = include_str!("../../fixtures/gcp/instance_running.json");
const INSTANCE_RESTARTED: &str = include_str!("../../fixtures/gcp/instance_restarted.json");
const ERROR_PERMISSION: &str = include_str!("../../fixtures/gcp/error_permission_denied.json");

/// A driver over a scripted transport, clocks that do not move, and a timer
/// that records rather than waits.
type Recorded = GcpProvider<RecordedTransport, ManualClock, RecordingTimer, ManualWallClock>;

fn key() -> ServiceAccountKey {
    ServiceAccountKey::parse(KEY).expect("the key fixture parses")
}

fn provider(responses: Vec<HttpResponse>) -> Recorded {
    provider_over(GcpWorkspace::new(), responses)
}

fn provider_over(workspace: GcpWorkspace, responses: Vec<HttpResponse>) -> Recorded {
    GcpProvider::with_parts(
        RecordedTransport::new(responses),
        ManualClock::new(),
        RecordingTimer::new(),
        ManualWallClock::at(SIGNED_AT),
        key(),
        workspace,
    )
}

fn json(body: &str) -> HttpResponse {
    HttpResponse::new(200, body.as_bytes().to_vec())
}

fn refused(status: u16, body: &str) -> HttpResponse {
    HttpResponse::new(status, body.as_bytes().to_vec())
}

fn token() -> HttpResponse {
    json(r#"{"access_token":"ya29.stub","expires_in":3599,"token_type":"Bearer"}"#)
}

fn body_of(request: &HttpRequest) -> Value {
    serde_json::from_slice(&request.body).expect("the driver sends a JSON body")
}

fn header(request: &HttpRequest, name: &str) -> String {
    request
        .headers
        .iter()
        .find(|(header, _)| header == name)
        .map_or_else(
            || panic!("the request carries a `{name}` header"),
            |(_, value)| value.clone(),
        )
}

fn request_in(machine: MachineId, zone: &str, machine_type: &str, spot: bool) -> ProvisionRequest {
    ProvisionRequest {
        machine,
        spec: MachineSpec {
            provider: CloudProviderKind::Gcp,
            machine_type: machine_type.to_owned(),
            region: zone.to_owned(),
            spot,
            disk_gib: 30,
        },
        bootstrap: DaemonBootstrap {
            session: SessionId::generate(),
            control_plane_url: "https://flyco.dev/".to_owned(),
            daemon_token: "fd_a-live-daemon-token".to_owned(),
            harness: HarnessKind::ClaudeCode,
            permission_mode: PermissionMode::Default,
            claude_auth: ClaudeCredential::Inherit,
            resume_session_id: None,
        },
    }
}

fn request(machine: MachineId, machine_type: &str, spot: bool) -> ProvisionRequest {
    request_in(machine, ZONE, machine_type, spot)
}

/// The responses a clean provisioning run consumes, in order.
///
/// Index 0 is the token exchange, 1 the region's own answer (which carries
/// both halves of every quota), 2 the zone's machine types; the insert is at
/// [`INSERT`].
fn provision_script(insert: Vec<HttpResponse>) -> Vec<HttpResponse> {
    let mut script = vec![token(), json(REGION_INFO), json(MACHINE_TYPES)];
    script.extend(insert);
    script
}

/// Index of `instances.insert` in a [`provision_script`] run.
const INSERT: usize = 3;

fn provisioned(machine: MachineId) -> Machine {
    Machine {
        id: machine,
        native_id: format!(
            "{BASE}/projects/{PROJECT}/zones/{ZONE}/instances/{}",
            names::machine(machine)
        ),
        region: ZONE.to_owned(),
        state: MachineState::Running,
        capacity_mode: CapacityMode::Spot,
        address: Some(ADDRESS.to_owned()),
    }
}

// ── The gates ──

#[tokio::test]
async fn provisioning_reads_the_region_and_the_zones_types_before_it_writes() {
    let mut gcp = provider(provision_script(vec![
        json(OPERATION_DONE),
        json(INSTANCE_RUNNING),
    ]));
    gcp.provision(&request(MachineId::generate(), MACHINE_TYPE, true))
        .await
        .expect("provision");

    let transport = gcp.transport();
    assert_eq!(
        transport.request(0).url,
        "https://oauth2.googleapis.com/token"
    );

    // One read states both halves of every quota, which is what makes the
    // gate a single call rather than AWS's two.
    let region = transport.request(1);
    assert_eq!(region.method, Method::Get);
    assert_eq!(
        region.url,
        format!("{BASE}/projects/{PROJECT}/regions/{REGION}")
    );

    let types = transport.request(2);
    assert_eq!(
        types.url,
        format!("{BASE}/projects/{PROJECT}/zones/{ZONE}/machineTypes")
    );

    assert_eq!(transport.request(INSERT).method, Method::Post);
}

#[tokio::test]
async fn every_authenticated_request_carries_the_minted_token() {
    let mut gcp = provider(provision_script(vec![
        json(OPERATION_DONE),
        json(INSTANCE_RUNNING),
    ]));
    gcp.provision(&request(MachineId::generate(), MACHINE_TYPE, true))
        .await
        .expect("provision");

    let transport = gcp.transport();
    for index in 1..transport.request_count() {
        assert_eq!(
            header(&transport.request(index), "authorization"),
            "Bearer ya29.stub",
            "request {index} must present the token"
        );
    }
}

#[tokio::test]
async fn the_token_is_minted_once_per_driver() {
    let mut gcp = provider(
        [
            provision_script(vec![json(OPERATION_DONE), json(INSTANCE_RUNNING)]),
            vec![json(OPERATION_DONE)],
        ]
        .concat(),
    );
    let machine = MachineId::generate();
    gcp.provision(&request(machine, MACHINE_TYPE, true))
        .await
        .expect("provision");
    gcp.deallocate(&provisioned(machine))
        .await
        .expect("deallocate");

    let transport = gcp.transport();
    let mints = (0..transport.request_count())
        .filter(|index| {
            transport
                .request(*index)
                .url
                .contains("oauth2.googleapis.com")
        })
        .count();
    assert_eq!(
        mints, 1,
        "a token is good for an hour; minting one per call is waste"
    );
}

#[tokio::test]
async fn a_region_that_is_not_up_is_refused_before_the_machine_types_are_read() {
    let mut gcp = provider(vec![token(), json(REGION_DOWN)]);

    let error = gcp
        .provision(&request(MachineId::generate(), MACHINE_TYPE, true))
        .await
        .expect_err("a region that is down cannot be deployed into");
    assert!(matches!(error, ProviderError::Unavailable { .. }));
    assert_eq!(
        gcp.transport().request_count(),
        2,
        "no machine-type read happens for a region that cannot be used at all"
    );
}

#[tokio::test]
async fn a_machine_type_the_zone_does_not_offer_is_refused_before_any_write() {
    let mut gcp = provider(vec![token(), json(REGION_INFO), json(MACHINE_TYPES)]);

    let error = gcp
        .provision(&request(MachineId::generate(), "e2-standard-4", true))
        .await
        .expect_err("an unoffered machine type cannot be created");
    assert!(matches!(error, ProviderError::Unavailable { .. }));
    assert_eq!(gcp.transport().request_count(), 3);
}

#[tokio::test]
async fn a_withdrawn_machine_type_is_refused_rather_than_attempted() {
    let mut gcp = provider(vec![token(), json(REGION_INFO), json(MACHINE_TYPES)]);

    let error = gcp
        .provision(&request(MachineId::generate(), OBSOLETE_TYPE, true))
        .await
        .expect_err("an obsolete machine type is refused by the API");
    let ProviderError::Unavailable { reason, .. } = &error else {
        panic!("a withdrawn type is an availability failure: {error}");
    };
    assert!(
        reason.contains("withdrawn"),
        "the refusal says why: {reason}"
    );
}

#[tokio::test]
async fn a_machine_only_the_preemptible_pool_can_fund_is_still_provisioned() {
    // Every on-demand vCPU is spent and the preemptible pool is free, which
    // is the case that makes spot worth defaulting to: the same machine is
    // refused on-demand and runs as spot.
    let script = |quota: &str| {
        vec![
            token(),
            json(quota),
            json(MACHINE_TYPES),
            json(OPERATION_DONE),
            json(INSTANCE_RUNNING),
        ]
    };

    let mut gcp = provider(script(REGION_NO_ON_DEMAND));
    let machine = gcp
        .provision(&request(MachineId::generate(), MACHINE_TYPE, true))
        .await
        .expect("spot draws on a different pool");
    assert_eq!(machine.capacity_mode, CapacityMode::Spot);

    let mut gcp = provider(script(REGION_NO_ON_DEMAND));
    let error = gcp
        .provision(&request(MachineId::generate(), MACHINE_TYPE, false))
        .await
        .expect_err("the on-demand pool has no room at all");
    assert!(matches!(
        error,
        ProviderError::QuotaExceeded { ref quota, .. } if quota == "CPUS"
    ));
}

#[tokio::test]
async fn a_spent_preemptible_pool_refuses_spot() {
    let mut gcp = provider(vec![
        token(),
        json(REGION_PREEMPTIBLE_SPENT),
        json(MACHINE_TYPES),
    ]);

    let error = gcp
        .provision(&request(MachineId::generate(), MACHINE_TYPE, true))
        .await
        .expect_err("the preemptible pool is spent");
    assert!(matches!(
        error,
        ProviderError::QuotaExceeded { ref quota, .. } if quota == "PREEMPTIBLE_CPUS"
    ));
}

#[tokio::test]
async fn a_machine_too_large_for_either_pool_is_refused() {
    let mut gcp = provider(vec![token(), json(REGION_INFO), json(MACHINE_TYPES)]);

    let error = gcp
        .provision(&request(MachineId::generate(), LARGE_TYPE, true))
        .await
        .expect_err("thirty-two vCPUs do not fit under a limit of four");
    assert!(matches!(error, ProviderError::QuotaExceeded { .. }));
}

// ── The insert body ──

#[tokio::test]
async fn the_instance_body_is_the_measured_shape() {
    let machine = MachineId::generate();
    let provision = request(machine, MACHINE_TYPE, true);
    let session = provision.bootstrap.session;
    let mut gcp = provider(provision_script(vec![
        json(OPERATION_DONE),
        json(INSTANCE_RUNNING),
    ]));
    gcp.provision(&provision).await.expect("provision");

    let insert = gcp.transport().request(INSERT);
    assert_eq!(insert.method, Method::Post);
    assert_eq!(
        insert.url,
        format!("{BASE}/projects/{PROJECT}/zones/{ZONE}/instances")
    );

    let body = body_of(&insert);
    assert_eq!(body["name"], names::machine(machine));
    assert_eq!(
        body["machineType"],
        format!("{BASE}/projects/{PROJECT}/zones/{ZONE}/machineTypes/{MACHINE_TYPE}")
    );

    // Spot, and the two fields that decide what survives a preemption.
    let scheduling = &body["scheduling"];
    assert_eq!(scheduling["provisioningModel"], "SPOT");
    assert_eq!(
        scheduling["onHostMaintenance"], "TERMINATE",
        "a spot instance may not be live-migrated, and the API refuses the insert otherwise"
    );
    assert_eq!(scheduling["automaticRestart"], false);
    assert_eq!(
        scheduling["instanceTerminationAction"], "STOP",
        "`DELETE` would take the session's work with the machine"
    );

    // The boot disk outlives its instance, which is what makes a resize and
    // a destroy two different things.
    let disk = &body["disks"][0];
    assert_eq!(disk["boot"], true);
    assert_eq!(disk["autoDelete"], false);
    assert_eq!(
        disk["initializeParams"]["diskName"],
        names::boot_disk(machine)
    );
    assert_eq!(disk["initializeParams"]["diskSizeGb"], 30);
    assert_eq!(disk["initializeParams"]["sourceImage"], IMAGE_FAMILY_X86_64);
    assert!(
        disk["initializeParams"]["diskType"]
            .as_str()
            .expect("a disk type")
            .ends_with(&format!("/zones/{ZONE}/diskTypes/pd-balanced")),
        "the disk type is a zonal URL, and a bare name is refused"
    );

    // Without an access config the machine has no external address at all
    // and answers nothing.
    let interface = &body["networkInterfaces"][0];
    assert!(
        interface["network"]
            .as_str()
            .expect("a network")
            .ends_with("/global/networks/default")
    );
    assert_eq!(interface["accessConfigs"][0]["type"], "ONE_TO_ONE_NAT");
    assert_eq!(interface["accessConfigs"][0]["networkTier"], "PREMIUM");

    assert_eq!(body["labels"]["owner"], "gcp");
    assert_eq!(body["labels"]["flyco-session"], session.to_string());
    assert_eq!(body["labels"]["flyco-machine"], machine.to_string());
}

#[tokio::test]
async fn an_on_demand_request_is_scheduled_as_standard_and_may_migrate() {
    let mut gcp = provider(provision_script(vec![
        json(OPERATION_DONE),
        json(INSTANCE_RUNNING),
    ]));
    gcp.provision(&request(MachineId::generate(), MACHINE_TYPE, false))
        .await
        .expect("provision");

    let scheduling = body_of(&gcp.transport().request(INSERT))["scheduling"].clone();
    assert_eq!(scheduling["provisioningModel"], "STANDARD");
    // Live migration is what keeps an ordinary machine alive through host
    // maintenance, and only a spot instance is barred from it.
    assert_eq!(scheduling["onHostMaintenance"], "MIGRATE");
    assert_eq!(scheduling["automaticRestart"], true);
    assert!(
        scheduling.get("instanceTerminationAction").is_none(),
        "ordinary capacity is never preempted, so there is no action to state"
    );
}

#[tokio::test]
async fn cloud_init_carries_the_daemon_configuration_and_nothing_readable() {
    use base64::Engine as _;

    let provision = request(MachineId::generate(), MACHINE_TYPE, true);
    let mut gcp = provider(provision_script(vec![
        json(OPERATION_DONE),
        json(INSTANCE_RUNNING),
    ]));
    gcp.provision(&provision).await.expect("provision");

    let insert = gcp.transport().request(INSERT);
    let body = body_of(&insert);
    let item = &body["metadata"]["items"][0];
    assert_eq!(
        item["key"], "user-data",
        "`user-data` is the key the guest's cloud-init reads"
    );

    // The token must not be legible in the request itself.
    let raw = insert.body_text().expect("UTF-8").to_owned();
    assert!(!raw.contains("fd_a-live-daemon-token"));

    let cloud_config = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(item["value"].as_str().expect("a string"))
            .expect("the metadata value is base64"),
    )
    .expect("the cloud-config is UTF-8");

    assert!(cloud_config.starts_with("#cloud-config"));
    assert!(cloud_config.contains(CONFIG_PATH));
    assert!(cloud_config.contains("encoding: b64"));

    let encoded = cloud_config
        .lines()
        .find_map(|line| line.trim().strip_prefix("content: "))
        .expect("the cloud-config writes the daemon configuration");
    let config = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("the config is base64"),
    )
    .expect("the config is UTF-8");
    assert!(config.contains("daemon_token = \"fd_a-live-daemon-token\""));
    assert!(config.contains(&provision.bootstrap.session.to_string()));
}

// ── Spot, and the fallback ──

#[tokio::test]
async fn a_spot_refusal_is_retried_on_demand_for_every_code_that_means_it() {
    for failure in [OPERATION_FAILED, OPERATION_QUOTA] {
        let mut gcp = provider(provision_script(vec![
            json(failure),
            json(OPERATION_DONE),
            json(INSTANCE_RUNNING),
        ]));

        let provisioned = gcp
            .provision(&request(MachineId::generate(), MACHINE_TYPE, true))
            .await
            .expect("a spot refusal falls back rather than failing");
        assert_eq!(
            provisioned.capacity_mode,
            CapacityMode::OnDemand,
            "the machine records the capacity it got, not the one it asked for"
        );

        let first = body_of(&gcp.transport().request(INSERT));
        let second = body_of(&gcp.transport().request(INSERT + 1));
        assert_eq!(first["scheduling"]["provisioningModel"], "SPOT");
        assert_eq!(second["scheduling"]["provisioningModel"], "STANDARD");
        assert!(
            second["scheduling"]
                .get("instanceTerminationAction")
                .is_none()
        );
        assert_eq!(
            second["metadata"], first["metadata"],
            "the retry is the same machine, not a different one"
        );
        assert_eq!(second["disks"], first["disks"]);
    }
}

#[tokio::test]
async fn every_code_that_triggers_the_fallback_is_about_interruptible_capacity() {
    // A guard on the list itself: adding a code here that means something
    // else would turn one clear error into two.
    assert_eq!(
        SPOT_UNSUPPORTED_CODES,
        [
            "ZONE_RESOURCE_POOL_EXHAUSTED",
            "ZONE_RESOURCE_POOL_EXHAUSTED_WITH_DETAILS",
            "QUOTA_EXCEEDED",
        ]
    );
}

#[tokio::test]
async fn the_on_demand_fallback_re_checks_the_pool_it_would_spend() {
    // The machine passed the gate as spot and the on-demand pool is spent,
    // so re-sending it would fail later with an opaque error. Refuse it
    // here, where the reason is nameable.
    let mut gcp = provider(vec![
        token(),
        json(REGION_NO_ON_DEMAND),
        json(MACHINE_TYPES),
        json(OPERATION_FAILED),
    ]);

    let error = gcp
        .provision(&request(MachineId::generate(), MACHINE_TYPE, true))
        .await
        .expect_err("the on-demand pool cannot fund this machine");
    assert!(matches!(error, ProviderError::QuotaExceeded { .. }));
    assert_eq!(
        gcp.transport().request_count(),
        INSERT + 1,
        "no second insert is attempted"
    );
}

#[tokio::test]
async fn a_refusal_that_is_not_about_capacity_is_not_retried() {
    let mut gcp = provider(provision_script(vec![json(OPERATION_PERMISSION)]));

    let error = gcp
        .provision(&request(MachineId::generate(), MACHINE_TYPE, true))
        .await
        .expect_err("a permission failure is a failure");
    assert_eq!(error.code(), Some("PERMISSION_DENIED"));
    assert_eq!(
        gcp.transport().request_count(),
        INSERT + 1,
        "there must be no second insert"
    );
}

// ── The asynchronous-operation protocol ──

#[tokio::test]
async fn an_operation_is_polled_until_it_reaches_a_terminal_status() {
    let mut gcp = provider(provision_script(vec![
        json(OPERATION_RUNNING),
        json(OPERATION_RUNNING),
        json(OPERATION_DONE),
        json(INSTANCE_RUNNING),
    ]));

    gcp.provision(&request(MachineId::generate(), MACHINE_TYPE, true))
        .await
        .expect("provision");

    let transport = gcp.transport();
    for index in [INSERT + 1, INSERT + 2] {
        assert_eq!(transport.request(index).method, Method::Get);
        assert!(
            transport
                .request(index)
                .url
                .ends_with("/operations/operation-1788004800000-flyco"),
            "an in-flight operation is polled at the `selfLink` it names"
        );
    }
    assert_eq!(
        gcp.timer().delays(),
        vec![1, 2],
        "Compute Engine states no `Retry-After`, so the shared backoff paces it and grows"
    );
}

#[tokio::test]
async fn a_done_operation_that_carries_an_error_is_a_failure() {
    // The trap this driver exists to avoid: the status says the operation
    // finished, and the failure is inside it. Reading the status alone would
    // call a failed provision a success.
    let mut gcp = provider(provision_script(vec![json(OPERATION_PERMISSION)]));

    let error = gcp
        .provision(&request(MachineId::generate(), MACHINE_TYPE, false))
        .await
        .expect_err("a finished failure is a failure");
    assert!(matches!(
        error,
        ProviderError::OperationFailed { ref code, .. } if code == "PERMISSION_DENIED"
    ));
}

#[tokio::test]
async fn a_call_that_is_refused_outright_never_becomes_an_operation() {
    let mut gcp = provider(provision_script(vec![refused(403, ERROR_PERMISSION)]));

    let error = gcp
        .provision(&request(MachineId::generate(), MACHINE_TYPE, false))
        .await
        .expect_err("a refused insert is a failed provision");
    assert!(matches!(
        error,
        ProviderError::Refused { ref code, .. } if code == "forbidden"
    ));
}

#[tokio::test]
async fn a_401_re_mints_the_token_and_retries_once() {
    let mut gcp = provider(vec![
        token(),
        HttpResponse::new(401, Vec::new()),
        token(),
        json(REGION_INFO),
        json(MACHINE_TYPES),
        json(OPERATION_DONE),
        json(INSTANCE_RUNNING),
    ]);

    gcp.provision(&request(MachineId::generate(), MACHINE_TYPE, true))
        .await
        .expect("provision");

    let transport = gcp.transport();
    assert!(transport.request(0).url.contains("oauth2.googleapis.com"));
    assert!(transport.request(2).url.contains("oauth2.googleapis.com"));
    assert_eq!(
        transport.request(1).url,
        transport.request(3).url,
        "the same request is retried, not a different one"
    );
}

// ── Lifecycle ──

#[tokio::test]
async fn deallocating_posts_the_stop_action() {
    let machine = provisioned(MachineId::generate());
    let mut gcp = provider(vec![token(), json(OPERATION_DONE)]);

    gcp.deallocate(&machine).await.expect("deallocate");

    let request = gcp.transport().request(1);
    assert_eq!(request.method, Method::Post);
    assert_eq!(
        request.url,
        format!(
            "{BASE}/projects/{PROJECT}/zones/{ZONE}/instances/{}/stop",
            names::machine(machine.id)
        )
    );
    assert_eq!(request.body, [] as [u8; 0]);
}

#[tokio::test]
async fn starting_posts_the_start_action_and_reads_the_new_address() {
    let machine = provisioned(MachineId::generate());
    let mut gcp = provider(vec![
        token(),
        json(OPERATION_DONE),
        json(INSTANCE_RESTARTED),
    ]);

    let started = gcp.start(&machine).await.expect("start");
    assert_eq!(started.state, MachineState::Running);
    assert!(gcp.transport().request(1).url.ends_with("/start"));
    // An ephemeral address is released when the instance stops, so a started
    // machine answers somewhere new and the caller has to be told where.
    assert_eq!(started.address.as_deref(), Some("35.184.22.91"));
}

#[tokio::test]
async fn resizing_stops_sets_the_machine_type_and_starts_again() {
    let machine = provisioned(MachineId::generate());
    let mut gcp = provider(vec![
        token(),
        json(REGION_INFO),
        json(MACHINE_TYPES),
        json(OPERATION_DONE), // stop
        json(OPERATION_DONE), // setMachineType
        json(OPERATION_DONE), // start
    ]);

    gcp.resize(&machine, "e2-medium")
        .await
        .expect("resize preserves the disk");

    let transport = gcp.transport();
    let instance = format!(
        "{BASE}/projects/{PROJECT}/zones/{ZONE}/instances/{}",
        names::machine(machine.id)
    );
    assert_eq!(transport.request(3).url, format!("{instance}/stop"));

    let set = transport.request(4);
    assert_eq!(set.url, format!("{instance}/setMachineType"));
    assert_eq!(
        body_of(&set),
        serde_json::json!({
            "machineType":
                format!("{BASE}/projects/{PROJECT}/zones/{ZONE}/machineTypes/e2-medium")
        }),
        "the new type is a zonal URL, and a bare name is refused"
    );

    assert_eq!(transport.request(5).url, format!("{instance}/start"));
    // Nothing touches the disk: it survives because it was never being
    // deleted.
    assert!(
        !(0..transport.request_count())
            .any(|index| transport.request(index).url.contains("/disks/"))
    );
}

#[tokio::test]
async fn a_resize_to_a_type_with_no_quota_never_stops_the_machine() {
    let machine = provisioned(MachineId::generate());
    let mut gcp = provider(vec![token(), json(REGION_INFO), json(MACHINE_TYPES)]);

    gcp.resize(&machine, LARGE_TYPE)
        .await
        .expect_err("a resize into a machine the quota cannot fund is refused");
    assert_eq!(
        gcp.transport().request_count(),
        3,
        "nothing was stopped, so the session is still running"
    );
}

#[tokio::test]
async fn a_failed_resize_operation_is_a_failure_whatever_the_instance_reports() {
    let machine = provisioned(MachineId::generate());
    let mut gcp = provider(vec![
        token(),
        json(REGION_INFO),
        json(MACHINE_TYPES),
        json(OPERATION_DONE),
        json(OPERATION_PERMISSION),
    ]);

    let error = gcp
        .resize(&machine, "e2-medium")
        .await
        .expect_err("a failed operation is a failed resize");
    assert_eq!(error.code(), Some("PERMISSION_DENIED"));
}

#[tokio::test]
async fn destroying_deletes_the_instance_then_the_disk_that_outlived_it() {
    let machine = provisioned(MachineId::generate());
    let mut gcp = provider(vec![token(), json(OPERATION_DONE), json(OPERATION_DONE)]);

    gcp.destroy(&machine).await.expect("destroy");

    let transport = gcp.transport();
    // In that order: a disk that is still attached cannot be deleted, and
    // the instance's deletion is what detaches it.
    assert_eq!(transport.request(1).method, Method::Delete);
    assert_eq!(
        transport.request(1).url,
        format!(
            "{BASE}/projects/{PROJECT}/zones/{ZONE}/instances/{}",
            names::machine(machine.id)
        )
    );
    assert_eq!(transport.request(2).method, Method::Delete);
    assert_eq!(
        transport.request(2).url,
        format!(
            "{BASE}/projects/{PROJECT}/zones/{ZONE}/disks/{}",
            names::boot_disk(machine.id)
        )
    );
}

// ── The catalog ──

fn one_zone() -> GcpWorkspace {
    GcpWorkspace::new().with_zones(vec![ZONE.to_owned()])
}

fn catalog_script() -> Vec<HttpResponse> {
    vec![token(), json(REGION_INFO), json(MACHINE_TYPES), json(SKUS)]
}

#[tokio::test]
async fn the_catalog_offers_only_what_passes_both_gates_and_has_a_price() {
    let mut gcp = provider_over(one_zone(), catalog_script());

    let catalog = gcp.catalog().await.expect("catalog");
    let offered: Vec<&str> = catalog
        .iter()
        .map(|entry| entry.machine_type.as_str())
        .collect();

    assert!(offered.contains(&MACHINE_TYPE));
    // Deprecated still works and is offered.
    assert!(offered.contains(&"n1-standard-1"));
    // Obsolete is refused by the API, so offering it would put an
    // uncreatable machine on the menu.
    assert!(!offered.contains(&OBSOLETE_TYPE));
    // Its family has a core rate and no RAM rate, and half a price is not a
    // price.
    assert!(!offered.contains(&UNPRICED_TYPE));
    // Thirty-two vCPUs fit under neither pool.
    assert!(!offered.contains(&LARGE_TYPE));

    let standard = catalog
        .iter()
        .find(|entry| entry.machine_type == MACHINE_TYPE)
        .expect("the standard type is offered");
    assert_eq!(standard.provider, CloudProviderKind::Gcp);
    assert_eq!(
        standard.region, ZONE,
        "the entry names the zone, because a type offered in one zone of a \
         region is routinely absent from another"
    );
    assert_eq!(
        standard.capacity,
        Some(flyco_core::MachineCapacity {
            vcpus: 2,
            memory_mib: 8_192
        })
    );
    // Two core-hours plus eight RAM-hours of the E2 rate: there is no
    // per-machine-type price to look up.
    assert_eq!(
        standard.pricing,
        flyco_core::MachinePricing::Metered {
            on_demand_hourly: flyco_core::Usd::from_micros(2 * 21_811 + 8 * 2_923),
            spot_hourly: Some(flyco_core::Usd::from_micros(2 * 6_543 + 8 * 877)),
            minimum: None,
            storage: flyco_core::StoragePricing::PerGibHourly {
                rate: flyco_core::Usd::from_micros(137),
            },
        }
    );
}

#[tokio::test]
async fn a_type_the_preemptible_pool_cannot_fund_is_quoted_no_spot_price() {
    let mut gcp = provider_over(
        one_zone(),
        vec![
            token(),
            json(REGION_PREEMPTIBLE_SPENT),
            json(MACHINE_TYPES),
            json(SKUS),
        ],
    );

    let catalog = gcp.catalog().await.expect("catalog");
    let standard = catalog
        .iter()
        .find(|entry| entry.machine_type == MACHINE_TYPE)
        .expect("the on-demand pool still funds it");

    assert_eq!(
        standard.pricing.hourly(true),
        standard.pricing.hourly(false),
        "a spot price is only quoted when the spot pool could actually fund it"
    );
}

#[tokio::test]
async fn every_exclusion_says_which_of_the_problems_it_is() {
    let mut gcp = provider_over(one_zone(), catalog_script());

    let report = gcp.zone_report(ZONE).await.expect("report");
    let reason = |machine_type: &str| {
        report
            .excluded
            .iter()
            .find(|(name, _)| name == machine_type)
            .map_or_else(
                || panic!("`{machine_type}` should have been excluded"),
                |(_, reason)| reason.clone(),
            )
    };

    assert!(matches!(
        reason(OBSOLETE_TYPE),
        ExclusionReason::NotOffered(_)
    ));
    assert!(matches!(reason(UNPRICED_TYPE), ExclusionReason::Unpriced));
    assert!(matches!(reason(LARGE_TYPE), ExclusionReason::NoQuota(_)));
}

#[tokio::test]
async fn a_zone_whose_region_is_down_reports_one_exclusion_naming_it() {
    let mut gcp = provider(vec![token(), json(REGION_DOWN)]);

    let report = gcp.zone_report(ZONE).await.expect("report");
    assert_eq!(report.offered, Vec::new());
    assert_eq!(
        report.excluded.len(),
        1,
        "the answer is the same for every machine type in the zone"
    );
    let (subject, reason) = &report.excluded[0];
    assert_eq!(subject, ZONE);
    assert!(matches!(reason, ExclusionReason::ZoneUnavailable(_)));
}

#[tokio::test]
async fn the_price_catalog_is_a_billing_read_rather_than_a_compute_one() {
    let mut gcp = provider_over(one_zone(), catalog_script());
    gcp.catalog().await.expect("catalog");

    let skus = gcp.transport().request(3);
    assert!(
        skus.url
            .starts_with("https://cloudbilling.googleapis.com/v1/services/6F81-5844-456A/skus?"),
        "the catalog is Compute Engine's own service id in the billing API: {}",
        skus.url
    );
    assert_eq!(header(&skus, "authorization"), "Bearer ya29.stub");
}

#[tokio::test]
async fn a_zone_name_that_is_not_one_is_refused_rather_than_guessed_at() {
    // A quota is published per region, and a region is a zone's name minus
    // its suffix — so a caller who names a region has named nowhere.
    let mut gcp = provider(vec![token()]);
    let error = gcp
        .provision(&request_in(
            MachineId::generate(),
            REGION,
            MACHINE_TYPE,
            true,
        ))
        .await
        .expect_err("a region is not a zone");
    assert!(matches!(error, ProviderError::Malformed(_)));
}

/// The one test that touches a real project.
///
/// Ignored *and* feature-gated, so neither `cargo test` nor
/// `cargo test -- --ignored` can start it by accident: it creates billable
/// resources. See this module's documentation for the variables it needs.
#[cfg(feature = "gcp-live")]
#[tokio::test]
#[ignore = "creates real, billable GCP resources"]
async fn live_provision_and_destroy() {
    use crate::clock::{SystemClock, SystemTimer, SystemWallClock};
    use crate::http::LiveTransport;

    fn required(name: &str) -> String {
        std::env::var(name).unwrap_or_else(|_| panic!("the live test needs `{name}`"))
    }

    let key = ServiceAccountKey::parse(&required("FLYCO_GCP_SERVICE_ACCOUNT_JSON"))
        .expect("the service-account key parses");
    let zone = required("FLYCO_GCP_ZONE");

    let mut gcp = GcpProvider::with_parts(
        LiveTransport::new(),
        SystemClock::new(),
        SystemTimer::new(),
        SystemWallClock::new(),
        key,
        GcpWorkspace::new().with_zones(vec![zone.clone()]),
    );

    let catalog = gcp.catalog().await.expect("read the live catalog");
    assert!(
        !catalog.is_empty(),
        "the project can deploy nothing in the zone it was pointed at"
    );

    let cheapest = catalog
        .iter()
        .filter_map(|entry| entry.pricing.hourly(true).map(|price| (price, entry)))
        .min_by_key(|(price, _)| *price)
        .expect("a priced machine type")
        .1
        .machine_type
        .clone();

    let provision = request_in(MachineId::generate(), &zone, &cheapest, true);
    let machine = gcp.provision(&provision).await.expect("provision");
    gcp.destroy(&machine).await.expect("destroy");
}
