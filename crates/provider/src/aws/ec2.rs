//! The EC2 API: its endpoints, the requests flyco sends, and the XML it
//! answers with.
//!
//! Requests are `#[derive(Serialize)]` structs encoded by [`super::query`],
//! for the reason `azure::bodies` gives: a renamed or dropped field fails
//! the build rather than producing a call the service accepts and reads
//! differently. Responses are `#[derive(Deserialize)]` structs read by
//! `quick-xml` — EC2 has no JSON dialect, so the one XML reader in the
//! workspace lives here.
//!
//! # There is no operation resource
//!
//! Azure hands back a URL that reports an operation's status. EC2 hands back
//! the *instance's own* state, and the state it hands back is the state
//! before the call took effect: `StopInstances` answers `stopping`, not
//! `stopped`. Every mutating call is therefore followed by reading
//! [`InstanceState`] until it settles — see [`super::AwsProvider`] — and the
//! terminal state is what is trusted, never a read-back of the attribute
//! that was set.

use serde::{Deserialize, Serialize};

use crate::ProviderError;
use crate::http::HttpResponse;

/// The EC2 API version every call pins.
///
/// EC2 has published one stable version since 2016; pinning it means a
/// service-side default cannot move the request shape under the driver.
pub const API_VERSION: &str = "2016-11-15";

/// Signing name of the EC2 service.
pub const SERVICE: &str = "ec2";

/// The regional endpoint of a service.
#[must_use]
pub fn endpoint(service: &str, region: &str) -> String {
    format!("https://{service}.{region}.amazonaws.com/")
}

// ── Errors ──

/// The error document EC2 answers a rejection with.
#[derive(Debug, Clone, Deserialize)]
struct ErrorResponse {
    #[serde(rename = "Errors")]
    errors: ErrorList,
}

/// The `Errors` wrapper, which holds one `Error` per problem.
#[derive(Debug, Clone, Default, Deserialize)]
struct ErrorList {
    #[serde(rename = "Error", default)]
    error: Vec<ErrorBody>,
}

/// One EC2 error.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ErrorBody {
    /// Machine-readable code, e.g. `InsufficientInstanceCapacity`.
    #[serde(rename = "Code", default)]
    pub code: String,
    /// Human-readable explanation.
    #[serde(rename = "Message", default)]
    pub message: String,
}

impl ErrorBody {
    /// Reads the first error out of a response body, if it carries one.
    ///
    /// A rejection whose body is not an EC2 error document — a load
    /// balancer's HTML, a throttling page — yields nothing, so the caller
    /// reports the status and the raw text rather than an empty code that
    /// reads like a real one.
    #[must_use]
    pub fn of(response: &HttpResponse) -> Option<Self> {
        let text = core::str::from_utf8(&response.body).ok()?;
        quick_xml::de::from_str::<ErrorResponse>(text)
            .ok()?
            .errors
            .error
            .into_iter()
            .next()
    }
}

/// Turns a refused EC2 response into an error that keeps its code.
#[must_use]
pub fn refusal(response: &HttpResponse) -> ProviderError {
    ErrorBody::of(response).map_or_else(
        || {
            ProviderError::Rejected(format!(
                "EC2 answered HTTP {}: {}",
                response.status,
                response.body_text()
            ))
        },
        |error| ProviderError::Refused {
            code: error.code,
            message: error.message,
        },
    )
}

/// Decodes a successful EC2 answer.
///
/// # Errors
///
/// Returns [`ProviderError::Malformed`] when the body is not the XML this
/// call was expecting.
pub fn decode<T: serde::de::DeserializeOwned>(response: &HttpResponse) -> Result<T, ProviderError> {
    let text = core::str::from_utf8(&response.body)
        .map_err(|_| ProviderError::Malformed("an EC2 response was not UTF-8"))?;
    quick_xml::de::from_str(text)
        .map_err(|_| ProviderError::Malformed("an EC2 response was not the expected XML"))
}

// ── Shared request shapes ──

/// One `Filter.N` on a describe call.
#[derive(Debug, Clone, Serialize)]
pub struct Filter {
    /// Filter name, e.g. `instance-state-name`.
    #[serde(rename = "Name")]
    pub name: String,
    /// Values, any of which matches.
    #[serde(rename = "Value")]
    pub value: Vec<String>,
}

impl Filter {
    /// A filter matching one value.
    #[must_use]
    pub fn is(name: &str, value: impl Into<String>) -> Self {
        Self {
            name: name.to_owned(),
            value: vec![value.into()],
        }
    }

    /// A filter matching any of several values.
    #[must_use]
    pub fn any_of(name: &str, values: &[&str]) -> Self {
        Self {
            name: name.to_owned(),
            value: values.iter().map(|value| (*value).to_owned()).collect(),
        }
    }
}

/// One tag applied at creation time.
#[derive(Debug, Clone, Serialize)]
pub struct Tag {
    /// Tag key.
    #[serde(rename = "Key")]
    pub key: String,
    /// Tag value.
    #[serde(rename = "Value")]
    pub value: String,
}

