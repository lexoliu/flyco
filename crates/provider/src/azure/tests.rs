//! What the Azure driver actually puts on the wire.
//!
//! No credentials exist here and no test may create a cloud resource, so
//! every exchange is recorded: the driver is handed a scripted list of
//! responses and the assertions are on the requests that come back out —
//! the exact URL, the exact `api-version`, the exact JSON body. That is the
//! only property of a provisioning driver that can be checked without a
//! subscription, and it is the one that breaks silently.
//!
//! The fixtures describe **policy-allowed** regions (`northcentralus`,
//! `canadacentral`) rather than the region with the most machine types,
//! because the latter is forbidden outright on the reference subscription
//! and a fixture built from it would exercise a path that cannot run there.
//!
//! # Running it against a real subscription
//!
//! There is a live test, `live_provision_and_destroy`, behind the
//! `azure-live` feature, which is off by default and which CI never enables.
//! It creates and destroys real, billable resources. To run it:
//!
//! ```text
//! export FLYCO_AZURE_TENANT_ID=…        FLYCO_AZURE_CLIENT_ID=…
//! export FLYCO_AZURE_CLIENT_SECRET=…    FLYCO_AZURE_SUBSCRIPTION_ID=…
//! export FLYCO_AZURE_RESOURCE_GROUP=flyco-rg
//! export FLYCO_AZURE_REGION=northcentralus
//! export FLYCO_AZURE_SSH_PUBLIC_KEY="$(cat ~/.ssh/id_ed25519.pub)"
//! cargo test -p flyco-provider --features azure-live -- --ignored --nocapture
//! ```
//!
//! The resource group must exist first, in a region the subscription's
//! policy allows, and the service principal must be `Contributor` on it —
//! no resource-group-scoped role can create the group it is scoped to.

use flyco_core::machine::{CloudProviderKind, MachineSpec, MachineState};
use flyco_core::{HarnessKind, MachineId, PermissionMode, SessionId};
use serde_json::Value;

use super::{
    ADMIN_USERNAME, AzureProvider, ExclusionReason, IMAGE_SKU_ARM64, IMAGE_SKU_X64,
    SPOT_UNSUPPORTED_CODES, Workspace, names,
};
use crate::azure::auth::ServicePrincipal;
use crate::clock::ManualClock;
use crate::cloud_init::CONFIG_PATH;
use crate::http::{HttpRequest, HttpResponse, Method};
use crate::testing::{RecordedTransport, RecordingTimer};
use crate::{
    CapacityMode, ClaudeCredential, CloudProvider, DaemonBootstrap, Machine, ProviderError,
    ProvisionRequest,
};

const SUBSCRIPTION: &str = "e47d07d8-2715-4909-aa56-1bfde801bdf0";
const TENANT: &str = "f9dd8f4f-3b8b-4768-aba7-bbd379e0736b";
const RESOURCE_GROUP: &str = "flyco-rg";
const SSH_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFlycoTestKey lexo@flyco";

/// A region the reference subscription's policy allows.
const REGION: &str = "northcentralus";

/// Another allowed region, whose small unrestricted machine types are Arm64.
const ARM64_REGION: &str = "canadacentral";

/// The region with the most deployable machine types, and the one the
/// subscription's policy forbids outright.
const FORBIDDEN_REGION: &str = "westus2";

const X64_TYPE: &str = "Standard_D2als_v6";
const ARM64_TYPE: &str = "Standard_D2pls_v5";
const ZONE_RESTRICTED_TYPE: &str = "Standard_B2ats_v2";
const NO_FAMILY_QUOTA_TYPE: &str = "Standard_D2ads_v5";

const POLICY: &str = include_str!("../../fixtures/azure/policy_assignments.json");
const NO_POLICY: &str = include_str!("../../fixtures/azure/policy_assignments_none.json");
const SKUS: &str = include_str!("../../fixtures/azure/skus_northcentralus.json");
const ARM64_SKUS: &str = include_str!("../../fixtures/azure/skus_canadacentral.json");
const USAGES: &str = include_str!("../../fixtures/azure/usages.json");
const LOW_PRIORITY_SPENT: &str =
    include_str!("../../fixtures/azure/usages_low_priority_spent.json");
const PRICES: &str = include_str!("../../fixtures/azure/retail_prices.json");
const STORAGE_PRICES: &str = include_str!("../../fixtures/azure/standard_ssd_prices.json");
const COST: &str = include_str!("../../fixtures/azure/cost_month_to_date.json");
const COST_IN_EUROS: &str = include_str!("../../fixtures/azure/cost_month_to_date_euros.json");
const COST_EMPTY: &str = include_str!("../../fixtures/azure/cost_month_to_date_empty.json");

/// 2026-08-29T12:00:00Z, an instant inside the month `MonthToDate` covers.
const QUERIED_AT: u64 = 1_788_004_800;

/// 2026-08-01T00:00:00Z: midnight UTC on the first of that month.
const MONTH_START: u64 = 1_785_542_400;

/// A driver over a scripted transport, a clock at zero and a timer that
/// records rather than waits.
type Recorded = AzureProvider<RecordedTransport, ManualClock, RecordingTimer>;

fn principal() -> ServicePrincipal {
    ServicePrincipal {
        tenant_id: TENANT.to_owned(),
        client_id: "app-id".to_owned(),
        client_secret: "app-secret".to_owned(),
        subscription_id: SUBSCRIPTION.to_owned(),
    }
}

fn provider(responses: Vec<HttpResponse>) -> Recorded {
    provider_over(Workspace::new(RESOURCE_GROUP, SSH_KEY), responses)
}

fn provider_over(workspace: Workspace, responses: Vec<HttpResponse>) -> Recorded {
    AzureProvider::with_parts(
        RecordedTransport::new(responses),
        ManualClock::new(),
        RecordingTimer::new(),
        principal(),
        workspace,
    )
}

fn json(status: u16, body: &str) -> HttpResponse {
    HttpResponse::new(status, body.as_bytes().to_vec())
}

