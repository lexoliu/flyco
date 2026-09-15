//! What the Codespaces driver actually puts on the wire.
//!
//! No credentials exist here and no test may create a cloud resource, so
//! every exchange is recorded: the driver is handed a scripted list of
//! responses and the assertions are on the requests that come back out —
//! the exact URL, the exact bearer token, the exact JSON body.

use flyco_core::machine::{CloudProviderKind, MachineSpec, MachineState, Runtime};
use flyco_core::{MachineId, PermissionMode, SessionId, Usd};
use serde_json::Value;

use super::{CodespacesProvider, ENV_REPO_NAME, ensure_environment, included_core_hours};
use crate::http::{HttpRequest, HttpResponse, Method};
use crate::testing::{RecordedTransport, RecordingTimer};
use crate::{
    CapacityMode, ClaudeCredential, CloudProvider, DaemonBootstrap, HarnessCredential, Machine,
    ProviderError, ProvisionRequest, Provisioning,
};

const TOKEN: &str = "gho_the-link-token";
const OWNER: &str = "octocat";
const ENV_REPO: &str = "octocat/flyco-sessions";
const ENV_REPO_ID: u64 = 1_296_269;
const INCLUDED_CORE_HOURS: u32 = 120;
const API: &str = "https://api.github.com";

const MACHINE_TYPE: &str = "standardLinux32gb";
const GEO: &str = "UsEast";
const CODESPACE: &str = "octocat-flyco-sessions-xr7g2p4k9";

/// 2026-08-29T12:00:00Z.
const BILLED_AT: u64 = 1_788_004_800;
/// 2026-08-01T00:00:00Z.
const MONTH_START: u64 = 1_785_542_400;

const MACHINES: &str = include_str!("../../fixtures/codespaces/machines.json");
const CODESPACE_PROVISIONING: &str =
    include_str!("../../fixtures/codespaces/codespace_provisioning.json");
const CODESPACE_AVAILABLE: &str =
    include_str!("../../fixtures/codespaces/codespace_available.json");
const CODESPACE_SHUTDOWN: &str = include_str!("../../fixtures/codespaces/codespace_shutdown.json");
const USER: &str = include_str!("../../fixtures/codespaces/user.json");
const REPO_PRIVATE: &str = include_str!("../../fixtures/codespaces/repo_private.json");
const REPO_PUBLIC: &str = include_str!("../../fixtures/codespaces/repo_public.json");
const USAGE: &str = include_str!("../../fixtures/codespaces/usage.json");
const ERROR_VALIDATION: &str = include_str!("../../fixtures/codespaces/error_validation.json");

/// A driver over a scripted transport and a timer that records rather than
/// waits.
type Recorded = CodespacesProvider<RecordedTransport, RecordingTimer>;

fn provider_for(responses: Vec<HttpResponse>) -> Recorded {
    CodespacesProvider::with_parts(
        RecordedTransport::new(responses),
        RecordingTimer::new(),
        TOKEN,
        ENV_REPO,
        ENV_REPO_ID,
        INCLUDED_CORE_HOURS,
    )
}

fn json(body: &str) -> HttpResponse {
    HttpResponse::new(200, body.as_bytes().to_vec())
}

fn refused(status: u16, body: &str) -> HttpResponse {
    HttpResponse::new(status, body.as_bytes().to_vec())
}