/// Tags to apply to one kind of resource a call creates.
///
/// Applied at creation rather than afterwards: a `CreateTags` that fails
/// leaves a resource nothing can find, which is exactly the orphan the
/// teardown ordering exists to prevent.
#[derive(Debug, Clone, Serialize)]
pub struct TagSpecification {
    /// `instance`, `volume`, `elastic-ip`, `security-group`.
    #[serde(rename = "ResourceType")]
    pub resource_type: &'static str,
    /// The tags themselves.
    #[serde(rename = "Tag")]
    pub tag: Vec<Tag>,
}

// ── RunInstances ──

/// Body of `RunInstances`.
#[derive(Debug, Clone, Serialize)]
pub struct RunInstances {
    /// AMI to boot.
    #[serde(rename = "ImageId")]
    pub image_id: String,
    /// Instance type.
    #[serde(rename = "InstanceType")]
    pub instance_type: String,
    /// Exactly one machine per session.
    #[serde(rename = "MinCount")]
    pub min_count: u32,
    /// Exactly one machine per session.
    #[serde(rename = "MaxCount")]
    pub max_count: u32,
    /// The subnet the interface lands in.
    #[serde(rename = "SubnetId")]
    pub subnet_id: String,
    /// The workspace security group.
    #[serde(rename = "SecurityGroupId")]
    pub security_group_id: Vec<String>,
    /// The user's own EC2 key pair, when they registered one.
    #[serde(rename = "KeyName", skip_serializing_if = "Option::is_none")]
    pub key_name: Option<String>,
    /// Base64 cloud-config.
    #[serde(rename = "UserData")]
    pub user_data: String,
    /// The root volume.
    #[serde(rename = "BlockDeviceMapping")]
    pub block_device_mapping: Vec<BlockDeviceMapping>,
    /// Spot, or absent for ordinary capacity — see
    /// [`RunInstances::without_spot`].
    #[serde(
        rename = "InstanceMarketOptions",
        skip_serializing_if = "Option::is_none"
    )]
    pub instance_market_options: Option<InstanceMarketOptions>,
    /// Ownership tags, on the instance and on the volume it creates.
    #[serde(rename = "TagSpecification")]
    pub tag_specification: Vec<TagSpecification>,
}

impl RunInstances {
    /// The same machine, requested as ordinary on-demand capacity.
    ///
    /// The market options come off as a unit, which is the whole of the
    /// difference: everything else about the instance — its image, its
    /// volume, its user data — is identical, and that is what makes the
    /// fallback a retry rather than a second, different machine.
    #[must_use]
    pub const fn without_spot(mut self) -> Self {
        self.instance_market_options = None;
        self
    }

    /// Whether this body asks for spot capacity.
    #[must_use]
    pub const fn is_spot(&self) -> bool {
        self.instance_market_options.is_some()
    }
}

/// One block device on a new instance.
#[derive(Debug, Clone, Serialize)]
pub struct BlockDeviceMapping {
    /// The image's own root device name, read from the AMI rather than
    /// assumed: it is `/dev/sda1` on some images and `/dev/xvda` on others.
    #[serde(rename = "DeviceName")]
    pub device_name: String,
    /// The EBS volume behind it.
    #[serde(rename = "Ebs")]
    pub ebs: Ebs,
}

/// An EBS root volume.
#[derive(Debug, Clone, Serialize)]
pub struct Ebs {
    /// Size in GiB.
    #[serde(rename = "VolumeSize")]
    pub volume_size: u32,
    /// `gp3`, the general-purpose tier.
    #[serde(rename = "VolumeType")]
    pub volume_type: &'static str,
    /// Always `false`, which is this driver's `Detach`: terminating the
    /// instance leaves the disk, and destroying deletes it explicitly.
    #[serde(rename = "DeleteOnTermination")]
    pub delete_on_termination: bool,
}

/// Which capacity market `RunInstances` is asking for.
#[derive(Debug, Clone, Serialize)]
pub struct InstanceMarketOptions {
    /// Always `spot`; ordinary capacity omits the whole structure.
    #[serde(rename = "MarketType")]
    pub market_type: MarketType,
    /// How the spot request behaves.
    #[serde(rename = "SpotOptions")]
    pub spot_options: SpotOptions,
}

/// The one market type flyco ever names.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MarketType {
    /// Interruptible capacity.
    Spot,
}

/// The spot request's own shape.
#[derive(Debug, Clone, Serialize)]
pub struct SpotOptions {
    /// `persistent`, so the request survives an interruption and brings the
    /// machine back rather than expiring with it.
    #[serde(rename = "SpotInstanceType")]
    pub spot_instance_type: SpotInstanceType,
    /// `stop`, which is the whole reason the request is persistent: a
    /// stopped instance keeps its EBS root volume, so an interruption costs
    /// the session its compute and not its work. `terminate` would delete
    /// the machine, and `hibernate` needs an instance-store-free type and
    /// enough volume for the RAM image.
    #[serde(rename = "InstanceInterruptionBehavior")]
    pub instance_interruption_behavior: InterruptionBehavior,
}

