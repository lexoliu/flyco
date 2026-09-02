//! What the AWS driver actually puts on the wire.
//!
//! No credentials exist here and no test may create a cloud resource, so
//! every exchange is recorded: the driver is handed a scripted list of
//! responses and the assertions are on the requests that come back out —
//! the exact URL, the exact `Action`, the exact form parameters, and the
//! exact `SigV4` signature. The signature is only assertable because the
//! signing instant is a [`ManualWallClock`](crate::clock::ManualWallClock)
//! rather than the host's clock, which is the whole reason the driver takes
//! one.
//!
//! # Running it against a real account
//!
//! There is a live test, `live_provision_and_destroy`, behind the
//! `aws-live` feature, which is off by default and which CI never enables.
//! It creates and destroys real, billable resources — an instance, an EBS
//! volume and an elastic IP. To run it:
//!
//! ```text
//! export FLYCO_AWS_ACCESS_KEY_ID=…      FLYCO_AWS_SECRET_ACCESS_KEY=…
//! export FLYCO_AWS_REGION=us-west-2
//! # Optional: an EC2 key pair in that region, for a break-glass login.
//! export FLYCO_AWS_KEY_PAIR=my-keypair
//! cargo test -p flyco-provider --features aws-live -- --ignored --nocapture
//! ```
//!
//! The key needs `ec2:*` on the resources it creates, `ssm:GetParameter`,
//! `servicequotas:ListServiceQuotas`, `pricing:GetProducts` and
//! `ce:GetCostAndUsage`; the region must be one the account has enabled and
//! must still have its default VPC.

use flyco_core::machine::{CloudProviderKind, MachineSpec, MachineState};
use flyco_core::{HarnessKind, MachineId, PermissionMode, SessionId};

use super::{AwsProvider, AwsWorkspace, ExclusionReason, SPOT_UNSUPPORTED_CODES, names};
use crate::clock::{ManualClock, ManualWallClock};
use crate::cloud_init::CONFIG_PATH;
use crate::http::{HttpRequest, HttpResponse, Method};
use crate::testing::{RecordedTransport, RecordingTimer};
use crate::{
    CapacityMode, ClaudeCredential, CloudProvider, DaemonBootstrap, Machine, ProviderError,
    ProvisionRequest,
};

const REGION: &str = "us-west-2";
const DISABLED_REGION: &str = "ap-east-1";
const KEY_PAIR: &str = "lexo-flyco";

const X64_TYPE: &str = "t3.small";
const ARM64_TYPE: &str = "t4g.small";
const UNOFFERED_TYPE: &str = "t2.small";
const MAC_TYPE: &str = "mac2.metal";
const UNPRICED_TYPE: &str = "t3.unpriced";

const INSTANCE: &str = "i-0d3f9a1c2b4e5f607";
const VOLUME: &str = "vol-04ac6f9e1d2b3c4d5";
const ALLOCATION: &str = "eipalloc-0abc123def456789a";
const PUBLIC_IP: &str = "52.13.24.9";
const SUBNET: &str = "subnet-0123456789abcdef0";
const SECURITY_GROUP: &str = "sg-0fedcba9876543210";
const IMAGE: &str = "ami-0c2b8ca1dad447f8a";

/// 2026-08-29T12:00:00Z — the instant every signature in these tests is
/// made at, which is what makes them reproducible.
const SIGNED_AT: u64 = 1_788_004_800;

/// 2026-08-01T00:00:00Z: midnight UTC on the first of that month.
const MONTH_START: u64 = 1_785_542_400;

const REGIONS: &str = include_str!("../../fixtures/aws/describe_regions.xml");
const TYPES: &str = include_str!("../../fixtures/aws/describe_instance_types.xml");
const OFFERINGS: &str = include_str!("../../fixtures/aws/describe_instance_type_offerings.xml");
const QUOTAS: &str = include_str!("../../fixtures/aws/list_service_quotas.json");
/// The same account with no on-demand headroom at all, which is what makes
/// spot the only market a machine can be started on.
const NO_ON_DEMAND_QUOTA: &str =
    include_str!("../../fixtures/aws/list_service_quotas_no_on_demand.json");
const NO_INSTANCES: &str = include_str!("../../fixtures/aws/describe_instances_none.xml");
const VPCS: &str = include_str!("../../fixtures/aws/describe_vpcs.xml");
const SUBNETS: &str = include_str!("../../fixtures/aws/describe_subnets.xml");
const GROUPS: &str = include_str!("../../fixtures/aws/describe_security_groups.xml");
const NO_GROUPS: &str = include_str!("../../fixtures/aws/describe_security_groups_none.xml");
const CREATED_GROUP: &str = include_str!("../../fixtures/aws/create_security_group.xml");
const AUTHORIZED: &str = include_str!("../../fixtures/aws/authorize_ingress.xml");
const PARAMETER: &str = include_str!("../../fixtures/aws/get_parameter.json");
const IMAGES: &str = include_str!("../../fixtures/aws/describe_images.xml");
const ALLOCATED: &str = include_str!("../../fixtures/aws/allocate_address.xml");
const RUN: &str = include_str!("../../fixtures/aws/run_instances.xml");
const RUN_ON_DEMAND: &str = include_str!("../../fixtures/aws/run_instances_on_demand.xml");
const RUNNING: &str = include_str!("../../fixtures/aws/describe_instances_running.xml");
const ON_DEMAND_RUNNING: &str =
    include_str!("../../fixtures/aws/describe_instances_on_demand_running.xml");
const PENDING: &str = include_str!("../../fixtures/aws/describe_instances_pending.xml");
const STOPPED: &str = include_str!("../../fixtures/aws/describe_instances_stopped.xml");
const TERMINATED: &str = include_str!("../../fixtures/aws/describe_instances_terminated.xml");
const ASSOCIATED: &str = include_str!("../../fixtures/aws/associate_address.xml");
const STOP: &str = include_str!("../../fixtures/aws/stop_instances.xml");
const START: &str = include_str!("../../fixtures/aws/start_instances.xml");
const TERMINATE: &str = include_str!("../../fixtures/aws/terminate_instances.xml");
const MODIFIED: &str = include_str!("../../fixtures/aws/modify_instance_attribute.xml");
const ADDRESSES: &str = include_str!("../../fixtures/aws/describe_addresses.xml");
const VOLUME_AVAILABLE: &str = include_str!("../../fixtures/aws/describe_volumes_available.xml");
const VOLUME_IN_USE: &str = include_str!("../../fixtures/aws/describe_volumes_in_use.xml");
const RELEASED: &str = include_str!("../../fixtures/aws/release_address.xml");
const DELETED_VOLUME: &str = include_str!("../../fixtures/aws/delete_volume.xml");
const PRODUCTS: &str = include_str!("../../fixtures/aws/get_products.json");
const STORAGE_PRODUCTS: &str = include_str!("../../fixtures/aws/get_storage_products.json");
const SPOT_PRICES: &str = include_str!("../../fixtures/aws/describe_spot_price_history.xml");
const COST: &str = include_str!("../../fixtures/aws/get_cost_and_usage.json");
const IDENTITY: &str = include_str!("../../fixtures/aws/get_caller_identity.xml");