fn empty(status: u16) -> HttpResponse {
    HttpResponse::new(status, Vec::new())
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

fn request_spec(machine: MachineId) -> ProvisionRequest {
    ProvisionRequest {
        machine,
        spec: MachineSpec {
            provider: CloudProviderKind::Codespaces,
            machine_type: MACHINE_TYPE.to_owned(),
            runtime: Runtime::Vm,
            region: GEO.to_owned(),
            spot: false,
            disk_gib: 32,
        },
        bootstrap: DaemonBootstrap {
            session: SessionId::generate(),
            provider: CloudProviderKind::Codespaces,
            runtime: Runtime::Vm,
            control_plane_url: "https://flyco.dev/".to_owned(),
            daemon_token: "fd_a-live-daemon-token".to_owned(),
            permission_mode: PermissionMode::Default,
            auth: HarnessCredential::ClaudeCode(ClaudeCredential::Inherit),
            repo: crate::testing::checkout(),
            machine_origin: flyco_core::MachineOrigin::Auto,
            machine: crate::testing::session_machine(),
            resume_session_id: None,
            model: crate::testing::session_model(),
            mcp_servers: crate::testing::mcp_servers(),
        },
    }
}

fn running(machine: MachineId) -> Machine {
    Machine {
        id: machine,
        native_id: CODESPACE.to_owned(),
        runtime: Runtime::Vm,
        region: GEO.to_owned(),
        state: MachineState::Running,
        capacity_mode: CapacityMode::OnDemand,
        address: Some(format!("https://github.com/codespaces/{CODESPACE}")),
    }
}

fn stopped(machine: MachineId) -> Machine {
    Machine {
        state: MachineState::Deallocated,
        ..running(machine)
    }
}

#[tokio::test]
async fn the_catalog_lists_every_machine_type_in_every_geography() {
    let mut provider = provider_for(vec![json(MACHINES)]);
    let catalog = provider.catalog().await.expect("a catalog");

    assert_eq!(catalog.len(), 16, "four machine types, four geographies");
    let standard = catalog
        .iter()
        .find(|entry| entry.machine_type == MACHINE_TYPE && entry.region == GEO)
        .expect("the standard type in UsEast");
    assert_eq!(standard.provider, CloudProviderKind::Codespaces);
    assert_eq!(standard.runtime, Runtime::Vm);
    let capacity = standard.capacity.as_ref().expect("a published size");
    assert_eq!(capacity.vcpus, 4);
    assert_eq!(capacity.memory_mib, 16 * 1024);

    // $0.18/hour for two cores is $0.36 for four; storage is the published
    // $0.07/GiB-month, at 96 micros per GiB-hour.
    let flyco_core::machine::MachinePricing::Metered {
        on_demand_hourly,
        spot_hourly,
        storage,
        ..
    } = &standard.pricing
    else {
        panic!("a codespace is metered");
    };
    assert_eq!(*on_demand_hourly, Usd::from_micros(360_000));
    assert_eq!(*spot_hourly, None);
    let flyco_core::machine::StoragePricing::PerGibHourly { rate } = storage else {
        panic!("codespace storage is billed per GiB-hour");
    };
    assert_eq!(*rate, Usd::from_micros(96));

    let grant = standard.free_grant.expect("the account's grant");
    assert_eq!(grant.vcpu_seconds_per_month, 120 * 3_600);
    // Memory is part of the core-hour, not a second meter.
    assert_eq!(grant.gib_seconds_per_month, u64::MAX);

    let request = provider.transport().request(0);
    assert_eq!(request.method, Method::Get);
    assert_eq!(
        request.url,
        format!("{API}/repos/{ENV_REPO}/codespaces/machines")
    );
    assert_eq!(header(&request, "authorization"), format!("Bearer {TOKEN}"));
    assert_eq!(header(&request, "x-github-api-version"), "2022-11-28");
}

#[tokio::test]
async fn a_provision_creates_the_codespace_and_polls_it_available() {
    let mut provider = provider_for(vec![
        HttpResponse::new(201, CODESPACE_PROVISIONING.as_bytes().to_vec()),
        json(CODESPACE_AVAILABLE),
    ]);
    let machine = MachineId::generate();

    let provisioned = provider
        .provision(&request_spec(machine))
        .await
        .expect("a provision")
        .ready()
        .expect("the codespace is up");

    assert_eq!(provisioned.native_id, CODESPACE);
    assert_eq!(provisioned.state, MachineState::Running);
    assert_eq!(provisioned.runtime, Runtime::Vm);
    assert_eq!(provisioned.region, GEO);
    assert_eq!(provisioned.capacity_mode, CapacityMode::OnDemand);
    assert_eq!(
        provisioned.address.as_deref(),
        Some(format!("https://github.com/codespaces/{CODESPACE}").as_str())
    );

    let create = provider.transport().request(0);
    assert_eq!(create.method, Method::Post);
    assert_eq!(create.url, format!("{API}/user/codespaces"));
    let body = body_of(&create);
    assert_eq!(body["repository_id"], ENV_REPO_ID);
    assert_eq!(body["geo"], GEO);
    assert_eq!(body["machine"], MACHINE_TYPE);
    assert!(
        body["display_name"]
            .as_str()
            .expect("a display name")
            .starts_with("flyco-")
    );
    assert_eq!(body["idle_timeout_minutes"], 30);
    assert_eq!(body["retention_period_minutes"], 43_200);
    assert_eq!(body["devcontainer_path"], ".devcontainer/devcontainer.json");

    let poll = provider.transport().request(1);
    assert_eq!(poll.method, Method::Get);
    assert_eq!(poll.url, format!("{API}/user/codespaces/{CODESPACE}"));
}

#[tokio::test]
async fn a_provision_yields_a_continuation_when_the_build_outlives_the_polls() {
    let mut script = vec![HttpResponse::new(
        201,
        CODESPACE_PROVISIONING.as_bytes().to_vec(),
    )];
    // POLLS_PER_INVOCATION inspections, all still building.
    script.extend((0..12).map(|_| json(CODESPACE_PROVISIONING)));
    let mut provider = provider_for(script);

    let outcome = provider
        .provision(&request_spec(MachineId::generate()))
        .await
        .expect("a provision");

    let Provisioning::Pending {
        machine,
        continuation,
    } = outcome
    else {
        panic!("a build that outlives its polls answers Pending");
    };
    assert_eq!(machine.native_id, CODESPACE);
    assert_eq!(machine.state, MachineState::Provisioning);

    // And the continuation picks up where the invocation left off.
    let mut resumed = provider_for(vec![json(CODESPACE_AVAILABLE)]);
    let provisioned = resumed
        .resume(&machine, &continuation)
        .await
        .expect("a resume")
        .ready()
        .expect("the codespace is up");
    assert_eq!(provisioned.state, MachineState::Running);
    assert_eq!(provisioned.native_id, CODESPACE);
    assert_eq!(provisioned.region, GEO);
}

#[tokio::test]
async fn a_failed_build_is_an_operation_failure() {
    let mut provider = provider_for(vec![
        HttpResponse::new(201, CODESPACE_PROVISIONING.as_bytes().to_vec()),
        json(&CODESPACE_PROVISIONING.replace("\"Provisioning\"", "\"Failed\"")),
    ]);

    let error = provider
        .provision(&request_spec(MachineId::generate()))
        .await
        .expect_err("a failed build fails");
    assert!(
        matches!(error, ProviderError::OperationFailed { .. }),
        "a Failed codespace is an OperationFailed, not a refusal: {error}"
    );
}

#[tokio::test]
async fn a_refusal_lifts_the_errors_code_out_of_the_document() {
    let mut provider = provider_for(vec![refused(422, ERROR_VALIDATION)]);

    let error = provider
        .provision(&request_spec(MachineId::generate()))
        .await
        .expect_err("a refused create fails");
    match error {
        ProviderError::Refused { code, message } => {
            assert_eq!(code, "invalid");
            assert_eq!(message, "Validation Failed");
        }
        other => panic!("a 422 is a Refused, not {other:?}"),
    }
}

#[tokio::test]
async fn a_deallocate_stops_the_codespace_and_waits_for_shutdown() {
    let mut provider = provider_for(vec![json(CODESPACE_SHUTDOWN), json(CODESPACE_SHUTDOWN)]);
    let machine = running(MachineId::generate());

    provider.deallocate(&machine).await.expect("stopped");

    let stop = provider.transport().request(0);
    assert_eq!(stop.method, Method::Post);
    assert_eq!(stop.url, format!("{API}/user/codespaces/{CODESPACE}/stop"));
    let poll = provider.transport().request(1);
    assert_eq!(poll.method, Method::Get);
}

#[tokio::test]
async fn a_start_brings_a_stopped_codespace_back_on_its_disk() {
    let mut provider = provider_for(vec![json(CODESPACE_AVAILABLE), json(CODESPACE_AVAILABLE)]);
    let machine = stopped(MachineId::generate());

    let started = provider.start(&machine).await.expect("started");
    assert_eq!(started.state, MachineState::Running);
    assert_eq!(started.native_id, CODESPACE);

    let start = provider.transport().request(0);
    assert_eq!(start.method, Method::Post);
    assert_eq!(
        start.url,
        format!("{API}/user/codespaces/{CODESPACE}/start")
    );
}

#[tokio::test]
async fn a_start_reports_a_codespace_that_no_longer_exists() {
    let mut provider = provider_for(vec![empty(404)]);
    let machine = stopped(MachineId::generate());

    let error = provider.start(&machine).await.expect_err("it is gone");
    assert!(
        matches!(error, ProviderError::Gone(_)),
        "a deleted codespace is `gone`, which is what the recovery acts on: {error}"
    );
}

#[tokio::test]
async fn a_destroy_releases_the_codespace() {
    let mut provider = provider_for(vec![empty(204)]);
    provider
        .destroy(&running(MachineId::generate()))
        .await
        .expect("destroyed");

    let delete = provider.transport().request(0);
    assert_eq!(delete.method, Method::Delete);
    assert_eq!(delete.url, format!("{API}/user/codespaces/{CODESPACE}"));
}

#[tokio::test]
async fn a_destroy_tolerates_a_codespace_retention_already_deleted() {
    let mut provider = provider_for(vec![empty(404)]);
    provider
        .destroy(&stopped(MachineId::generate()))
        .await
        .expect("gone is destroyed");
}

#[tokio::test]
async fn a_resize_patches_then_restarts_a_running_codespace() {
    let mut provider = provider_for(vec![
        json(CODESPACE_AVAILABLE),
        json(CODESPACE_SHUTDOWN),
        json(CODESPACE_SHUTDOWN),
        json(CODESPACE_AVAILABLE),
        json(CODESPACE_AVAILABLE),
    ]);
    let machine = running(MachineId::generate());

    let resized = provider
        .resize(&machine, "premiumLinux")
        .await
        .expect("resized");
    assert_eq!(resized.state, MachineState::Running);

    let patch = provider.transport().request(0);
    assert_eq!(patch.method, Method::Patch);
    assert_eq!(patch.url, format!("{API}/user/codespaces/{CODESPACE}"));
    assert_eq!(body_of(&patch)["machine"], "premiumLinux");

    // The PATCH applies at the next start, so a running codespace is taken
    // through a full stop/start — the disk survives all of it.
    let requests = (1..5)
        .map(|index| provider.transport().request(index))
        .collect::<Vec<_>>();
    assert_eq!(requests[0].method, Method::Post);
    assert!(requests[0].url.ends_with("/stop"));
    assert_eq!(requests[1].method, Method::Get);
    assert_eq!(requests[2].method, Method::Post);
    assert!(requests[2].url.ends_with("/start"));
    assert_eq!(requests[3].method, Method::Get);
}

#[tokio::test]
async fn a_resize_of_a_stopped_codespace_is_one_patch() {
    let mut provider = provider_for(vec![json(CODESPACE_SHUTDOWN)]);
    let machine = stopped(MachineId::generate());

    let resized = provider
        .resize(&machine, "premiumLinux")
        .await
        .expect("resized");
    assert_eq!(resized.state, MachineState::Deallocated);
    assert_eq!(provider.transport().request_count(), 1);
}

#[tokio::test]
async fn verify_proves_the_scope_and_the_repository() {
    let provider = provider_for(vec![
        json(USER).header("x-oauth-scopes", "repo, codespace"),
        json(REPO_PRIVATE),
    ]);

    let verified = provider.verify().await.expect("the credential works");
    assert_eq!(verified.user.login, OWNER);
    assert_eq!(verified.user.id, 583_231);
    assert_eq!(verified.user.plan.expect("a plan").name, "pro");

    assert_eq!(provider.transport().request(0).url, format!("{API}/user"));
    assert_eq!(
        provider.transport().request(1).url,
        format!("{API}/repos/{ENV_REPO}")
    );
}

#[tokio::test]
async fn verify_refuses_a_token_without_the_codespace_scope() {
    let provider = provider_for(vec![
        json(USER).header("x-oauth-scopes", "repo"),
        json(REPO_PRIVATE),
    ]);

    let error = provider.verify().await.expect_err("unscoped");
    assert!(matches!(error, ProviderError::Rejected(_)));
}

#[tokio::test]
async fn verify_refuses_a_public_environment_repository() {
    let provider = provider_for(vec![
        json(USER).header("x-oauth-scopes", "repo, codespace"),
        json(REPO_PUBLIC),
    ]);

    let error = provider.verify().await.expect_err("public");
    assert!(matches!(error, ProviderError::Rejected(_)));
}

#[tokio::test]
async fn an_inspect_answers_none_for_a_codespace_that_is_gone() {
    let provider = provider_for(vec![empty(404)]);
    assert!(
        provider
            .inspect(CODESPACE)
            .await
            .expect("the read works")
            .is_none()
    );
}

#[tokio::test]
async fn the_billing_read_sums_only_codespaces_items() {
    let provider = provider_for(vec![json(USAGE)]);
    let spend = provider
        .billing_period_cost(BILLED_AT)
        .await
        .expect("a read")
        .expect("the account has enhanced billing");

    assert_eq!(spend.spent, Usd::from_micros(450_000));
    assert_eq!(spend.period_start_unix, MONTH_START);
    assert_eq!(spend.period_end_unix, BILLED_AT);

    let request = provider.transport().request(0);
    assert_eq!(
        request.url,
        format!("{API}/users/{OWNER}/settings/billing/usage?year=2026&month=8&product=codespaces")
    );
}

#[tokio::test]
async fn the_billing_read_is_silent_without_enhanced_billing() {
    let provider = provider_for(vec![empty(403)]);
    assert!(
        provider
            .billing_period_cost(BILLED_AT)
            .await
            .expect("the read itself works")
            .is_none()
    );
}

#[test]
fn the_plan_sets_the_included_core_hours() {
    assert_eq!(included_core_hours(Some("pro")), 180);
    assert_eq!(included_core_hours(Some("free")), 120);
    // An account the API did not name a plan for reads as the free tier:
    // overstating a grant would let a budget plan on credit that is not
    // there.
    assert_eq!(included_core_hours(None), 120);
}

#[tokio::test]
async fn ensure_environment_creates_the_repository_and_its_devcontainer() {
    let transport = RecordedTransport::new(vec![
        empty(404),
        HttpResponse::new(201, REPO_PRIVATE.as_bytes().to_vec()),
        HttpResponse::new(201, b"{}".to_vec()),
    ]);
    let devcontainer =
        "{\"image\":\"ghcr.io/flyco/session:latest\",\"postStartCommand\":\"flycod codespace\"}";

    let repo = ensure_environment(&transport, TOKEN, OWNER, devcontainer)
        .await
        .expect("the environment is ensured");

    assert_eq!(repo.id, ENV_REPO_ID);
    assert_eq!(repo.full_name, ENV_REPO);

    let lookup = transport.request(0);
    assert_eq!(lookup.method, Method::Get);
    assert_eq!(lookup.url, format!("{API}/repos/{OWNER}/{ENV_REPO_NAME}"));

    let create = transport.request(1);
    assert_eq!(create.method, Method::Post);
    assert_eq!(create.url, format!("{API}/user/repos"));
    let body = body_of(&create);
    assert_eq!(body["name"], ENV_REPO_NAME);
    assert_eq!(body["private"], true);
    assert_eq!(body["auto_init"], true);

    let put = transport.request(2);
    assert_eq!(put.method, Method::Put);
    assert_eq!(
        put.url,
        format!("{API}/repos/{ENV_REPO}/contents/.devcontainer/devcontainer.json")
    );
    let expected = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(devcontainer.as_bytes())
    };
    assert_eq!(body_of(&put)["content"], Value::String(expected));
}

#[tokio::test]
async fn ensure_environment_rewrites_the_devcontainer_it_finds() {
    let transport = RecordedTransport::new(vec![
        json(REPO_PRIVATE),
        HttpResponse::new(200, b"{}".to_vec()),
    ]);

    let repo = ensure_environment(&transport, TOKEN, OWNER, "{}")
        .await
        .expect("ensured");

    assert_eq!(repo.full_name, ENV_REPO);
    // GET, then the contents PUT — an existing repository is never
    // recreated.
    assert_eq!(transport.request_count(), 2);
    assert_eq!(transport.request(1).method, Method::Put);
}

#[tokio::test]
async fn ensure_environment_refuses_a_public_repository() {
    let transport = RecordedTransport::new(vec![json(REPO_PUBLIC)]);

    let error = ensure_environment(&transport, TOKEN, OWNER, "{}")
        .await
        .expect_err("a public repository is refused");
    assert!(matches!(error, ProviderError::Rejected(_)));
    // And nothing was written to it.
    assert_eq!(transport.request_count(), 1);
}