/// Whether a spot request outlives the instance it created.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SpotInstanceType {
    /// The request stays open, so an interrupted machine comes back.
    Persistent,
}

/// What an interruption does to the machine.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InterruptionBehavior {
    /// Stop it, keeping the EBS root volume.
    Stop,
}

/// Body of `DescribeImages`.
#[derive(Debug, Clone, Serialize)]
pub struct DescribeImages {
    /// The images to describe.
    #[serde(rename = "ImageId")]
    pub image_id: Vec<String>,
}

/// What `DescribeImages` answers.
#[derive(Debug, Clone, Deserialize)]
pub struct DescribeImagesResponse {
    /// The images matched.
    #[serde(rename = "imagesSet", default)]
    pub images_set: ImageSet,
}

/// The images on one page.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ImageSet {
    /// One entry per image.
    #[serde(rename = "item", default)]
    pub item: Vec<Image>,
}

/// One AMI.
#[derive(Debug, Clone, Deserialize)]
pub struct Image {
    /// `ami-…`.
    #[serde(rename = "imageId")]
    pub image_id: String,
    /// The device its root volume attaches at, which a block-device mapping
    /// has to name exactly or be ignored.
    #[serde(rename = "rootDeviceName", default)]
    pub root_device_name: Option<String>,
}

/// What `RunInstances` answers.
#[derive(Debug, Clone, Deserialize)]
pub struct RunInstancesResponse {
    /// The instances it created — exactly one, here.
    #[serde(rename = "instancesSet")]
    pub instances_set: InstanceSet,
}

// ── Describing instances ──

/// Body of `DescribeInstances`.
#[derive(Debug, Clone, Serialize)]
pub struct DescribeInstances {
    /// Instances to describe, when the caller names them.
    #[serde(rename = "InstanceId", skip_serializing_if = "Vec::is_empty")]
    pub instance_id: Vec<String>,
    /// Filters, when it does not.
    #[serde(rename = "Filter", skip_serializing_if = "Vec::is_empty")]
    pub filter: Vec<Filter>,
    /// Continuation token.
    #[serde(rename = "NextToken", skip_serializing_if = "Option::is_none")]
    pub next_token: Option<String>,
}

/// What `DescribeInstances` answers.
#[derive(Debug, Clone, Deserialize)]
pub struct DescribeInstancesResponse {
    /// One reservation per `RunInstances` that created the instances in it.
    #[serde(rename = "reservationSet", default)]
    pub reservation_set: ReservationSet,
    /// Continuation token, when the account has more instances than a page.
    #[serde(rename = "nextToken", default)]
    pub next_token: Option<String>,
}

/// The reservations on one page.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReservationSet {
    /// One entry per reservation.
    #[serde(rename = "item", default)]
    pub item: Vec<Reservation>,
}

/// One reservation.
#[derive(Debug, Clone, Deserialize)]
pub struct Reservation {
    /// The instances it holds.
    #[serde(rename = "instancesSet")]
    pub instances_set: InstanceSet,
}

/// The instances in one reservation.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct InstanceSet {
    /// One entry per instance.
    #[serde(rename = "item", default)]
    pub item: Vec<Instance>,
}

/// One instance, as EC2 reports it.
#[derive(Debug, Clone, Deserialize)]
pub struct Instance {
    /// `i-…`.
    #[serde(rename = "instanceId")]
    pub instance_id: String,
    /// Its type.
    #[serde(rename = "instanceType", default)]
    pub instance_type: String,
    /// Where it is in its lifecycle.
    #[serde(rename = "instanceState")]
    pub instance_state: InstanceStateBody,
    /// `spot` on an instance holding interruptible capacity, absent
    /// otherwise. This is what the machine actually got, as opposed to what
    /// was asked for.
    #[serde(rename = "instanceLifecycle", default)]
    pub instance_lifecycle: Option<String>,
    /// Public DNS name, once one exists.
    #[serde(rename = "dnsName", default)]
    pub dns_name: Option<String>,
    /// Public address, once one is associated.
    #[serde(rename = "ipAddress", default)]
    pub ip_address: Option<String>,
    /// The device the root volume is attached at.
    #[serde(rename = "rootDeviceName", default)]
    pub root_device_name: Option<String>,
    /// Attached block devices, which is where the root volume's id is.
    #[serde(rename = "blockDeviceMapping", default)]
    pub block_device_mapping: BlockDeviceMappingSet,
}

impl Instance {
    /// This instance's state.
    #[must_use]
    pub fn state(&self) -> InstanceState {
        InstanceState::parse(&self.instance_state.name)
    }

    /// Whether it holds interruptible capacity.
    ///
    /// Read off `instanceLifecycle` rather than off the request: a spot
    /// request EC2 refused was retried as on-demand, and the price flyco
    /// bills follows what was obtained.
    #[must_use]
    pub fn is_spot(&self) -> bool {
        self.instance_lifecycle.as_deref() == Some("spot")
    }