/// A driver over a scripted transport, clocks that do not move, and a timer
/// that records rather than waits.
type Recorded = AwsProvider<RecordedTransport, ManualClock, RecordingTimer, ManualWallClock>;

fn key() -> super::sigv4::AccessKey {
    super::sigv4::AccessKey::new(
        "AKIAIOSFODNN7EXAMPLE",
        "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
    )
}

fn provider(responses: Vec<HttpResponse>) -> Recorded {
    provider_over(AwsWorkspace::new().with_key_pair(KEY_PAIR), responses)
}

fn provider_over(workspace: AwsWorkspace, responses: Vec<HttpResponse>) -> Recorded {
    AwsProvider::with_parts(
        RecordedTransport::new(responses),
        ManualClock::new(),
        RecordingTimer::new(),
        ManualWallClock::at(SIGNED_AT),
        key(),
        workspace,
    )
}

fn xml(body: &str) -> HttpResponse {
    HttpResponse::new(200, body.as_bytes().to_vec())
}

fn json(body: &str) -> HttpResponse {
    HttpResponse::new(200, body.as_bytes().to_vec())
}

fn refusal(fixture: &str) -> HttpResponse {
    HttpResponse::new(400, fixture.as_bytes().to_vec())
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
            provider: CloudProviderKind::Aws,
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
            repo: crate::testing::checkout(),
            resume_session_id: None,
        },
    }
}

fn request(machine: MachineId, machine_type: &str, spot: bool) -> ProvisionRequest {
    request_in(machine, REGION, machine_type, spot)
}