fn token() -> HttpResponse {
    json(
        200,
        r#"{"token_type":"Bearer","expires_in":3599,"access_token":"eyJ0eXAi.stub"}"#,
    )
}

/// A synchronous `200`, which is what ARM answers for a create-or-update
/// that finished inline.
fn done() -> HttpResponse {
    json(200, "{}")
}

fn request_in(
    machine: MachineId,
    region: &str,
    machine_type: &str,
    spot: bool,
) -> ProvisionRequest {
    ProvisionRequest {
        machine,
        spec: MachineSpec {
            provider: CloudProviderKind::Azure,
            machine_type: machine_type.to_owned(),
            region: region.to_owned(),
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
    request_in(machine, REGION, machine_type, spot)
}

/// The responses a clean provisioning run consumes, in order.
///
/// Index 0 is the token exchange, 1 the policy read, 2 the SKU list and 3
/// the quota list; the writes start at 4 and the machine's own `PUT` is
/// [`MACHINE_PUT`].
fn provision_script(vm: Vec<HttpResponse>) -> Vec<HttpResponse> {
    let mut script = vec![
        token(),
        json(200, POLICY),
        json(200, SKUS),
        json(200, USAGES),
        done(), // virtual network
        done(), // network security group
        done(), // public IP
        done(), // network interface
    ];
    script.extend(vm);
    script
}

/// Index of the virtual machine's own `PUT` in a [`provision_script`] run.
const MACHINE_PUT: usize = 8;

fn body_of(request: &HttpRequest) -> Value {
    serde_json::from_slice(&request.body).expect("the driver sends a JSON body")
}

fn provisioned(machine: MachineId) -> Machine {
    Machine {
        id: machine,
        native_id: format!(
            "/subscriptions/{SUBSCRIPTION}/resourceGroups/{RESOURCE_GROUP}\
             /providers/Microsoft.Compute/virtualMachines/{}",
            names::machine(machine)
        ),
        region: REGION.to_owned(),
        state: MachineState::Running,
        capacity_mode: CapacityMode::OnDemand,
        address: Some(names::fqdn(machine, REGION)),
    }
}

// ── The three gates ──

#[tokio::test]
async fn provisioning_reads_the_policy_availability_and_quota_before_it_writes() {
    let mut azure = provider(provision_script(vec![done()]));
    azure
        .provision(&request(MachineId::generate(), X64_TYPE, false))
        .await
        .expect("provision");

    let transport = azure.transport();

    // The policy gate is invisible from the SKU list, so it is read.
    assert_eq!(
        transport.request(1).url,
        format!(
            "https://management.azure.com/subscriptions/{SUBSCRIPTION}\
             /providers/Microsoft.Authorization/policyAssignments?api-version=2024-04-01"
        )
    );

    assert!(
        transport
            .request(2)
            .url
            .contains("/providers/Microsoft.Compute/skus?")
    );
    assert!(
        transport.request(2).url.contains("api-version=2021-07-01"),
        "the SKUs list is read at the pinned version"
    );
    assert!(
        transport
            .request(2)
            .url
            .contains("location+eq+%27northcentralus%27"),
        "the SKU list is filtered to the region: {}",
        transport.request(2).url
    );

    assert_eq!(
        transport.request(3).url,
        format!(
            "https://management.azure.com/subscriptions/{SUBSCRIPTION}\
             /providers/Microsoft.Compute/locations/{REGION}/usages?api-version=2024-11-01"
        )
    );

    for index in 4..=MACHINE_PUT {
        assert_ne!(
            transport.request(index).method,
            Method::Get,
            "request {index} should be a write"
        );
    }
}

#[tokio::test]
async fn a_region_the_subscriptions_policy_forbids_is_refused_before_any_read() {
    // The SKU list reports this region's machine types as perfectly
    // available; the policy is an independent gate, and what would actually
    // fail is the *virtual network* `PUT`, with an error naming neither the
    // region nor the machine.
    let mut azure = provider(vec![token(), json(200, POLICY)]);

    let error = azure
        .provision(&request_in(
            MachineId::generate(),
            FORBIDDEN_REGION,
            X64_TYPE,
            false,
        ))
        .await
        .expect_err("a policy-forbidden region cannot be deployed into");

    let ProviderError::Unavailable { reason, .. } = &error else {
        panic!("a forbidden region is an availability failure: {error}");
    };
    assert!(
        reason.contains("Allowed resource deployment regions"),
        "the refusal names the policy, so the user knows which problem this is: {reason}"
    );
    assert_eq!(
        azure.transport().request_count(),
        2,
        "no SKU or quota read happens for a region that cannot be used at all"
    );
}

#[tokio::test]
async fn a_subscription_with_no_policy_may_deploy_where_it_likes() {
    let mut azure = provider(vec![
        token(),
        json(200, NO_POLICY),
        json(200, SKUS),
        json(200, USAGES),
        done(),
        done(),
        done(),
        done(),
        done(),
    ]);

    azure
        .provision(&request_in(
            MachineId::generate(),
            FORBIDDEN_REGION,
            X64_TYPE,
            false,
        ))
        .await
        .expect("no assignment means every region, not no region");
}

#[tokio::test]
async fn the_policy_is_read_once_per_driver() {
    let mut azure = provider(provision_script(vec![done(), done()]));
    let machine = MachineId::generate();
    azure
        .provision(&request(machine, X64_TYPE, false))
        .await
        .expect("provision");
    azure
        .deallocate(&provisioned(machine))
        .await
        .expect("deallocate");

    let transport = azure.transport();
    let policy_reads = (0..transport.request_count())
        .filter(|index| transport.request(*index).url.contains("policyAssignments"))
        .count();
    assert_eq!(
        policy_reads, 1,
        "a policy assignment changes on a human timescale; reading it per resource is waste"
    );
}

#[tokio::test]
async fn a_location_restricted_machine_type_is_refused_before_any_write() {
    let mut azure = provider(vec![token(), json(200, POLICY), json(200, SKUS)]);

    let error = azure
        .provision(&request(MachineId::generate(), "Standard_B2ls_v2", false))
        .await
        .expect_err("a location-restricted SKU cannot be deployed");
    assert!(matches!(error, ProviderError::Unavailable { .. }));
    assert_eq!(
        azure.transport().request_count(),
        3,
        "the quota read never happens: availability already settled it"
    );
}

#[tokio::test]
async fn a_machine_type_with_no_family_quota_is_refused_on_demand() {
    let mut azure = provider(vec![
        token(),
        json(200, POLICY),
        json(200, SKUS),
        json(200, USAGES),
    ]);

    let error = azure
        .provision(&request(MachineId::generate(), NO_FAMILY_QUOTA_TYPE, false))
        .await
        .expect_err("a family with a zero limit cannot take an on-demand machine");
    assert!(matches!(error, ProviderError::QuotaExceeded { .. }));
    assert_eq!(azure.transport().request_count(), 4);
}

#[tokio::test]
async fn the_same_machine_type_is_allowed_as_spot_because_spot_has_its_own_pool() {
    // Spot bypasses per-family quota entirely and spends only
    // `lowPriorityCores`. Checking it against the family quota would refuse
    // machines the subscription can genuinely run — which, on an account
    // whose families are mostly zero-limit, is nearly all of them.
    let mut azure = provider(provision_script(vec![done()]));

    let machine = azure
        .provision(&request(MachineId::generate(), NO_FAMILY_QUOTA_TYPE, true))
        .await
        .expect("spot draws on a different pool");
    assert_eq!(machine.capacity_mode, CapacityMode::Spot);
}

#[tokio::test]
async fn a_spent_low_priority_pool_refuses_spot() {
    let mut azure = provider(vec![
        token(),
        json(200, POLICY),
        json(200, SKUS),
        json(200, LOW_PRIORITY_SPENT),
    ]);

    let error = azure
        .provision(&request(MachineId::generate(), X64_TYPE, true))
        .await
        .expect_err("two of three spot vCPUs are already spent");
    assert!(matches!(
        error,
        ProviderError::QuotaExceeded { ref quota, .. } if quota == "lowPriorityCores"
    ));
}

#[tokio::test]
async fn a_machine_type_the_region_does_not_list_is_refused() {
    let mut azure = provider(vec![token(), json(200, POLICY), json(200, SKUS)]);

    let error = azure
        .provision(&request(MachineId::generate(), "Standard_NotReal", false))
        .await
        .expect_err("an unknown machine type cannot be deployed");
    assert!(matches!(error, ProviderError::Unavailable { .. }));
}

// ── The provisioning sequence ──

#[tokio::test]
async fn the_workspace_network_and_security_group_come_before_the_session_resources() {
    let mut azure = provider(provision_script(vec![done()]));
    azure
        .provision(&request(MachineId::generate(), X64_TYPE, false))
        .await
        .expect("provision");

    let transport = azure.transport();
    let vnet = transport.request(4);
    assert_eq!(vnet.method, Method::Put);
    assert!(vnet.url.ends_with(
        "/providers/Microsoft.Network/virtualNetworks/flyco-northcentralus-vnet\
         ?api-version=2024-05-01"
    ));
    let body = body_of(&vnet);
    assert_eq!(body["location"], REGION);
    assert_eq!(
        body["properties"]["addressSpace"]["addressPrefixes"][0],
        "10.42.0.0/16"
    );
    assert_eq!(body["properties"]["subnets"][0]["name"], "default");
    assert_eq!(
        body["properties"]["subnets"][0]["properties"]["addressPrefix"],
        "10.42.0.0/24"
    );

    // The security group is mandatory: a Standard public IP is closed by
    // default and a machine without this provisions cleanly and answers
    // nothing.
    let nsg = transport.request(5);
    assert_eq!(nsg.method, Method::Put);
    assert!(
        nsg.url
            .contains("/networkSecurityGroups/flyco-northcentralus-nsg?")
    );
    let rule = &body_of(&nsg)["properties"]["securityRules"][0]["properties"];
    assert_eq!(rule["protocol"], "Tcp");
    assert_eq!(rule["destinationPortRange"], "22");
    assert_eq!(rule["access"], "Allow");
    assert_eq!(rule["direction"], "Inbound");
    assert_eq!(rule["priority"], 1_000);
}

#[tokio::test]
async fn the_public_ip_is_standard_static_and_named_for_dns() {
    let machine = MachineId::generate();
    let mut azure = provider(provision_script(vec![done()]));
    azure
        .provision(&request(machine, X64_TYPE, false))
        .await
        .expect("provision");

    let pip = azure.transport().request(6);
    assert_eq!(pip.method, Method::Put);
    assert!(pip.url.contains(&format!(
        "/providers/Microsoft.Network/publicIPAddresses/{}?api-version=2024-05-01",
        names::public_ip(machine)
    )));

    let body = body_of(&pip);
    assert_eq!(body["sku"]["name"], "Standard");
    assert_eq!(body["sku"]["tier"], "Regional");
    assert_eq!(body["properties"]["publicIPAllocationMethod"], "Static");
    assert_eq!(
        body["properties"]["dnsSettings"]["domainNameLabel"],
        names::dns_label(machine)
    );
    assert!(
        body.get("zones").is_none(),
        "a zonal public IP fails on the machine types this subscription can run"
    );
}

#[tokio::test]
async fn the_interface_joins_the_subnet_the_address_and_the_security_group() {
    let machine = MachineId::generate();
    let mut azure = provider(provision_script(vec![done()]));
    azure
        .provision(&request(machine, X64_TYPE, false))
        .await
        .expect("provision");

    let nic = azure.transport().request(7);
    assert_eq!(nic.method, Method::Put);
    let body = body_of(&nic);
    let properties = &body["properties"];

    assert!(
        properties["networkSecurityGroup"]["id"]
            .as_str()
            .expect("an id")
            .ends_with("/networkSecurityGroups/flyco-northcentralus-nsg")
    );
    let configuration = &properties["ipConfigurations"][0]["properties"];
    assert_eq!(configuration["primary"], true);
    assert!(
        configuration["subnet"]["id"]
            .as_str()
            .expect("an id")
            .ends_with("/virtualNetworks/flyco-northcentralus-vnet/subnets/default")
    );
    assert!(
        configuration["publicIPAddress"]["id"]
            .as_str()
            .expect("an id")
            .ends_with(&format!("/publicIPAddresses/{}", names::public_ip(machine)))
    );
    // Detach, so a rebuilt machine keeps the address and its DNS label.
    assert_eq!(
        configuration["publicIPAddress"]["properties"]["deleteOption"],
        "Detach"
    );
}

#[tokio::test]
async fn the_machine_body_is_the_measured_shape() {
    let machine = MachineId::generate();
    let provision = request(machine, X64_TYPE, true);
    let session = provision.bootstrap.session;
    let mut azure = provider(provision_script(vec![done()]));
    azure.provision(&provision).await.expect("provision");

    let vm = azure.transport().request(MACHINE_PUT);
    assert_eq!(vm.method, Method::Put);
    assert!(vm.url.ends_with(&format!(
        "/providers/Microsoft.Compute/virtualMachines/{}?api-version=2024-11-01",
        names::machine(machine)
    )));

    let body = body_of(&vm);
    assert_eq!(body["tags"]["owner"], "flyco");
    assert_eq!(body["tags"]["session"], session.to_string());
    assert!(
        body.get("zones").is_none(),
        "`zones` is never sent: zone-restricted-but-usable is the common case"
    );

    let properties = &body["properties"];
    assert_eq!(properties["priority"], "Spot");
    assert_eq!(properties["evictionPolicy"], "Deallocate");
    assert_eq!(properties["billingProfile"]["maxPrice"], -1);
    assert_eq!(properties["hardwareProfile"]["vmSize"], X64_TYPE);

    let storage = &properties["storageProfile"];
    assert_eq!(storage["imageReference"]["publisher"], "Canonical");
    assert_eq!(storage["imageReference"]["offer"], "ubuntu-24_04-lts");
    assert_eq!(storage["imageReference"]["version"], "latest");
    assert_eq!(storage["osDisk"]["diskSizeGB"], 30);
    assert_eq!(storage["osDisk"]["createOption"], "FromImage");
    assert_eq!(storage["osDisk"]["deleteOption"], "Detach");
    assert_eq!(storage["osDisk"]["name"], names::os_disk(machine));

    let os = &properties["osProfile"];
    assert_eq!(os["adminUsername"], ADMIN_USERNAME);
    assert_eq!(os["allowExtensionOperations"], false);
    assert_eq!(
        os["linuxConfiguration"]["disablePasswordAuthentication"],
        true
    );
    assert_eq!(
        os["linuxConfiguration"]["ssh"]["publicKeys"][0]["keyData"],
        SSH_KEY
    );
    assert!(
        os.get("adminPassword").is_none(),
        "flyco never sets a password on a machine it provisions"
    );

    assert_eq!(
        properties["diagnosticsProfile"]["bootDiagnostics"]["enabled"],
        true
    );
}

#[tokio::test]
async fn the_image_sku_follows_the_machine_types_instruction_set() {
    // x64 in one allowed region, Arm64 in another. Hardcoding either would
    // make one of the two regions unusable.
    for (region, skus, machine_type, expected) in [
        (REGION, SKUS, X64_TYPE, IMAGE_SKU_X64),
        (ARM64_REGION, ARM64_SKUS, ARM64_TYPE, IMAGE_SKU_ARM64),
    ] {
        let mut azure = provider(vec![
            token(),
            json(200, POLICY),
            json(200, skus),
            json(200, USAGES),
            done(),
            done(),
            done(),
            done(),
            done(),
        ]);
        azure
            .provision(&request_in(
                MachineId::generate(),
                region,
                machine_type,
                false,
            ))
            .await
            .expect("provision");

        let body = body_of(&azure.transport().request(MACHINE_PUT));
        assert_eq!(
            body["properties"]["storageProfile"]["imageReference"]["sku"], expected,
            "`{machine_type}` must boot the `{expected}` image"
        );
    }
}

#[tokio::test]
async fn cloud_init_carries_the_daemon_configuration_and_nothing_readable() {
    use base64::Engine as _;

    let provision = request(MachineId::generate(), X64_TYPE, false);
    let mut azure = provider(provision_script(vec![done()]));
    azure.provision(&provision).await.expect("provision");

    let body = body_of(&azure.transport().request(MACHINE_PUT));
    let custom_data = body["properties"]["osProfile"]["customData"]
        .as_str()
        .expect("customData is a string");

    // The token must not be legible in the request itself.
    let raw = String::from_utf8(azure.transport().request(MACHINE_PUT).body).expect("UTF-8");
    assert!(!raw.contains("fd_a-live-daemon-token"));

    let cloud_config = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(custom_data)
            .expect("customData is base64"),
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

fn spot_refusal(code: &str) -> HttpResponse {
    json(
        400,
        &serde_json::to_string(&serde_json::json!({
            "error": { "code": code, "message": "spot is not available here" }
        }))
        .expect("serialize"),
    )
}

#[tokio::test]
async fn a_spot_refusal_is_retried_as_on_demand_for_both_codes() {
    for code in SPOT_UNSUPPORTED_CODES {
        let mut azure = provider(provision_script(vec![spot_refusal(code), done()]));

        let provisioned = azure
            .provision(&request(MachineId::generate(), X64_TYPE, true))
            .await
            .unwrap_or_else(|error| panic!("`{code}` must fall back, not fail: {error}"));

        assert_eq!(
            provisioned.capacity_mode,
            CapacityMode::OnDemand,
            "the machine records the capacity it got, not the one it asked for"
        );

        let first = body_of(&azure.transport().request(MACHINE_PUT));
        let second = body_of(&azure.transport().request(MACHINE_PUT + 1));
        assert_eq!(first["properties"]["priority"], "Spot");

        // The identical body, minus exactly the three spot fields: Azure
        // rejects any partial combination of them.
        for field in ["priority", "evictionPolicy", "billingProfile"] {
            assert!(
                second["properties"].get(field).is_none(),
                "`{field}` must come off for the on-demand retry"
            );
        }
        assert_eq!(
            second["properties"]["osProfile"], first["properties"]["osProfile"],
            "the retry is the same machine, not a different one"
        );
    }
}

#[tokio::test]
async fn the_on_demand_fallback_re_checks_the_pools_it_would_spend() {
    // The machine passed the spot check on `lowPriorityCores`; its family
    // quota is zero, so re-sending it as on-demand would fail later with an
    // opaque error. Refuse it here, where the reason is nameable.
    let mut azure = provider(provision_script(vec![spot_refusal(
        SPOT_UNSUPPORTED_CODES[0],
    )]));

    let error = azure
        .provision(&request(MachineId::generate(), NO_FAMILY_QUOTA_TYPE, true))
        .await
        .expect_err("the on-demand pools cannot fund this machine");
    assert!(matches!(error, ProviderError::QuotaExceeded { .. }));
    assert_eq!(
        azure.transport().request_count(),
        MACHINE_PUT + 1,
        "no second machine `PUT` is attempted"
    );
}

#[tokio::test]
async fn a_spot_machine_that_azure_accepts_records_spot() {
    let mut azure = provider(provision_script(vec![done()]));
    let machine = azure
        .provision(&request(MachineId::generate(), X64_TYPE, true))
        .await
        .expect("provision");

    assert_eq!(machine.capacity_mode, CapacityMode::Spot);
    assert_eq!(azure.transport().request_count(), MACHINE_PUT + 1);
}

#[tokio::test]
async fn a_refusal_that_is_not_about_spot_is_not_retried() {
    let mut azure = provider(provision_script(vec![spot_refusal("SkuNotAvailable")]));

    let error = azure
        .provision(&request(MachineId::generate(), X64_TYPE, true))
        .await
        .expect_err("a capacity failure is a failure");
    assert_eq!(error.code(), Some("SkuNotAvailable"));
    assert_eq!(
        azure.transport().request_count(),
        MACHINE_PUT + 1,
        "there must be no second machine `PUT`"
    );
}

#[tokio::test]
async fn an_on_demand_request_never_carries_the_spot_fields() {
    let mut azure = provider(provision_script(vec![done()]));
    azure
        .provision(&request(MachineId::generate(), X64_TYPE, false))
        .await
        .expect("provision");

    let body = body_of(&azure.transport().request(MACHINE_PUT));
    for field in ["priority", "evictionPolicy", "billingProfile"] {
        assert!(body["properties"].get(field).is_none());
    }
}

// ── The asynchronous-operation protocol ──

fn accepted_with_operation(retry_after: Option<&str>) -> HttpResponse {
    let response = HttpResponse::new(201, Vec::new()).header(
        "Azure-AsyncOperation",
        "https://management.azure.com/subscriptions/s/providers/Microsoft.Compute/operations/op-1",
    );
    match retry_after {
        Some(seconds) => response.header("Retry-After", seconds),
        None => response,
    }
}

fn operation(status: &str) -> HttpResponse {
    json(
        200,
        &serde_json::to_string(&serde_json::json!({
            "name": "op-1",
            "status": status,
        }))
        .expect("serialize"),
    )
}

/// An in-flight poll that states its own `Retry-After`.
fn operation_after(status: &str, retry_after: &str) -> HttpResponse {
    operation(status).header("Retry-After", retry_after)
}

#[tokio::test]
async fn an_operation_is_polled_until_it_reaches_a_terminal_status() {
    // Measured: a spot machine sat in `Creating` for over five minutes, so
    // the poller must be willing to wait rather than assume a minute.
    let mut azure = provider(provision_script(vec![
        accepted_with_operation(Some("7")),
        // Resource providers return their own in-flight values; none of
        // these may be read as done.
        operation_after("Accepted", "11"),
        operation("InProgress"),
        operation("Running"),
        operation("Succeeded"),
    ]));

    azure
        .provision(&request(MachineId::generate(), X64_TYPE, false))
        .await
        .expect("provision");

    let transport = azure.transport();
    assert_eq!(transport.request_count(), MACHINE_PUT + 5);
    for index in (MACHINE_PUT + 1)..(MACHINE_PUT + 5) {
        assert_eq!(transport.request(index).method, Method::Get);
        assert!(transport.request(index).url.ends_with("/operations/op-1"));
    }
    assert_eq!(
        azure.timer().delays(),
        vec![7, 11, 5, 10],
        "each response's own `Retry-After` wins, and the backoff grows where none is stated"
    );
}

#[tokio::test]
async fn a_failed_operation_is_a_failure_even_though_the_call_was_accepted() {
    let mut azure = provider(provision_script(vec![
        accepted_with_operation(None),
        json(
            200,
            r#"{"name":"op-1","status":"Failed","error":{"code":"AllocationFailed","message":"no capacity"}}"#,
        ),
    ]));

    let error = azure
        .provision(&request(MachineId::generate(), X64_TYPE, false))
        .await
        .expect_err("a Failed operation is a failed provision");
    assert!(matches!(
        error,
        ProviderError::OperationFailed { ref code, .. } if code == "AllocationFailed"
    ));
}

#[tokio::test]
async fn a_cancelled_operation_is_terminal_too() {
    let mut azure = provider(provision_script(vec![
        accepted_with_operation(None),
        operation("Canceled"),
    ]));

    assert!(matches!(
        azure
            .provision(&request(MachineId::generate(), X64_TYPE, false))
            .await,
        Err(ProviderError::OperationFailed { .. })
    ));
}

#[tokio::test]
async fn a_location_only_operation_is_followed_by_http_status() {
    let location = "https://management.azure.com/subscriptions/s/operationResults/op-2";
    let mut azure = provider(provision_script(vec![
        HttpResponse::new(202, Vec::new())
            .header("Location", location)
            .header("Retry-After", "3"),
        // `202` on the polling URL means still running…
        HttpResponse::new(202, Vec::new()).header("Location", location),
        // …and `200` means done, with the final resource in the body.
        done(),
    ]));

    azure
        .provision(&request(MachineId::generate(), X64_TYPE, false))
        .await
        .expect("provision");

    let transport = azure.transport();
    assert_eq!(transport.request(MACHINE_PUT + 1).url, location);
    assert_eq!(transport.request(MACHINE_PUT + 2).url, location);
    assert_eq!(
        azure.timer().delays(),
        vec![3, 2],
        "the stated Retry-After first, then the default backoff once it stops being stated"
    );
}

#[tokio::test]
async fn a_spot_refusal_reported_by_the_operation_falls_back_too() {
    // The refusal can arrive asynchronously rather than on the `PUT`, and
    // the fallback has to work either way.
    let mut azure = provider(provision_script(vec![
        accepted_with_operation(None),
        json(
            200,
            r#"{"name":"op-1","status":"Failed","error":{"code":"AzureSpotFeatureNotEnabledForSubscription","message":"nope"}}"#,
        ),
        done(),
    ]));

    let machine = azure
        .provision(&request(MachineId::generate(), X64_TYPE, true))
        .await
        .expect("the operation's refusal falls back to on-demand");
    assert_eq!(machine.capacity_mode, CapacityMode::OnDemand);
}

// ── Token refresh ──

#[tokio::test]
async fn a_401_re_mints_the_token_and_retries_once() {
    let mut azure = provider(vec![
        token(),
        HttpResponse::new(401, Vec::new()),
        token(),
        json(200, POLICY),
        json(200, SKUS),
        json(200, USAGES),
        done(),
        done(),
        done(),
        done(),
        done(),
    ]);

    azure
        .provision(&request(MachineId::generate(), X64_TYPE, false))
        .await
        .expect("provision");

    let transport = azure.transport();
    assert!(transport.request(0).url.contains("oauth2/v2.0/token"));
    assert!(transport.request(2).url.contains("oauth2/v2.0/token"));
    assert_eq!(
        transport.request(1).url,
        transport.request(3).url,
        "the same request is retried, not a different one"
    );
}

// ── Lifecycle ──

#[tokio::test]
async fn deallocating_posts_the_deallocate_action() {
    let machine = provisioned(MachineId::generate());
    let mut azure = provider(vec![token(), done()]);

    azure.deallocate(&machine).await.expect("deallocate");

    let request = azure.transport().request(1);
    assert_eq!(request.method, Method::Post);
    assert!(request.url.ends_with(&format!(
        "/providers/Microsoft.Compute/virtualMachines/{}/deallocate?api-version=2024-11-01",
        names::machine(machine.id)
    )));
    assert_eq!(request.body, [] as [u8; 0]);
}

#[tokio::test]
async fn starting_posts_the_start_action() {
    let machine = provisioned(MachineId::generate());
    let mut azure = provider(vec![token(), done()]);

    let started = azure.start(&machine).await.expect("start");
    assert_eq!(started.state, MachineState::Running);
    assert!(azure.transport().request(1).url.ends_with(&format!(
        "/virtualMachines/{}/start?api-version=2024-11-01",
        names::machine(machine.id)
    )));
}

#[tokio::test]
async fn resizing_deallocates_patches_the_size_and_starts_again() {
    let machine = provisioned(MachineId::generate());
    let mut azure = provider(vec![
        token(),
        json(200, POLICY),
        json(200, SKUS),
        json(200, USAGES),
        done(), // deallocate
        done(), // patch
        done(), // start
    ]);

    azure
        .resize(&machine, ZONE_RESTRICTED_TYPE)
        .await
        .expect("resize preserves the disk");

    let transport = azure.transport();
    assert!(transport.request(4).url.ends_with(&format!(
        "/virtualMachines/{}/deallocate?api-version=2024-11-01",
        names::machine(machine.id)
    )));

    let patch = transport.request(5);
    assert_eq!(patch.method, Method::Patch);
    assert_eq!(
        body_of(&patch),
        serde_json::json!({
            "properties": { "hardwareProfile": { "vmSize": ZONE_RESTRICTED_TYPE } }
        })
    );

    assert!(transport.request(6).url.ends_with(&format!(
        "/virtualMachines/{}/start?api-version=2024-11-01",
        names::machine(machine.id)
    )));
}

#[tokio::test]
async fn a_resize_to_a_type_with_no_quota_never_stops_the_machine() {
    let machine = provisioned(MachineId::generate());
    let mut azure = provider(vec![
        token(),
        json(200, POLICY),
        json(200, SKUS),
        json(200, USAGES),
    ]);

    azure
        .resize(&machine, NO_FAMILY_QUOTA_TYPE)
        .await
        .expect_err("a resize into a zero-quota family is refused");
    assert_eq!(
        azure.transport().request_count(),
        4,
        "nothing was deallocated, so the session is still running"
    );
}

#[tokio::test]
async fn a_failed_resize_operation_is_a_failure_whatever_the_machine_reports() {
    // Azure's documented trap: after a failed resize the machine's own body
    // reports the requested size while still running on the old one, so the
    // operation's terminal status is the only trustworthy answer.
    let machine = provisioned(MachineId::generate());
    let mut azure = provider(vec![
        token(),
        json(200, POLICY),
        json(200, SKUS),
        json(200, USAGES),
        done(),
        accepted_with_operation(None),
        json(
            200,
            r#"{"name":"op-1","status":"Failed","error":{"code":"OperationNotAllowed","message":"size unavailable on this cluster"}}"#,
        ),
    ]);

    let error = azure
        .resize(&machine, ZONE_RESTRICTED_TYPE)
        .await
        .expect_err("a failed resize operation is a failed resize");
    assert_eq!(error.code(), Some("OperationNotAllowed"));
}

#[tokio::test]
async fn destroying_removes_the_machine_then_everything_detach_kept() {
    let machine = provisioned(MachineId::generate());
    let mut azure = provider(vec![token(), done(), done(), done(), done()]);

    azure.destroy(&machine).await.expect("destroy");

    let transport = azure.transport();
    let deleted: Vec<String> = (1..5)
        .map(|index| {
            let request = transport.request(index);
            assert_eq!(request.method, Method::Delete);
            request.url
        })
        .collect();

    // In dependency order: the interface still references the address, and
    // the machine still references both, so a different order fails.
    assert!(deleted[0].contains(&format!("/virtualMachines/{}?", names::machine(machine.id))));
    assert!(deleted[1].contains(&format!(
        "/networkInterfaces/{}?api-version=2024-05-01",
        names::network_interface(machine.id)
    )));
    assert!(deleted[2].contains(&format!(
        "/publicIPAddresses/{}?api-version=2024-05-01",
        names::public_ip(machine.id)
    )));
    assert!(deleted[3].contains(&format!(
        "/disks/{}?api-version=2025-01-02",
        names::os_disk(machine.id)
    )));
}

// ── The catalog ──

fn one_region() -> Workspace {
    Workspace::new(RESOURCE_GROUP, SSH_KEY).with_regions(vec![REGION.to_owned()])
}

fn catalog_script() -> Vec<HttpResponse> {
    vec![
        token(),
        json(200, POLICY),
        json(200, SKUS),
        json(200, USAGES),
        json(200, PRICES),
        json(200, STORAGE_PRICES),
    ]
}

#[tokio::test]
async fn the_catalog_offers_only_what_passes_all_three_gates() {
    let mut azure = provider_over(one_region(), catalog_script());

    let catalog = azure.catalog().await.expect("catalog");
    let offered: Vec<&str> = catalog
        .iter()
        .map(|entry| entry.machine_type.as_str())
        .collect();

    assert!(offered.contains(&X64_TYPE));
    // Zone-restricted but location-clear, so still deployable — regionally.
    assert!(offered.contains(&ZONE_RESTRICTED_TYPE));
    // Location-restricted here.
    assert!(!offered.contains(&"Standard_B2ls_v2"));
    // A disk SKU is not a machine.
    assert!(!offered.contains(&"Premium_LRS"));

    let x64 = catalog
        .iter()
        .find(|entry| entry.machine_type == X64_TYPE)
        .expect("the x64 type is offered");
    assert_eq!(x64.provider, CloudProviderKind::Azure);
    assert_eq!(
        x64.capacity,
        Some(flyco_core::MachineCapacity {
            vcpus: 2,
            memory_mib: 4_096
        })
    );
    assert_eq!(
        x64.pricing,
        flyco_core::MachinePricing::Metered {
            on_demand_hourly: flyco_core::Usd::from_micros(76_400),
            spot_hourly: Some(flyco_core::Usd::from_micros(14_126)),
            minimum_billing_hours: None,
            storage: flyco_core::StoragePricing::CapacityTiers {
                tiers: vec![
                    flyco_core::StoragePriceTier {
                        capacity_gib: 4,
                        hourly: flyco_core::Usd::from_micros(411),
                    },
                    flyco_core::StoragePriceTier {
                        capacity_gib: 64,
                        hourly: flyco_core::Usd::from_micros(6_576),
                    },
                    flyco_core::StoragePriceTier {
                        capacity_gib: 32_767,
                        hourly: flyco_core::Usd::from_micros(3_424_658),
                    },
                ],
            },
        }
    );
}

#[tokio::test]
async fn a_machine_type_only_the_spot_pool_can_fund_is_still_offered() {
    let mut azure = provider_over(one_region(), catalog_script());

    // Its family quota is zero, but `lowPriorityCores` can fund it, so it is
    // still a machine a session can run on — just not on-demand.
    let report = azure.region_report(REGION).await.expect("report");
    assert!(
        report
            .offered
            .iter()
            .any(|entry| entry.machine_type == NO_FAMILY_QUOTA_TYPE),
        "a machine the spot pool can fund belongs in the catalog"
    );
}

#[tokio::test]
async fn every_exclusion_says_which_of_the_problems_it_is() {
    let mut azure = provider_over(one_region(), catalog_script());

    let report = azure.region_report(REGION).await.expect("report");
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
        reason("Standard_B2ls_v2"),
        ExclusionReason::NotOffered(_)
    ));
    assert!(
        !report
            .excluded
            .iter()
            .any(|(name, _)| name == "Premium_LRS"),
        "a disk SKU is not a machine that was excluded; it never entered the list"
    );
}

#[tokio::test]
async fn a_forbidden_region_reports_one_exclusion_naming_the_policy() {
    let mut azure = provider(vec![token(), json(200, POLICY)]);

    let report = azure.region_report(FORBIDDEN_REGION).await.expect("report");

    assert_eq!(report.offered.len(), 0);
    assert_eq!(
        report.excluded.len(),
        1,
        "the answer is the same for every machine type in the region"
    );
    let (subject, reason) = &report.excluded[0];
    assert_eq!(subject, FORBIDDEN_REGION);
    assert!(matches!(reason, ExclusionReason::RegionForbidden(_)));
    assert!(reason.to_string().contains("northcentralus"));
}

#[tokio::test]
async fn the_catalogs_regions_come_from_the_subscriptions_own_policy() {
    // No region named by the caller, so the policy's list is the catalog's.
    // The script covers one region and then runs out, which is the
    // assertion: the driver went where the policy said, not where a
    // hardcoded default said.
    let mut azure = provider(catalog_script());

    azure
        .catalog()
        .await
        .expect_err("the script covers one of the five allowed regions");

    let read = azure.transport().request(2).url;
    assert!(
        read.contains("location+eq+%27norwayeast%27"),
        "the first allowed region is the first one read: {read}"
    );
    assert!(
        !read.contains("westus2"),
        "no region outside the policy's list is ever read"
    );
}

#[tokio::test]
async fn a_named_region_the_policy_forbids_is_dropped_rather_than_attempted() {
    let workspace = Workspace::new(RESOURCE_GROUP, SSH_KEY)
        .with_regions(vec![FORBIDDEN_REGION.to_owned(), REGION.to_owned()]);
    let mut azure = provider_over(workspace, catalog_script());

    azure.catalog().await.expect("catalog");
    assert!(
        azure.transport().request(2).url.contains("northcentralus"),
        "the forbidden region is skipped, not read"
    );
}

/// The one test that touches a real subscription.
///
/// Ignored *and* feature-gated, so neither `cargo test` nor
/// `cargo test -- --ignored` can start it by accident: it creates billable
/// resources. See this module's documentation for the variables it needs.
#[cfg(feature = "azure-live")]
#[tokio::test]
#[ignore = "creates real, billable Azure resources"]
async fn live_provision_and_destroy() {
    use crate::clock::{SystemClock, SystemTimer};
    use crate::http::ZenwaveTransport;

    fn required(name: &str) -> String {
        std::env::var(name).unwrap_or_else(|_| panic!("the live test needs `{name}`"))
    }

    let principal = ServicePrincipal {
        tenant_id: required("FLYCO_AZURE_TENANT_ID"),
        client_id: required("FLYCO_AZURE_CLIENT_ID"),
        client_secret: required("FLYCO_AZURE_CLIENT_SECRET"),
        subscription_id: required("FLYCO_AZURE_SUBSCRIPTION_ID"),
    };
    // The resource group's own region, which the subscription's policy must
    // already allow — the group exists, so it does.
    let region = required("FLYCO_AZURE_REGION");
    let workspace = Workspace::new(
        required("FLYCO_AZURE_RESOURCE_GROUP"),
        required("FLYCO_AZURE_SSH_PUBLIC_KEY"),
    )
    .with_regions(vec![region.clone()]);

    let mut azure = AzureProvider::with_parts(
        ZenwaveTransport::new(),
        SystemClock::new(),
        SystemTimer::new(),
        principal,
        workspace,
    );

    let catalog = azure.catalog().await.expect("read the live catalog");
    assert!(
        !catalog.is_empty(),
        "the subscription can deploy nothing in the region its policy allows"
    );

    let cheapest = catalog
        .iter()
        .filter_map(|entry| entry.pricing.hourly(true).map(|price| (price, entry)))
        .min_by_key(|(price, _)| *price)
        .expect("a priced machine type")
        .1
        .machine_type
        .clone();

    let provision = request_in(MachineId::generate(), &region, &cheapest, true);
    let machine = azure.provision(&provision).await.expect("provision");
    azure.destroy(&machine).await.expect("destroy");
}

// ── Metered spend ──

#[tokio::test]
async fn the_cost_query_names_the_subscription_scope_and_asks_for_actual_cost() {
    let mut azure = provider(vec![token(), json(200, COST)]);

    let spend = azure
        .billing_period_cost(QUERIED_AT)
        .await
        .expect("read the metered spend");

    // The token call is first; the query is what this test is about.
    let request = azure.transport().request(1);
    assert_eq!(request.method, Method::Post);
    assert_eq!(
        request.url,
        format!(
            "https://management.azure.com/subscriptions/{SUBSCRIPTION}\
             /providers/Microsoft.CostManagement/query?api-version=2025-03-01"
        )
    );

    let body: Value =
        serde_json::from_str(request.body_text().expect("a UTF-8 body")).expect("a JSON body");
    assert_eq!(body["type"], "ActualCost");
    assert_eq!(body["timeframe"], "MonthToDate");
    assert_eq!(body["dataset"]["granularity"], "None");
    assert_eq!(body["dataset"]["aggregation"]["totalCost"]["name"], "Cost");
    assert_eq!(
        body["dataset"]["aggregation"]["totalCost"]["function"],
        "Sum"
    );
    // A grouping would split the total across rows for no gain, and the
    // 2025-03-01 API refuses several of the obvious dimensions outright.
    assert!(body["dataset"].get("grouping").is_none());

    assert_eq!(spend.spent, flyco_core::Usd::from_micros(12_345_678));
    assert_eq!(spend.period_start_unix, MONTH_START);
    assert_eq!(
        spend.period_end_unix, QUERIED_AT,
        "the window flyco reports ends where the query did"
    );
    assert_eq!(
        spend.remaining_credit, None,
        "a credit balance is EA-only, and inventing a zero would say it is spent"
    );
}

#[tokio::test]
async fn a_month_with_nothing_metered_is_a_real_zero() {
    let mut azure = provider(vec![token(), json(200, COST_EMPTY)]);

    let spend = azure
        .billing_period_cost(QUERIED_AT)
        .await
        .expect("read the metered spend");

    // Unlike an unmetered provider, which contributes no row at all, Azure
    // answering "nothing" is Azure's own number.
    assert_eq!(spend.spent, flyco_core::Usd::ZERO);
}

#[tokio::test]
async fn a_subscription_billed_in_another_currency_is_refused() {
    let mut azure = provider(vec![token(), json(200, COST_IN_EUROS)]);

    let error = azure
        .billing_period_cost(QUERIED_AT)
        .await
        .expect_err("euros must not be reported as dollars");
    assert!(error.to_string().contains("EUR"));
}

#[tokio::test]
async fn a_cost_query_azure_answered_with_no_content_meters_nothing() {
    let mut azure = provider(vec![token(), HttpResponse::new(204, Vec::new())]);

    let spend = azure
        .billing_period_cost(QUERIED_AT)
        .await
        .expect("a 204 is an empty period, not an unreadable answer");
    assert_eq!(spend.spent, flyco_core::Usd::ZERO);
    assert_eq!(spend.period_start_unix, MONTH_START);
}