    /// The volume behind the root device, when the instance names one.
    #[must_use]
    pub fn root_volume(&self) -> Option<&str> {
        let root = self.root_device_name.as_deref()?;
        self.block_device_mapping
            .item
            .iter()
            .find(|mapping| mapping.device_name == root)
            .map(|mapping| mapping.ebs.volume_id.as_str())
    }

    /// The address the daemon bootstrap reaches it on.
    #[must_use]
    pub fn address(&self) -> Option<String> {
        self.dns_name
            .as_ref()
            .filter(|name| !name.is_empty())
            .or_else(|| self.ip_address.as_ref().filter(|ip| !ip.is_empty()))
            .cloned()
    }
}

/// An instance's state, as the XML nests it.
#[derive(Debug, Clone, Deserialize)]
pub struct InstanceStateBody {
    /// State name, e.g. `running`.
    #[serde(rename = "name", default)]
    pub name: String,
}

/// The block devices attached to an instance.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BlockDeviceMappingSet {
    /// One entry per device.
    #[serde(rename = "item", default)]
    pub item: Vec<AttachedBlockDevice>,
}

/// One attached block device.
#[derive(Debug, Clone, Deserialize)]
pub struct AttachedBlockDevice {
    /// Where it is attached.
    #[serde(rename = "deviceName")]
    pub device_name: String,
    /// The volume behind it.
    #[serde(rename = "ebs")]
    pub ebs: AttachedEbs,
}

/// The volume behind an attached device.
#[derive(Debug, Clone, Deserialize)]
pub struct AttachedEbs {
    /// `vol-…`.
    #[serde(rename = "volumeId")]
    pub volume_id: String,
}

/// Where an instance is in its lifecycle.
///
/// The transitional states are named rather than lumped together, because
/// which one an instance is in decides whether waiting will help: `pending`
/// becomes `running`, `stopping` becomes `stopped`, and `shutting-down`
/// becomes `terminated`. Anything unrecognised is treated as transitional
/// for the same reason Azure treats an unknown operation status that way —
/// a value the service invents must not read as "finished".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstanceState {
    /// Starting up.
    Pending,
    /// Running.
    Running,
    /// Stopping.
    Stopping,
    /// Stopped, disk retained.
    Stopped,
    /// Terminating.
    ShuttingDown,
    /// Gone.
    Terminated,
    /// Something else the service reported: keep waiting.
    Other(String),
}

impl InstanceState {
    /// Classifies a state name.
    #[must_use]
    pub fn parse(name: &str) -> Self {
        match name {
            "pending" => Self::Pending,
            "running" => Self::Running,
            "stopping" => Self::Stopping,
            "stopped" => Self::Stopped,
            "shutting-down" => Self::ShuttingDown,
            "terminated" => Self::Terminated,
            other => Self::Other(other.to_owned()),
        }
    }

    /// Whether the instance has settled: nothing further will happen without
    /// another call.
    #[must_use]
    pub const fn is_settled(&self) -> bool {
        matches!(self, Self::Running | Self::Stopped | Self::Terminated)
    }
}

// ── Lifecycle actions ──

/// Body of `StopInstances`, `StartInstances` and `TerminateInstances`, all
/// of which take exactly the same one parameter.
#[derive(Debug, Clone, Serialize)]
pub struct InstanceAction {
    /// The instances to act on.
    #[serde(rename = "InstanceId")]
    pub instance_id: Vec<String>,
}

impl InstanceAction {
    /// The action, against one instance.
    #[must_use]
    pub fn on(instance: &str) -> Self {
        Self {
            instance_id: vec![instance.to_owned()],
        }
    }
}

/// Body of `ModifyInstanceAttribute`, setting the instance type.
///
/// The attribute is settable only while the instance is stopped, which is
/// what makes a resize a stop-modify-start rather than a single call.
#[derive(Debug, Clone, Serialize)]
pub struct ModifyInstanceType {
    /// The instance to change.
    #[serde(rename = "InstanceId")]
    pub instance_id: String,
    /// The new type, in the `Value` sub-field this attribute takes.
    #[serde(rename = "InstanceType")]
    pub instance_type: AttributeValue,
}

/// A scalar instance attribute, which EC2 nests one level deep.
#[derive(Debug, Clone, Serialize)]
pub struct AttributeValue {
    /// The value.
    #[serde(rename = "Value")]
    pub value: String,
}

// ── Addresses ──

/// Body of `AllocateAddress`.
#[derive(Debug, Clone, Serialize)]
pub struct AllocateAddress {
    /// `vpc`, the only domain left.
    #[serde(rename = "Domain")]
    pub domain: &'static str,
    /// Tags, applied at allocation so the address is findable at teardown
    /// even after it has been disassociated.
    #[serde(rename = "TagSpecification")]
    pub tag_specification: Vec<TagSpecification>,
}

/// What `AllocateAddress` answers.
#[derive(Debug, Clone, Deserialize)]
pub struct AllocateAddressResponse {
    /// `eipalloc-…`.
    #[serde(rename = "allocationId")]
    pub allocation_id: String,
    /// The address itself.
    #[serde(rename = "publicIp")]
    pub public_ip: String,
}