/// The form parameters of a request, as the driver encoded them.
fn form(request: &HttpRequest) -> Vec<(String, String)> {
    url::form_urlencoded::parse(&request.body)
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

/// One form parameter, which must be present.
fn field(request: &HttpRequest, name: &str) -> String {
    form(request)
        .into_iter()
        .find(|(key, _)| key == name)
        .map_or_else(
            || panic!("the request carries `{name}`: {:?}", form(request)),
            |(_, value)| value,
        )
}

/// Whether a request carries a parameter at all.
fn has(request: &HttpRequest, name: &str) -> bool {
    form(request).iter().any(|(key, _)| key == name)
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

/// The action a request names, or the JSON-RPC target it carries.
fn action(request: &HttpRequest) -> String {
    form(request)
        .into_iter()
        .find(|(key, _)| key == "Action")
        .map(|(_, value)| value)
        .or_else(|| {
            request
                .headers
                .iter()
                .find(|(name, _)| name == "x-amz-target")
                .map(|(_, value)| value.clone())
        })
        .unwrap_or_else(|| panic!("the request names neither an Action nor a target"))
}

/// Every action the driver performed, in order.
fn actions(provider: &Recorded) -> Vec<String> {
    let transport = provider.transport();
    (0..transport.request_count())
        .map(|index| action(&transport.request(index)))
        .collect()
}

/// The responses a clean provisioning run consumes, in order.
///
/// Index 0 is the region-access read, 1 the instance types, 2 the offerings,
/// 3 the quota limits, 4 the running instances that spend them; the
/// workspace reads follow, then the image, then the address, and the launch
/// is at [`RUN_INSTANCES`].
fn provision_script(launch: Vec<HttpResponse>) -> Vec<HttpResponse> {
    let mut script = vec![
        xml(REGIONS),
        xml(TYPES),
        xml(OFFERINGS),
        json(QUOTAS),
        xml(NO_INSTANCES),
        xml(VPCS),
        xml(SUBNETS),
        xml(GROUPS),
        json(PARAMETER),
        xml(IMAGES),
        xml(ALLOCATED),
    ];
    script.extend(launch);
    script
}

/// Index of `RunInstances` in a [`provision_script`] run.
const RUN_INSTANCES: usize = 11;

/// The tail of a successful provision: the instance settles, then the
/// address is attached.
fn settles() -> Vec<HttpResponse> {
    vec![xml(RUNNING), xml(ASSOCIATED)]
}

fn provisioned(machine: MachineId) -> Machine {
    Machine {
        id: machine,
        native_id: INSTANCE.to_owned(),
        region: REGION.to_owned(),
        state: MachineState::Running,
        capacity_mode: CapacityMode::Spot,
        address: Some(PUBLIC_IP.to_owned()),
    }
}

// ── The three gates ──

#[tokio::test]
async fn provisioning_reads_region_access_availability_and_quota_before_it_writes() {
    let mut aws = provider(provision_script([vec![xml(RUN)], settles()].concat()));
    aws.provision(&request(MachineId::generate(), X64_TYPE, true))
        .await
        .expect("provision");

    let transport = aws.transport();

    // The opt-in gate is invisible from the instance-type answers, so it is
    // read — and against a region every account has enabled, because the
    // question is which regions are enabled.
    let access = transport.request(0);
    assert_eq!(access.url, "https://ec2.us-east-1.amazonaws.com/");
    assert_eq!(field(&access, "Action"), "DescribeRegions");
    assert_eq!(
        field(&access, "AllRegions"),
        "true",
        "a region the account never enabled is only visible with AllRegions"
    );

    assert_eq!(
        transport.request(1).url,
        "https://ec2.us-west-2.amazonaws.com/"
    );
    assert_eq!(field(&transport.request(1), "Version"), "2016-11-15");
    assert_eq!(
        action(&transport.request(2)),
        "DescribeInstanceTypeOfferings"
    );
    assert_eq!(
        field(&transport.request(2), "Filter.1.Name"),
        "location",
        "the offerings are asked for one region"
    );
    assert_eq!(field(&transport.request(2), "Filter.1.Value.1"), REGION);
    assert_eq!(field(&transport.request(2), "LocationType"), "region");

    // Service Quotas states the ceiling; the running instances state what is
    // under it, because AWS publishes the two separately.
    let limits = transport.request(3);
    assert_eq!(limits.url, "https://servicequotas.us-west-2.amazonaws.com/");
    assert_eq!(
        header(&limits, "x-amz-target"),
        "ServiceQuotasV20190624.ListServiceQuotas"
    );
    assert_eq!(action(&transport.request(4)), "DescribeInstances");
    assert_eq!(
        field(&transport.request(4), "Filter.1.Name"),
        "instance-state-name"
    );
}

#[tokio::test]
async fn a_region_the_account_has_not_enabled_is_refused_before_any_other_read() {
    let mut aws = provider(vec![xml(REGIONS)]);

    let error = aws
        .provision(&request_in(
            MachineId::generate(),
            DISABLED_REGION,
            X64_TYPE,
            true,
        ))
        .await
        .expect_err("a region the account never enabled cannot be deployed into");

    let ProviderError::Unavailable { reason, .. } = &error else {
        panic!("a disabled region is an availability failure: {error}");
    };
    assert!(
        reason.contains("has not enabled"),
        "the refusal names the problem the user can fix: {reason}"
    );
    assert_eq!(
        aws.transport().request_count(),
        1,
        "no instance-type or quota read happens for a region that cannot be used at all"
    );
}

#[tokio::test]
async fn the_region_access_read_happens_once_per_driver() {
    let mut aws = provider(
        [
            provision_script([vec![xml(RUN)], settles()].concat()),
            vec![xml(STOP), xml(STOPPED)],
        ]
        .concat(),
    );
    let machine = MachineId::generate();
    aws.provision(&request(machine, X64_TYPE, true))
        .await
        .expect("provision");
    aws.deallocate(&provisioned(machine))
        .await
        .expect("deallocate");

    assert_eq!(
        actions(&aws)
            .iter()
            .filter(|name| *name == "DescribeRegions")
            .count(),
        1,
        "a region's opt-in status changes on a human timescale; reading it per call is waste"
    );
}

#[tokio::test]
async fn an_instance_type_the_region_does_not_offer_is_refused_before_any_write() {
    let mut aws = provider(vec![xml(REGIONS), xml(TYPES), xml(OFFERINGS)]);

    let error = aws
        .provision(&request(MachineId::generate(), UNOFFERED_TYPE, true))
        .await
        .expect_err("an unoffered instance type cannot be launched");
    assert!(matches!(error, ProviderError::Unavailable { .. }));
    assert_eq!(
        aws.transport().request_count(),
        3,
        "the quota read never happens: availability already settled it"
    );
}

#[tokio::test]
async fn an_instance_type_ec2_does_not_describe_is_refused() {
    let mut aws = provider(vec![xml(REGIONS), xml(TYPES)]);

    let error = aws
        .provision(&request(MachineId::generate(), "t9.enormous", true))
        .await
        .expect_err("an unknown instance type cannot be launched");
    assert!(matches!(error, ProviderError::Unavailable { .. }));
}

#[tokio::test]
async fn a_machine_type_that_runs_macos_is_refused_before_anything_is_created() {
    // Flyco's session image is Ubuntu and there is no Ubuntu for a Mac, so
    // asking for one is refused where the reason can be stated rather than
    // failing later on an image lookup that cannot succeed. The catalog
    // still carries it — see `an_ec2_mac_says_it_bills_a_day_at_a_time`.
    for spot in [true, false] {
        let mut aws = provider(vec![xml(REGIONS), xml(TYPES), xml(OFFERINGS)]);
        let error = aws
            .provision(&request(MachineId::generate(), MAC_TYPE, spot))
            .await
            .expect_err("flyco boots no macOS machine");

        let ProviderError::Unsupported { reason, .. } = &error else {
            panic!("a macOS machine type is unsupported, not unavailable: {error}");
        };
        assert!(reason.contains("macOS"), "the refusal names why: {reason}");
        assert!(
            !actions(&aws).iter().any(|name| name == "AllocateAddress"),
            "nothing is created for a machine that cannot be booted"
        );
    }
}

// ── The provisioning sequence ──

#[tokio::test]
async fn the_workspace_network_is_the_accounts_default_vpc_and_one_flyco_group() {
    let mut aws = provider(provision_script([vec![xml(RUN)], settles()].concat()));
    aws.provision(&request(MachineId::generate(), ARM64_TYPE, true))
        .await
        .expect("provision");

    let transport = aws.transport();
    let vpcs = transport.request(5);
    assert_eq!(action(&vpcs), "DescribeVpcs");
    assert_eq!(field(&vpcs, "Filter.1.Name"), "isDefault");
    assert_eq!(field(&vpcs, "Filter.1.Value.1"), "true");

    let subnets = transport.request(6);
    assert_eq!(action(&subnets), "DescribeSubnets");
    assert_eq!(field(&subnets, "Filter.1.Name"), "vpc-id");

    let groups = transport.request(7);
    assert_eq!(action(&groups), "DescribeSecurityGroups");
    assert_eq!(
        field(&groups, "Filter.1.Value.1"),
        names::security_group(REGION)
    );

    // The subnet is the one in the alphabetically first zone, so two
    // provisions in a region land in the same zone rather than wherever the
    // API happened to list first.
    assert_eq!(
        field(&transport.request(RUN_INSTANCES), "SubnetId"),
        SUBNET,
        "the subnet must not move with the API's own ordering"
    );
}

#[tokio::test]
async fn a_missing_security_group_is_created_and_opened_for_ssh() {
    let mut script = provision_script([vec![xml(RUN)], settles()].concat());
    // Index 7 is the security-group read: nothing there, so it is created.
    script[7] = xml(NO_GROUPS);
    script.insert(8, xml(CREATED_GROUP));
    script.insert(9, xml(AUTHORIZED));

    let mut aws = provider(script);
    aws.provision(&request(MachineId::generate(), ARM64_TYPE, true))
        .await
        .expect("provision");

    let transport = aws.transport();
    let created = transport.request(8);
    assert_eq!(action(&created), "CreateSecurityGroup");
    assert_eq!(field(&created, "GroupName"), names::security_group(REGION));
    assert_eq!(field(&created, "VpcId"), "vpc-0a1b2c3d4e5f60718");

    // Not optional: a machine launched into a group that admits nothing
    // comes up perfectly and answers nothing.
    let opened = transport.request(9);
    assert_eq!(action(&opened), "AuthorizeSecurityGroupIngress");
    assert_eq!(field(&opened, "GroupId"), SECURITY_GROUP);
    assert_eq!(field(&opened, "IpPermissions.1.IpProtocol"), "tcp");
    assert_eq!(field(&opened, "IpPermissions.1.FromPort"), "22");
    assert_eq!(field(&opened, "IpPermissions.1.ToPort"), "22");
    assert_eq!(
        field(&opened, "IpPermissions.1.IpRanges.1.CidrIp"),
        "0.0.0.0/0"
    );
}

#[tokio::test]
async fn the_image_is_read_from_canonicals_parameter_for_the_types_own_architecture() {
    for (machine_type, architecture) in [(ARM64_TYPE, "arm64"), (X64_TYPE, "amd64")] {
        let mut aws = provider(provision_script([vec![xml(RUN)], settles()].concat()));
        aws.provision(&request(MachineId::generate(), machine_type, true))
            .await
            .expect("provision");

        let parameter = aws.transport().request(8);
        assert_eq!(parameter.url, "https://ssm.us-west-2.amazonaws.com/");
        assert_eq!(header(&parameter, "x-amz-target"), "AmazonSSM.GetParameter");
        let body: serde_json::Value = serde_json::from_slice(&parameter.body).expect("a JSON body");
        assert_eq!(
            body["Name"],
            format!(
                "/aws/service/canonical/ubuntu/server/24.04/stable/current/\
                 {architecture}/hvm/ebs-gp3/ami-id"
            ),
            "`{machine_type}` must boot the `{architecture}` image"
        );

        // The root device is read from the AMI rather than assumed: a
        // mapping naming the wrong device is silently ignored and the volume
        // comes out the image's default size.
        assert_eq!(action(&aws.transport().request(9)), "DescribeImages");
        let launch = aws.transport().request(RUN_INSTANCES);
        assert_eq!(field(&launch, "ImageId"), IMAGE);
        assert_eq!(
            field(&launch, "BlockDeviceMapping.1.DeviceName"),
            "/dev/sda1"
        );
    }
}

#[tokio::test]
async fn the_launch_body_is_the_measured_shape() {
    let machine = MachineId::generate();
    let provision = request(machine, ARM64_TYPE, true);
    let session = provision.bootstrap.session;
    let mut aws = provider(provision_script([vec![xml(RUN)], settles()].concat()));
    aws.provision(&provision).await.expect("provision");

    let launch = aws.transport().request(RUN_INSTANCES);
    assert_eq!(launch.method, Method::Post);
    assert_eq!(launch.url, "https://ec2.us-west-2.amazonaws.com/");
    assert_eq!(field(&launch, "Action"), "RunInstances");

    assert_eq!(field(&launch, "InstanceType"), ARM64_TYPE);
    assert_eq!(field(&launch, "MinCount"), "1");
    assert_eq!(field(&launch, "MaxCount"), "1");
    assert_eq!(field(&launch, "SecurityGroupId.1"), SECURITY_GROUP);
    assert_eq!(field(&launch, "KeyName"), KEY_PAIR);

    // Spot, persistent, stopping on interruption — and all three matter.
    assert_eq!(field(&launch, "InstanceMarketOptions.MarketType"), "spot");
    assert_eq!(
        field(
            &launch,
            "InstanceMarketOptions.SpotOptions.SpotInstanceType"
        ),
        "persistent",
        "a one-time request expires with the machine it created"
    );
    assert_eq!(
        field(
            &launch,
            "InstanceMarketOptions.SpotOptions.InstanceInterruptionBehavior"
        ),
        "stop",
        "`terminate` would delete the session's work on the first interruption"
    );

    // The root volume outlives its instance, which is what makes a resize
    // and a destroy two different things.
    assert_eq!(field(&launch, "BlockDeviceMapping.1.Ebs.VolumeSize"), "30");
    assert_eq!(field(&launch, "BlockDeviceMapping.1.Ebs.VolumeType"), "gp3");
    assert_eq!(
        field(&launch, "BlockDeviceMapping.1.Ebs.DeleteOnTermination"),
        "false"
    );

    // Tags on the instance and on the volume, applied at creation: a
    // `CreateTags` that failed would leave a resource nothing can find.
    assert_eq!(
        field(&launch, "TagSpecification.1.ResourceType"),
        "instance"
    );
    assert_eq!(field(&launch, "TagSpecification.2.ResourceType"), "volume");
    assert_eq!(field(&launch, "TagSpecification.1.Tag.1.Key"), "owner");
    assert_eq!(field(&launch, "TagSpecification.1.Tag.1.Value"), "aws");
    assert_eq!(
        field(&launch, "TagSpecification.1.Tag.2.Value"),
        session.to_string()
    );
    assert_eq!(
        field(&launch, "TagSpecification.1.Tag.3.Value"),
        machine.to_string()
    );

    assert!(
        !has(&launch, "Placement.Tenancy"),
        "host tenancy is refused for an ordinary instance type"
    );
}

#[tokio::test]
async fn every_request_is_signed_for_the_region_and_service_it_is_sent_to() {
    let mut aws = provider(provision_script([vec![xml(RUN)], settles()].concat()));
    aws.provision(&request(MachineId::generate(), ARM64_TYPE, true))
        .await
        .expect("provision");

    let transport = aws.transport();
    let credential = |index: usize| {
        let authorization = header(&transport.request(index), "authorization");
        authorization
            .split("Credential=")
            .nth(1)
            .and_then(|rest| rest.split(',').next())
            .unwrap_or_else(|| panic!("request {index} is signed"))
            .to_owned()
    };

    // The scope names the date, the region and the service, and each of the
    // three is part of the derived signing key.
    assert_eq!(
        credential(1),
        "AKIAIOSFODNN7EXAMPLE/20260829/us-west-2/ec2/aws4_request"
    );
    assert_eq!(
        credential(3),
        "AKIAIOSFODNN7EXAMPLE/20260829/us-west-2/servicequotas/aws4_request",
        "Service Quotas is a different service, so it is a different signing key"
    );
    assert_eq!(
        credential(8),
        "AKIAIOSFODNN7EXAMPLE/20260829/us-west-2/ssm/aws4_request"
    );

    for index in 0..transport.request_count() {
        let authorization = header(&transport.request(index), "authorization");
        assert!(
            authorization.contains("SignedHeaders=content-type;host;x-amz-date"),
            "request {index} must sign the host it is addressed to: {authorization}"
        );
        assert_eq!(
            header(&transport.request(index), "x-amz-date"),
            "20260829T120000Z"
        );
    }
}

#[tokio::test]
async fn a_json_rpc_call_carries_the_versioned_media_type_the_protocol_selects() {
    let mut aws = provider(provision_script([vec![xml(RUN)], settles()].concat()));
    aws.provision(&request(MachineId::generate(), ARM64_TYPE, true))
        .await
        .expect("provision");

    for index in [3, 8] {
        assert_eq!(
            header(&aws.transport().request(index), "content-type"),
            "application/x-amz-json-1.1",
            "the JSON-RPC services refuse `application/json`"
        );
    }
}

#[tokio::test]
async fn cloud_init_carries_the_daemon_configuration_and_nothing_readable() {
    use base64::Engine as _;

    let provision = request(MachineId::generate(), ARM64_TYPE, true);
    let mut aws = provider(provision_script([vec![xml(RUN)], settles()].concat()));
    aws.provision(&provision).await.expect("provision");

    let launch = aws.transport().request(RUN_INSTANCES);
    let user_data = field(&launch, "UserData");

    // The token must not be legible in the request itself.
    let raw = launch.body_text().expect("UTF-8").to_owned();
    assert!(!raw.contains("fd_a-live-daemon-token"));

    let cloud_config = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(&user_data)
            .expect("UserData is base64"),
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

#[tokio::test]
async fn the_address_is_an_elastic_ip_allocated_before_the_launch_and_attached_after_it() {
    let machine = MachineId::generate();
    let mut aws = provider(provision_script([vec![xml(RUN)], settles()].concat()));
    let provisioned = aws
        .provision(&request(machine, ARM64_TYPE, true))
        .await
        .expect("provision");

    let transport = aws.transport();
    let allocate = transport.request(10);
    assert_eq!(action(&allocate), "AllocateAddress");
    assert_eq!(field(&allocate, "Domain"), "vpc");
    // Tagged at allocation, because an association is gone the moment the
    // instance is and an address nothing can find is one nobody stops paying
    // for.
    assert_eq!(
        field(&allocate, "TagSpecification.1.ResourceType"),
        "elastic-ip"
    );
    assert_eq!(
        field(&allocate, "TagSpecification.1.Tag.3.Value"),
        machine.to_string()
    );

    // Attached only once the instance has actually reached `running`.
    assert_eq!(
        action(&transport.request(RUN_INSTANCES + 1)),
        "DescribeInstances"
    );
    let associate = transport.request(RUN_INSTANCES + 2);
    assert_eq!(action(&associate), "AssociateAddress");
    assert_eq!(field(&associate, "AllocationId"), ALLOCATION);
    assert_eq!(field(&associate, "InstanceId"), INSTANCE);

    assert_eq!(
        provisioned.address.as_deref(),
        Some(PUBLIC_IP),
        "the elastic IP survives a stop, so it is the address the machine keeps"
    );
    assert_eq!(provisioned.native_id, INSTANCE);
    assert_eq!(provisioned.region, REGION);
}

#[tokio::test]
async fn a_launch_that_fails_takes_its_address_with_it() {
    let mut aws = provider(provision_script(vec![
        refusal(include_str!("../../fixtures/aws/error_unauthorized.xml")),
        xml(RELEASED),
    ]));

    let error = aws
        .provision(&request(MachineId::generate(), ARM64_TYPE, true))
        .await
        .expect_err("an unauthorized launch is a failed provision");
    assert_eq!(error.code(), Some("UnauthorizedOperation"));

    // An elastic IP nothing is attached to is still billed by the hour.
    let released = aws.transport().request(RUN_INSTANCES + 1);
    assert_eq!(action(&released), "ReleaseAddress");
    assert_eq!(field(&released, "AllocationId"), ALLOCATION);
}

// ── Spot, and the fallback ──

#[tokio::test]
async fn a_spot_refusal_is_retried_on_demand_for_every_code_that_means_it() {
    for fixture in [
        include_str!("../../fixtures/aws/error_insufficient_capacity.xml"),
        include_str!("../../fixtures/aws/error_unsupported.xml"),
    ] {
        let mut aws = provider(provision_script(vec![
            refusal(fixture),
            xml(RUN_ON_DEMAND),
            xml(ON_DEMAND_RUNNING),
            xml(ASSOCIATED),
        ]));

        let provisioned = aws
            .provision(&request(MachineId::generate(), ARM64_TYPE, true))
            .await
            .expect("a spot refusal falls back rather than failing");

        assert_eq!(
            provisioned.capacity_mode,
            CapacityMode::OnDemand,
            "the machine records the capacity it got, not the one it asked for"
        );

        let first = aws.transport().request(RUN_INSTANCES);
        let second = aws.transport().request(RUN_INSTANCES + 1);
        assert_eq!(field(&first, "InstanceMarketOptions.MarketType"), "spot");

        // The identical body, minus exactly the market options.
        for parameter in [
            "InstanceMarketOptions.MarketType",
            "InstanceMarketOptions.SpotOptions.SpotInstanceType",
            "InstanceMarketOptions.SpotOptions.InstanceInterruptionBehavior",
        ] {
            assert!(
                !has(&second, parameter),
                "`{parameter}` must come off for the on-demand retry"
            );
        }
        assert_eq!(
            field(&second, "UserData"),
            field(&first, "UserData"),
            "the retry is the same machine, not a different one"
        );
        assert_eq!(
            field(&second, "InstanceType"),
            field(&first, "InstanceType")
        );
    }
}

#[tokio::test]
async fn every_code_that_triggers_the_fallback_is_about_the_spot_market() {
    // A guard on the list itself: adding a code here that means something
    // else would turn one clear error into two.
    assert_eq!(
        SPOT_UNSUPPORTED_CODES,
        [
            "InsufficientInstanceCapacity",
            "MaxSpotInstanceCountExceeded",
            "SpotMaxPriceTooLow",
            "Unsupported",
        ]
    );
}

#[tokio::test]
async fn the_on_demand_fallback_re_checks_the_pool_it_would_spend() {
    // The account's on-demand limit for this family is zero while its spot
    // limit is four, so the machine passed the gate as spot and re-sending
    // it on-demand would fail later with an opaque error. Refuse it here,
    // where the reason is nameable — exactly Azure's spot-only machine.
    let mut script = provision_script(vec![
        refusal(include_str!(
            "../../fixtures/aws/error_insufficient_capacity.xml"
        )),
        xml(RELEASED),
    ]);
    script[3] = json(NO_ON_DEMAND_QUOTA);

    let mut aws = provider(script);
    let error = aws
        .provision(&request(MachineId::generate(), X64_TYPE, true))
        .await
        .expect_err("the on-demand pool cannot fund this machine");
    assert!(matches!(error, ProviderError::QuotaExceeded { .. }));
    assert_eq!(
        actions(&aws)
            .iter()
            .filter(|name| *name == "RunInstances")
            .count(),
        1,
        "no second launch is attempted"
    );
}

#[tokio::test]
async fn a_machine_only_the_spot_pool_can_fund_is_still_provisioned() {
    // The other side of the same fixture: an on-demand limit of zero refuses
    // the machine outright, and the spot pool runs it. On an account whose
    // on-demand limits are small, this is the difference between a usable
    // catalog and an empty one.
    let mut script = provision_script([vec![xml(RUN)], settles()].concat());
    script[3] = json(NO_ON_DEMAND_QUOTA);

    let mut aws = provider(script);
    let machine = aws
        .provision(&request(MachineId::generate(), X64_TYPE, true))
        .await
        .expect("spot draws on a different pool");
    assert_eq!(machine.capacity_mode, CapacityMode::Spot);

    let mut script = provision_script(vec![]);
    script[3] = json(NO_ON_DEMAND_QUOTA);
    let mut aws = provider(script);
    let error = aws
        .provision(&request(MachineId::generate(), X64_TYPE, false))
        .await
        .expect_err("the on-demand pool has no room at all");
    assert!(matches!(error, ProviderError::QuotaExceeded { .. }));
}

#[tokio::test]
async fn a_spot_machine_ec2_accepts_records_spot() {
    let mut aws = provider(provision_script([vec![xml(RUN)], settles()].concat()));
    let machine = aws
        .provision(&request(MachineId::generate(), ARM64_TYPE, true))
        .await
        .expect("provision");

    assert_eq!(machine.capacity_mode, CapacityMode::Spot);
}

#[tokio::test]
async fn a_refusal_that_is_not_about_spot_is_not_retried() {
    let mut aws = provider(provision_script(vec![
        refusal(include_str!("../../fixtures/aws/error_unauthorized.xml")),
        xml(RELEASED),
    ]));

    let error = aws
        .provision(&request(MachineId::generate(), ARM64_TYPE, true))
        .await
        .expect_err("a permission failure is a failure");
    assert_eq!(error.code(), Some("UnauthorizedOperation"));
    assert_eq!(
        actions(&aws)
            .iter()
            .filter(|name| *name == "RunInstances")
            .count(),
        1,
        "there must be no second launch"
    );
}

#[tokio::test]
async fn an_on_demand_request_never_carries_the_market_options() {
    let mut aws = provider(provision_script(vec![
        xml(RUN_ON_DEMAND),
        xml(ON_DEMAND_RUNNING),
        xml(ASSOCIATED),
    ]));
    aws.provision(&request(MachineId::generate(), ARM64_TYPE, false))
        .await
        .expect("provision");

    assert!(!has(
        &aws.transport().request(RUN_INSTANCES),
        "InstanceMarketOptions.MarketType"
    ));
}

// ── The state machine that stands in for an operation resource ──

#[tokio::test]
async fn an_instance_is_polled_until_it_settles() {
    let mut aws = provider(provision_script(vec![
        xml(RUN),
        // EC2 answers a launch with `pending` and keeps answering it: the
        // settled state is the only trustworthy one.
        xml(PENDING),
        xml(PENDING),
        xml(RUNNING),
        xml(ASSOCIATED),
    ]));

    aws.provision(&request(MachineId::generate(), ARM64_TYPE, true))
        .await
        .expect("provision");

    let described = actions(&aws)
        .iter()
        .skip(RUN_INSTANCES)
        .filter(|name| *name == "DescribeInstances")
        .count();
    assert_eq!(described, 3, "a non-terminal state means keep polling");
    assert_eq!(
        aws.timer().delays(),
        vec![1, 2, 5],
        "EC2 states no `Retry-After`, so the shared backoff paces it and grows"
    );
}

#[tokio::test]
async fn an_instance_that_settles_somewhere_else_is_a_failure() {
    // A spot launch that is interrupted before it comes up settles as
    // `terminated`, and waiting for `running` forever would be the wrong
    // answer to a machine that will never arrive.
    let mut aws = provider(provision_script(vec![
        xml(RUN),
        xml(PENDING),
        xml(TERMINATED),
    ]));

    let error = aws
        .provision(&request(MachineId::generate(), ARM64_TYPE, true))
        .await
        .expect_err("an instance that terminated is not an instance that started");
    assert!(matches!(
        error,
        ProviderError::OperationFailed { ref status, .. } if status == "terminated"
    ));
}

// ── Lifecycle ──

#[tokio::test]
async fn deallocating_stops_the_instance_and_waits_for_it_to_be_stopped() {
    let machine = provisioned(MachineId::generate());
    let mut aws = provider(vec![xml(STOP), xml(STOPPED)]);

    aws.deallocate(&machine).await.expect("deallocate");

    let stop = aws.transport().request(0);
    assert_eq!(action(&stop), "StopInstances");
    assert_eq!(field(&stop, "InstanceId.1"), INSTANCE);
    // `StopInstances` answers `stopping`, so the call is not the answer.
    assert_eq!(action(&aws.transport().request(1)), "DescribeInstances");
}

#[tokio::test]
async fn starting_starts_the_instance_and_waits_for_it_to_run() {
    let machine = provisioned(MachineId::generate());
    let mut aws = provider(vec![xml(START), xml(RUNNING)]);

    let started = aws.start(&machine).await.expect("start");
    assert_eq!(started.state, MachineState::Running);
    assert_eq!(
        started.address.as_deref(),
        Some(PUBLIC_IP),
        "an elastic IP stays associated across a stop, so the address does not move"
    );
    assert_eq!(action(&aws.transport().request(0)), "StartInstances");
}

#[tokio::test]
async fn resizing_stops_modifies_the_type_and_starts_again() {
    let machine = provisioned(MachineId::generate());
    let mut aws = provider(vec![
        xml(REGIONS),
        xml(TYPES),
        xml(OFFERINGS),
        json(QUOTAS),
        xml(NO_INSTANCES),
        xml(STOP),
        xml(STOPPED),
        xml(MODIFIED),
        xml(START),
        xml(RUNNING),
    ]);

    aws.resize(&machine, X64_TYPE)
        .await
        .expect("resize preserves the disk");

    let transport = aws.transport();
    assert_eq!(
        actions(&aws)[5..],
        [
            "StopInstances",
            // The attribute is settable only on a stopped instance, so the
            // stop has to have actually landed.
            "DescribeInstances",
            "ModifyInstanceAttribute",
            "StartInstances",
            "DescribeInstances",
        ]
    );

    let modify = transport.request(7);
    assert_eq!(field(&modify, "InstanceId"), INSTANCE);
    assert_eq!(
        field(&modify, "InstanceType.Value"),
        X64_TYPE,
        "the attribute is nested one level deep, and a flat `InstanceType` is ignored"
    );

    // Nothing touches the volume: the disk survives because it was never
    // being deleted.
    assert!(!actions(&aws).iter().any(|name| name == "DeleteVolume"));
}

#[tokio::test]
async fn a_resize_to_a_type_with_no_quota_never_stops_the_machine() {
    let machine = provisioned(MachineId::generate());
    let mut aws = provider(vec![xml(REGIONS), xml(TYPES), xml(OFFERINGS)]);

    aws.resize(&machine, UNOFFERED_TYPE)
        .await
        .expect_err("a resize into an unoffered type is refused");
    assert!(
        !actions(&aws).iter().any(|name| name == "StopInstances"),
        "nothing was stopped, so the session is still running"
    );
}

#[tokio::test]
async fn a_resize_whose_instance_settles_running_again_is_a_failure() {
    // EC2's documented trap: a `ModifyInstanceAttribute` sent to an instance
    // that is not stopped is refused, and an instance that never stopped
    // reports the old type with nothing to distinguish it.
    let machine = provisioned(MachineId::generate());
    let mut aws = provider(vec![
        xml(REGIONS),
        xml(TYPES),
        xml(OFFERINGS),
        json(QUOTAS),
        xml(NO_INSTANCES),
        xml(STOP),
        // It settled `running` rather than `stopped`.
        xml(RUNNING),
    ]);

    let error = aws
        .resize(&machine, X64_TYPE)
        .await
        .expect_err("an instance that did not stop cannot be modified");
    assert!(matches!(error, ProviderError::OperationFailed { .. }));
    assert!(
        !actions(&aws)
            .iter()
            .any(|name| name == "ModifyInstanceAttribute"),
        "the modification is never attempted against a running instance"
    );
}

#[tokio::test]
async fn destroying_removes_the_instance_then_everything_that_outlived_it() {
    let machine = provisioned(MachineId::generate());
    let mut aws = provider(vec![
        xml(RUNNING),
        xml(ADDRESSES),
        xml(TERMINATE),
        xml(TERMINATED),
        xml(RELEASED),
        // The volume detaches as the instance goes; deleting it before it
        // has is refused.
        xml(VOLUME_IN_USE),
        xml(VOLUME_AVAILABLE),
        xml(DELETED_VOLUME),
    ]);

    aws.destroy(&machine).await.expect("destroy");

    assert_eq!(
        actions(&aws),
        [
            // Both ids have to be read *before* anything is destroyed: the
            // volume's is only on the instance, and an address's association
            // is gone the moment the instance is.
            "DescribeInstances",
            "DescribeAddresses",
            "TerminateInstances",
            "DescribeInstances",
            "ReleaseAddress",
            "DescribeVolumes",
            "DescribeVolumes",
            "DeleteVolume",
        ]
    );

    let transport = aws.transport();
    assert_eq!(
        field(&transport.request(1), "Filter.1.Name"),
        format!("tag:{}", super::MACHINE_TAG),
        "an address is found by its tag, because its association will not survive"
    );
    assert_eq!(field(&transport.request(4), "AllocationId"), ALLOCATION);
    assert_eq!(field(&transport.request(7), "VolumeId"), VOLUME);
}

#[tokio::test]
async fn destroying_a_machine_with_no_address_still_deletes_its_volume() {
    let machine = provisioned(MachineId::generate());
    let mut aws = provider(vec![
        xml(RUNNING),
        xml(include_str!(
            "../../fixtures/aws/describe_addresses_none.xml"
        )),
        xml(TERMINATE),
        xml(TERMINATED),
        xml(VOLUME_AVAILABLE),
        xml(DELETED_VOLUME),
    ]);

    aws.destroy(&machine).await.expect("destroy");
    assert!(!actions(&aws).iter().any(|name| name == "ReleaseAddress"));
    assert!(actions(&aws).iter().any(|name| name == "DeleteVolume"));
}

// ── The catalog ──

fn one_region() -> AwsWorkspace {
    AwsWorkspace::new()
        .with_key_pair(KEY_PAIR)
        .with_regions(vec![REGION.to_owned()])
}

fn catalog_script() -> Vec<HttpResponse> {
    vec![
        xml(REGIONS),
        xml(TYPES),
        xml(OFFERINGS),
        json(QUOTAS),
        xml(NO_INSTANCES),
        json(PRODUCTS),
        json(STORAGE_PRODUCTS),
        xml(SPOT_PRICES),
    ]
}

#[tokio::test]
async fn the_catalog_offers_only_what_passes_all_three_gates() {
    let mut aws = provider_over(one_region(), catalog_script());

    let catalog = aws.catalog().await.expect("catalog");
    let offered: Vec<&str> = catalog
        .iter()
        .map(|entry| entry.machine_type.as_str())
        .collect();

    assert!(offered.contains(&ARM64_TYPE));
    assert!(offered.contains(&X64_TYPE));
    // Described by EC2, not offered in this region.
    assert!(!offered.contains(&UNOFFERED_TYPE));
    // Offered, but the Price List publishes no Linux meter for it, so flyco
    // cannot say what an hour would cost.
    assert!(!offered.contains(&UNPRICED_TYPE));

    let arm = catalog
        .iter()
        .find(|entry| entry.machine_type == ARM64_TYPE)
        .expect("the Arm type is offered");
    assert_eq!(arm.provider, CloudProviderKind::Aws);
    assert_eq!(arm.region, REGION);
    assert_eq!(
        arm.capacity,
        Some(flyco_core::MachineCapacity {
            vcpus: 2,
            memory_mib: 2_048
        })
    );
    assert_eq!(
        arm.pricing,
        flyco_core::MachinePricing::Metered {
            on_demand_hourly: flyco_core::Usd::from_micros(13_400),
            // The cheapest zone, because a launch that names none lands
            // wherever there is capacity and is billed at that zone's rate.
            spot_hourly: Some(flyco_core::Usd::from_micros(4_860)),
            minimum: None,
            storage: flyco_core::StoragePricing::PerGibHourly {
                rate: flyco_core::Usd::from_micros(110),
            },
        }
    );
}

#[tokio::test]
async fn an_ec2_mac_says_it_bills_a_day_at_a_time() {
    let mut aws = provider_over(one_region(), catalog_script());
    let catalog = aws.catalog().await.expect("catalog");

    let mac = catalog
        .iter()
        .find(|entry| entry.machine_type == MAC_TYPE)
        .expect("an EC2 Mac is a machine a session can run on");

    assert_eq!(
        mac.pricing,
        flyco_core::MachinePricing::Metered {
            on_demand_hourly: flyco_core::Usd::from_micros(650_000),
            // Not sold on the spot market at all.
            spot_hourly: None,
            // Apple's licence requires a 24-hour minimum allocation of the
            // dedicated host, so an hour of this is billed as a day — and a
            // budget told the hourly rate alone would be wrong by about
            // sixteen dollars.
            minimum: Some(flyco_core::BillingMinimum::new(
                24,
                flyco_core::Usd::from_micros(650_000)
            )),
            storage: flyco_core::StoragePricing::PerGibHourly {
                rate: flyco_core::Usd::from_micros(110),
            },
        }
    );
}

#[tokio::test]
async fn every_exclusion_says_which_of_the_problems_it_is() {
    let mut aws = provider_over(one_region(), catalog_script());

    let report = aws.region_report(REGION).await.expect("report");
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
        reason(UNOFFERED_TYPE),
        ExclusionReason::NotOffered(_)
    ));
    assert!(matches!(reason(UNPRICED_TYPE), ExclusionReason::Unpriced));
}

#[tokio::test]
async fn a_disabled_region_reports_one_exclusion_naming_the_opt_in() {
    let mut aws = provider(vec![xml(REGIONS)]);

    let report = aws.region_report(DISABLED_REGION).await.expect("report");

    assert_eq!(report.offered, Vec::new());
    assert_eq!(
        report.excluded.len(),
        1,
        "the answer is the same for every instance type in the region"
    );
    let (subject, reason) = &report.excluded[0];
    assert_eq!(subject, DISABLED_REGION);
    assert!(matches!(reason, ExclusionReason::RegionForbidden(_)));
}

#[tokio::test]
async fn a_named_region_the_account_has_not_enabled_is_dropped_rather_than_read() {
    let workspace =
        AwsWorkspace::new().with_regions(vec![DISABLED_REGION.to_owned(), REGION.to_owned()]);
    let mut aws = provider_over(workspace, catalog_script());

    aws.catalog().await.expect("catalog");
    assert!(
        aws.transport().request(1).url.contains(REGION),
        "the disabled region is skipped, not read"
    );
}

#[tokio::test]
async fn the_price_query_is_signed_against_the_price_lists_own_region() {
    let mut aws = provider_over(one_region(), catalog_script());
    aws.catalog().await.expect("catalog");

    let products = aws.transport().request(5);
    assert_eq!(products.url, "https://api.pricing.us-east-1.amazonaws.com/");
    assert_eq!(
        header(&products, "x-amz-target"),
        "AWSPriceListService.GetProducts"
    );
    assert!(
        header(&products, "authorization").contains("/20260829/us-east-1/pricing/aws4_request"),
        "the Price List has three endpoints world-wide and is signed against its own region"
    );

    let storage = aws.transport().request(6);
    assert_eq!(
        header(&storage, "x-amz-target"),
        "AWSPriceListService.GetProducts"
    );

    // Spot is a different service entirely: EC2, in the region being priced.
    let spot = aws.transport().request(7);
    assert_eq!(action(&spot), "DescribeSpotPriceHistory");
    assert_eq!(field(&spot, "ProductDescription.1"), "Linux/UNIX");
    assert_eq!(field(&spot, "StartTime"), "2026-08-29T12:00:00Z");
}

// ── Metered spend, and proving a key works ──

#[tokio::test]
async fn the_cost_query_asks_cost_explorer_for_this_month_unblended() {
    let mut aws = provider(vec![json(COST)]);

    let spend = aws
        .billing_period_cost(SIGNED_AT)
        .await
        .expect("read the metered spend");

    let query = aws.transport().request(0);
    assert_eq!(query.url, "https://ce.us-east-1.amazonaws.com/");
    assert_eq!(
        header(&query, "x-amz-target"),
        "AWSInsightsIndexService.GetCostAndUsage"
    );

    let body: serde_json::Value = serde_json::from_slice(&query.body).expect("a JSON body");
    assert_eq!(body["TimePeriod"]["Start"], "2026-08-01");
    assert_eq!(
        body["TimePeriod"]["End"], "2026-08-30",
        "the end is exclusive, so today is only covered when it ends tomorrow"
    );
    assert_eq!(body["Granularity"], "MONTHLY");
    assert_eq!(body["Metrics"][0], "UnblendedCost");

    assert_eq!(spend.spent, flyco_core::Usd::from_micros(41_270_000));
    assert_eq!(spend.period_start_unix, MONTH_START);
    assert_eq!(spend.period_end_unix, SIGNED_AT);
    assert_eq!(spend.remaining_credit, None);
}

#[tokio::test]
async fn the_identity_check_is_the_cheapest_call_that_proves_a_key() {
    let mut aws = provider(vec![xml(IDENTITY)]);

    let identity = aws.caller_identity().await.expect("the key is real");
    assert_eq!(identity.account, "123456789012");

    let request = aws.transport().request(0);
    assert_eq!(request.url, "https://sts.us-east-1.amazonaws.com/");
    assert_eq!(field(&request, "Action"), "GetCallerIdentity");
    assert_eq!(field(&request, "Version"), "2011-06-15");
    assert!(
        header(&request, "authorization").contains("/us-east-1/sts/aws4_request"),
        "a regional endpoint in a region the account never enabled would refuse a good key"
    );
}

#[tokio::test]
async fn a_refused_key_is_reported_with_the_code_aws_gave() {
    let mut aws = provider(vec![HttpResponse::new(
        403,
        include_bytes!("../../fixtures/aws/error_auth_failure.xml").to_vec(),
    )]);

    let error = aws
        .caller_identity()
        .await
        .expect_err("a bad key is an error");
    assert_eq!(error.code(), Some("AuthFailure"));
}

/// The one test that touches a real account.
///
/// Ignored *and* feature-gated, so neither `cargo test` nor
/// `cargo test -- --ignored` can start it by accident: it creates billable
/// resources. See this module's documentation for the variables it needs.
#[cfg(feature = "aws-live")]
#[tokio::test]
#[ignore = "creates real, billable AWS resources"]
async fn live_provision_and_destroy() {
    use crate::clock::{SystemClock, SystemTimer, SystemWallClock};
    use crate::http::LiveTransport;

    fn required(name: &str) -> String {
        std::env::var(name).unwrap_or_else(|_| panic!("the live test needs `{name}`"))
    }

    let key = super::sigv4::AccessKey::new(
        required("FLYCO_AWS_ACCESS_KEY_ID"),
        required("FLYCO_AWS_SECRET_ACCESS_KEY"),
    );
    let region = required("FLYCO_AWS_REGION");
    let mut workspace = AwsWorkspace::new().with_regions(vec![region.clone()]);
    if let Ok(key_pair) = std::env::var("FLYCO_AWS_KEY_PAIR") {
        workspace = workspace.with_key_pair(key_pair);
    }

    let mut aws = AwsProvider::with_parts(
        LiveTransport::new(),
        SystemClock::new(),
        SystemTimer::new(),
        SystemWallClock::new(),
        key,
        workspace,
    );

    let catalog = aws.catalog().await.expect("read the live catalog");
    assert!(
        !catalog.is_empty(),
        "the account can deploy nothing in the region it enabled"
    );

    let cheapest = catalog
        .iter()
        // A dedicated host bills 24 hours whatever the test does with it.
        .filter(|entry| {
            !matches!(
                entry.pricing,
                flyco_core::MachinePricing::Metered {
                    minimum: Some(_),
                    ..
                }
            )
        })
        .filter_map(|entry| entry.pricing.hourly(true).map(|price| (price, entry)))
        .min_by_key(|(price, _)| *price)
        .expect("a priced instance type")
        .1
        .machine_type
        .clone();

    let provision = request_in(MachineId::generate(), &region, &cheapest, true);
    let machine = aws.provision(&provision).await.expect("provision");
    aws.destroy(&machine).await.expect("destroy");
}