/// Body of `AssociateAddress`.
#[derive(Debug, Clone, Serialize)]
pub struct AssociateAddress {
    /// The allocation to attach.
    #[serde(rename = "AllocationId")]
    pub allocation_id: String,
    /// The instance to attach it to.
    #[serde(rename = "InstanceId")]
    pub instance_id: String,
}

/// Body of `ReleaseAddress`.
#[derive(Debug, Clone, Serialize)]
pub struct ReleaseAddress {
    /// The allocation to release.
    #[serde(rename = "AllocationId")]
    pub allocation_id: String,
}

/// Body of `DescribeAddresses`.
#[derive(Debug, Clone, Serialize)]
pub struct DescribeAddresses {
    /// Filters — flyco looks an address up by the machine it belongs to.
    #[serde(rename = "Filter")]
    pub filter: Vec<Filter>,
}

/// What `DescribeAddresses` answers.
#[derive(Debug, Clone, Deserialize)]
pub struct DescribeAddressesResponse {
    /// The addresses matched.
    #[serde(rename = "addressesSet", default)]
    pub addresses_set: AddressSet,
}

/// The addresses on one page.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AddressSet {
    /// One entry per address.
    #[serde(rename = "item", default)]
    pub item: Vec<Address>,
}

/// One elastic IP.
#[derive(Debug, Clone, Deserialize)]
pub struct Address {
    /// `eipalloc-…`.
    #[serde(rename = "allocationId")]
    pub allocation_id: String,
    /// The address itself.
    #[serde(rename = "publicIp", default)]
    pub public_ip: String,
}

// ── Volumes ──

/// Body of `DeleteVolume`.
#[derive(Debug, Clone, Serialize)]
pub struct DeleteVolume {
    /// The volume to delete.
    #[serde(rename = "VolumeId")]
    pub volume_id: String,
}

/// Body of `DescribeVolumes`.
#[derive(Debug, Clone, Serialize)]
pub struct DescribeVolumes {
    /// The volumes to describe.
    #[serde(rename = "VolumeId")]
    pub volume_id: Vec<String>,
}

/// What `DescribeVolumes` answers.
#[derive(Debug, Clone, Deserialize)]
pub struct DescribeVolumesResponse {
    /// The volumes matched.
    #[serde(rename = "volumeSet", default)]
    pub volume_set: VolumeSet,
}

/// The volumes on one page.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct VolumeSet {
    /// One entry per volume.
    #[serde(rename = "item", default)]
    pub item: Vec<Volume>,
}

/// One EBS volume.
#[derive(Debug, Clone, Deserialize)]
pub struct Volume {
    /// `vol-…`.
    #[serde(rename = "volumeId")]
    pub volume_id: String,
    /// `creating`, `available`, `in-use`, `deleting`, `deleted`, `error`.
    #[serde(rename = "status", default)]
    pub status: String,
}

impl Volume {
    /// Whether the volume is detached and can therefore be deleted.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.status == "available"
    }
}

// ── Workspace network ──

/// Body of `DescribeVpcs`.
#[derive(Debug, Clone, Serialize)]
pub struct DescribeVpcs {
    /// Filters — flyco asks for the account's default VPC.
    #[serde(rename = "Filter")]
    pub filter: Vec<Filter>,
}

/// What `DescribeVpcs` answers.
#[derive(Debug, Clone, Deserialize)]
pub struct DescribeVpcsResponse {
    /// The VPCs matched.
    #[serde(rename = "vpcSet", default)]
    pub vpc_set: VpcSet,
}

/// The VPCs on one page.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct VpcSet {
    /// One entry per VPC.
    #[serde(rename = "item", default)]
    pub item: Vec<Vpc>,
}

/// One VPC.
#[derive(Debug, Clone, Deserialize)]
pub struct Vpc {
    /// `vpc-…`.
    #[serde(rename = "vpcId")]
    pub vpc_id: String,
}

/// Body of `DescribeSubnets`.
#[derive(Debug, Clone, Serialize)]
pub struct DescribeSubnets {
    /// Filters — the subnets of one VPC.
    #[serde(rename = "Filter")]
    pub filter: Vec<Filter>,
}

/// What `DescribeSubnets` answers.
#[derive(Debug, Clone, Deserialize)]
pub struct DescribeSubnetsResponse {
    /// The subnets matched.
    #[serde(rename = "subnetSet", default)]
    pub subnet_set: SubnetSet,
}

/// The subnets on one page.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SubnetSet {
    /// One entry per subnet.
    #[serde(rename = "item", default)]
    pub item: Vec<Subnet>,
}

/// One subnet.
#[derive(Debug, Clone, Deserialize)]
pub struct Subnet {
    /// `subnet-…`.
    #[serde(rename = "subnetId")]
    pub subnet_id: String,
    /// Which availability zone it is in.
    #[serde(rename = "availabilityZone", default)]
    pub availability_zone: String,
    /// Whether an instance launched here gets a public address without an
    /// elastic IP. Flyco attaches one either way — an auto-assigned address
    /// is lost on stop, and a session that comes back from a spot
    /// interruption at a different address is a session nothing can find.
    #[serde(rename = "mapPublicIpOnLaunch", default)]
    pub map_public_ip_on_launch: bool,
}

/// Body of `DescribeSecurityGroups`.
#[derive(Debug, Clone, Serialize)]
pub struct DescribeSecurityGroups {
    /// Filters — the workspace group, by name, inside one VPC.
    #[serde(rename = "Filter")]
    pub filter: Vec<Filter>,
}

/// What `DescribeSecurityGroups` answers.
#[derive(Debug, Clone, Deserialize)]
pub struct DescribeSecurityGroupsResponse {
    /// The groups matched.
    #[serde(rename = "securityGroupInfo", default)]
    pub security_group_info: SecurityGroupSet,
}

/// The groups on one page.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SecurityGroupSet {
    /// One entry per group.
    #[serde(rename = "item", default)]
    pub item: Vec<SecurityGroup>,
}

/// One security group.
#[derive(Debug, Clone, Deserialize)]
pub struct SecurityGroup {
    /// `sg-…`.
    #[serde(rename = "groupId")]
    pub group_id: String,
}

/// Body of `CreateSecurityGroup`.
#[derive(Debug, Clone, Serialize)]
pub struct CreateSecurityGroup {
    /// The group's name, which is how it is found again.
    #[serde(rename = "GroupName")]
    pub group_name: String,
    /// Required by the API, and read by whoever inherits the account.
    #[serde(rename = "GroupDescription")]
    pub group_description: String,
    /// The VPC it belongs to.
    #[serde(rename = "VpcId")]
    pub vpc_id: String,
}

/// What `CreateSecurityGroup` answers.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateSecurityGroupResponse {
    /// `sg-…`.
    #[serde(rename = "groupId")]
    pub group_id: String,
}

/// Body of `AuthorizeSecurityGroupIngress`.
#[derive(Debug, Clone, Serialize)]
pub struct AuthorizeSecurityGroupIngress {
    /// The group to open.
    #[serde(rename = "GroupId")]
    pub group_id: String,
    /// The permissions to add.
    #[serde(rename = "IpPermissions")]
    pub ip_permissions: Vec<IpPermission>,
}

/// One inbound rule.
#[derive(Debug, Clone, Serialize)]
pub struct IpPermission {
    /// `tcp`.
    #[serde(rename = "IpProtocol")]
    pub ip_protocol: &'static str,
    /// First port of the range.
    #[serde(rename = "FromPort")]
    pub from_port: u16,
    /// Last port of the range.
    #[serde(rename = "ToPort")]
    pub to_port: u16,
    /// Where it may be reached from.
    #[serde(rename = "IpRanges")]
    pub ip_ranges: Vec<IpRange>,
}

/// One CIDR an inbound rule admits.
#[derive(Debug, Clone, Serialize)]
pub struct IpRange {
    /// The CIDR.
    #[serde(rename = "CidrIp")]
    pub cidr_ip: &'static str,
}

// ── The catalog's reads ──

/// Body of `DescribeRegions`.
#[derive(Debug, Clone, Serialize)]
pub struct DescribeRegions {
    /// `true`, so regions the account has *not* opted into are listed too —
    /// which is the only way to tell "not enabled here" from "does not
    /// exist".
    #[serde(rename = "AllRegions")]
    pub all_regions: bool,
}

/// What `DescribeRegions` answers.
#[derive(Debug, Clone, Deserialize)]
pub struct DescribeRegionsResponse {
    /// The regions.
    #[serde(rename = "regionInfo", default)]
    pub region_info: RegionSet,
}

/// The regions on one page.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RegionSet {
    /// One entry per region.
    #[serde(rename = "item", default)]
    pub item: Vec<Region>,
}

/// One region, and whether this account may use it.
#[derive(Debug, Clone, Deserialize)]
pub struct Region {
    /// e.g. `eu-west-1`.
    #[serde(rename = "regionName")]
    pub region_name: String,
    /// `opt-in-not-required`, `opted-in`, or `not-opted-in`.
    #[serde(rename = "optInStatus", default)]
    pub opt_in_status: String,
}

/// Body of `DescribeInstanceTypeOfferings`.
#[derive(Debug, Clone, Serialize)]
pub struct DescribeInstanceTypeOfferings {
    /// `region`, so the answer is what the region offers rather than what
    /// one availability zone does.
    #[serde(rename = "LocationType")]
    pub location_type: &'static str,
    /// Filters — the region itself.
    #[serde(rename = "Filter")]
    pub filter: Vec<Filter>,
    /// The page size, at the API's maximum.
    #[serde(rename = "MaxResults")]
    pub max_results: u32,
    /// Continuation token.
    #[serde(rename = "NextToken", skip_serializing_if = "Option::is_none")]
    pub next_token: Option<String>,
}

/// What `DescribeInstanceTypeOfferings` answers.
#[derive(Debug, Clone, Deserialize)]
pub struct DescribeInstanceTypeOfferingsResponse {
    /// The offerings.
    #[serde(rename = "instanceTypeOfferingSet", default)]
    pub instance_type_offering_set: OfferingSet,
    /// Continuation token.
    #[serde(rename = "nextToken", default)]
    pub next_token: Option<String>,
}

/// The offerings on one page.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OfferingSet {
    /// One entry per offering.
    #[serde(rename = "item", default)]
    pub item: Vec<Offering>,
}

/// One instance type offered somewhere.
#[derive(Debug, Clone, Deserialize)]
pub struct Offering {
    /// The type offered.
    #[serde(rename = "instanceType")]
    pub instance_type: String,
}

/// Body of `DescribeInstanceTypes`.
#[derive(Debug, Clone, Serialize)]
pub struct DescribeInstanceTypes {
    /// The page size, at the API's maximum.
    #[serde(rename = "MaxResults")]
    pub max_results: u32,
    /// Continuation token.
    #[serde(rename = "NextToken", skip_serializing_if = "Option::is_none")]
    pub next_token: Option<String>,
}

/// What `DescribeInstanceTypes` answers.
#[derive(Debug, Clone, Deserialize)]
pub struct DescribeInstanceTypesResponse {
    /// The types.
    #[serde(rename = "instanceTypeSet", default)]
    pub instance_type_set: InstanceTypeSet,
    /// Continuation token.
    #[serde(rename = "nextToken", default)]
    pub next_token: Option<String>,
}

/// The instance types on one page.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct InstanceTypeSet {
    /// One entry per type.
    #[serde(rename = "item", default)]
    pub item: Vec<InstanceTypeInfo>,
}

/// One instance type's shape.
#[derive(Debug, Clone, Deserialize)]
pub struct InstanceTypeInfo {
    /// e.g. `t4g.small`.
    #[serde(rename = "instanceType")]
    pub instance_type: String,
    /// Which markets it can be bought on.
    #[serde(rename = "supportedUsageClasses", default)]
    pub supported_usage_classes: UsageClassSet,
    /// Which instruction sets it runs, which decides the AMI.
    #[serde(rename = "processorInfo", default)]
    pub processor_info: ProcessorInfo,
    /// Its virtual CPU count.
    #[serde(rename = "vCpuInfo", default)]
    pub vcpu_info: VCpuInfo,
    /// Its memory.
    #[serde(rename = "memoryInfo", default)]
    pub memory_info: MemoryInfo,
}

/// The markets an instance type is sold on.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsageClassSet {
    /// `on-demand`, `spot`, `capacity-block`.
    #[serde(rename = "item", default)]
    pub item: Vec<String>,
}

/// An instance type's processor.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProcessorInfo {
    /// The instruction sets it supports.
    #[serde(rename = "supportedArchitectures", default)]
    pub supported_architectures: ArchitectureSet,
}

/// The instruction sets an instance type supports.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ArchitectureSet {
    /// `x86_64`, `arm64`, `i386`, `x86_64_mac`, `arm64_mac`.
    #[serde(rename = "item", default)]
    pub item: Vec<String>,
}

/// An instance type's virtual CPUs.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct VCpuInfo {
    /// The count.
    #[serde(rename = "defaultVCpus", default)]
    pub default_vcpus: u32,
}

/// An instance type's memory.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MemoryInfo {
    /// Size in MiB, which is what EC2 states it in.
    #[serde(rename = "sizeInMiB", default)]
    pub size_in_mib: u64,
}

impl InstanceTypeInfo {
    /// Whether the type can be bought as spot at all.
    ///
    /// The direct analogue of Azure's spot-ineligible B-series: an instance
    /// type outside the spot market must never be quoted a spot price, and
    /// must never be asked for as one.
    #[must_use]
    pub fn supports_spot(&self) -> bool {
        self.supported_usage_classes
            .item
            .iter()
            .any(|class| class == "spot")
    }

    /// The instruction set an AMI has to match.
    ///
    /// Chosen from the type's own answer, never from its name: `t4g` is Arm
    /// and `t3` is x86, and pairing either with the other's image fails at
    /// launch.
    #[must_use]
    pub fn architecture(&self) -> Option<&str> {
        self.processor_info
            .supported_architectures
            .item
            .iter()
            .map(String::as_str)
            .find(|architecture| matches!(*architecture, "x86_64" | "arm64"))
    }
}

// ── Spot prices ──

/// Body of `DescribeSpotPriceHistory`.
#[derive(Debug, Clone, Serialize)]
pub struct DescribeSpotPriceHistory {
    /// `Linux/UNIX`, the only product description flyco boots.
    #[serde(rename = "ProductDescription")]
    pub product_description: Vec<String>,
    /// The instant to price at — the same one everything else in the call is
    /// signed with, so a catalog's prices are all as of one moment.
    #[serde(rename = "StartTime")]
    pub start_time: String,
    /// The page size.
    #[serde(rename = "MaxResults")]
    pub max_results: u32,
    /// Continuation token.
    #[serde(rename = "NextToken", skip_serializing_if = "Option::is_none")]
    pub next_token: Option<String>,
}

/// What `DescribeSpotPriceHistory` answers.
#[derive(Debug, Clone, Deserialize)]
pub struct DescribeSpotPriceHistoryResponse {
    /// The prices.
    #[serde(rename = "spotPriceHistorySet", default)]
    pub spot_price_history_set: SpotPriceSet,
    /// Continuation token.
    #[serde(rename = "nextToken", default)]
    pub next_token: Option<String>,
}

/// The spot prices on one page.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SpotPriceSet {
    /// One entry per instance type and availability zone.
    #[serde(rename = "item", default)]
    pub item: Vec<SpotPrice>,
}

/// One spot price, in one availability zone.
#[derive(Debug, Clone, Deserialize)]
pub struct SpotPrice {
    /// The type priced.
    #[serde(rename = "instanceType")]
    pub instance_type: String,
    /// The price per hour, as a decimal string.
    #[serde(rename = "spotPrice", default)]
    pub spot_price: String,
    /// Which zone it applies to.
    #[serde(rename = "availabilityZone", default)]
    pub availability_zone: String,
}

#[cfg(test)]
mod tests {
    use super::{
        DescribeInstancesResponse, ErrorBody, InstanceState, InstanceTypeInfo,
        RunInstancesResponse, decode, endpoint,
    };
    use crate::http::HttpResponse;

    const RUN: &str = include_str!("../../fixtures/aws/run_instances.xml");
    const RUNNING: &str = include_str!("../../fixtures/aws/describe_instances_running.xml");
    const TYPES: &str = include_str!("../../fixtures/aws/describe_instance_types.xml");

    fn xml(body: &str) -> HttpResponse {
        HttpResponse::new(200, body.as_bytes().to_vec())
    }

    #[test]
    fn an_endpoint_is_the_services_regional_one() {
        assert_eq!(
            endpoint("ec2", "eu-north-1"),
            "https://ec2.eu-north-1.amazonaws.com/"
        );
    }

    #[test]
    fn a_run_instances_answer_names_the_instance_it_created() {
        let answer: RunInstancesResponse = decode(&xml(RUN)).expect("decode");
        let instance = &answer.instances_set.item[0];

        assert_eq!(instance.instance_id, "i-0d3f9a1c2b4e5f607");
        assert_eq!(instance.state(), InstanceState::Pending);
        assert!(
            instance.is_spot(),
            "the capacity actually obtained is read off `instanceLifecycle`"
        );
    }

    #[test]
    fn a_running_instance_reports_its_address_and_root_volume() {
        let answer: DescribeInstancesResponse = decode(&xml(RUNNING)).expect("decode");
        let instance = &answer.reservation_set.item[0].instances_set.item[0];

        assert_eq!(instance.state(), InstanceState::Running);
        assert_eq!(
            instance.address().as_deref(),
            Some("ec2-52-13-24-9.us-west-2.compute.amazonaws.com")
        );
        // The root volume, matched by the instance's own root device name
        // rather than by position: an instance can carry several.
        assert_eq!(instance.root_volume(), Some("vol-04ac6f9e1d2b3c4d5"));
    }

    #[test]
    fn an_instance_types_shape_comes_from_its_own_answer() {
        let answer: super::DescribeInstanceTypesResponse = decode(&xml(TYPES)).expect("decode");
        let of = |name: &str| -> InstanceTypeInfo {
            answer
                .instance_type_set
                .item
                .iter()
                .find(|entry| entry.instance_type == name)
                .unwrap_or_else(|| panic!("the fixture holds `{name}`"))
                .clone()
        };

        let arm = of("t4g.small");
        assert_eq!(arm.architecture(), Some("arm64"));
        assert_eq!(arm.vcpu_info.default_vcpus, 2);
        assert_eq!(arm.memory_info.size_in_mib, 2_048);
        assert!(arm.supports_spot());

        assert_eq!(of("t3.small").architecture(), Some("x86_64"));
        // Dedicated Mac hosts are not sold on the spot market at all.
        assert!(!of("mac2.metal").supports_spot());
    }

    #[test]
    fn only_the_settled_states_are_settled() {
        for settled in ["running", "stopped", "terminated"] {
            assert!(InstanceState::parse(settled).is_settled());
        }
        // A transitional state, or one EC2 invents tomorrow, means keep
        // waiting — never "done".
        for waiting in ["pending", "stopping", "shutting-down", "rebooting"] {
            assert!(
                !InstanceState::parse(waiting).is_settled(),
                "`{waiting}` must not be read as settled"
            );
        }
    }

    #[test]
    fn an_ec2_error_document_yields_its_code() {
        let response = HttpResponse::new(
            400,
            include_bytes!("../../fixtures/aws/error_insufficient_capacity.xml").to_vec(),
        );
        let error = ErrorBody::of(&response).expect("an EC2 error document");
        assert_eq!(error.code, "InsufficientInstanceCapacity");
    }

    #[test]
    fn a_rejection_that_is_not_an_ec2_document_yields_no_code() {
        assert!(ErrorBody::of(&HttpResponse::new(502, b"<html>gateway</html>".to_vec())).is_none());
    }
}
