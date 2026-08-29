//! The request bodies ARM is sent, as typed documents.
//!
//! Serde structures rather than hand-built JSON, so a renamed or dropped
//! field fails the build instead of producing a `PUT` that Azure accepts and
//! silently interprets differently. Every shape here matches the measured
//! reference in `docs/research/azure-arm.md`.
//!
//! # Two absences are load-bearing
//!
//! * **No `zones`, anywhere.** Zone-restricted-but-usable is the dominant
//!   pattern for the SKUs a student subscription can run: in `westus2` every
//!   x64 B-series SKU is restricted in all three zones and in no location. A
//!   regional deployment succeeds and a zonal one fails, so the field is not
//!   modelled at all rather than being an `Option` somebody could set.
//! * **No `adminPassword`.** Linux authentication is public-key only, and
//!   the key is the one the user registered — flyco never holds a private
//!   key for a machine it provisions.
//!
//! A handful of fields carry an explicit `rename` on top of the struct's
//! `rename_all = "camelCase"`, because ARM capitalises the acronym in
//! `publicIPAllocationMethod`, `privateIPAllocationMethod`,
//! `publicIPAddressVersion`, `publicIPAddress` and `diskSizeGB`, and the
//! derived name would be silently ignored by a service that accepts unknown
//! fields.

use serde::Serialize;

/// A reference to another ARM resource, which is always by full id.
#[derive(Debug, Clone, Serialize)]
pub struct ResourceRef {
    /// The referenced resource's full ARM id.
    pub id: String,
}

/// What happens to an attached resource when its parent is deleted.
///
/// `Detach` everywhere: a session that is being rebuilt keeps its disk, its
/// address and its DNS label, which is what makes a resume a resume.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteOption {
    /// `Detach` or `Delete`.
    pub delete_option: &'static str,
}

/// Keep the resource when its parent goes.
pub const DETACH: &str = "Detach";

// ── Virtual network ──

/// Body of the workspace virtual network's `PUT`.
#[derive(Debug, Clone, Serialize)]
pub struct VirtualNetwork {
    /// Region.
    pub location: String,
    /// Address space and inline subnets.
    pub properties: VirtualNetworkProperties,
}

/// A virtual network's properties.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualNetworkProperties {
    /// The network's address space.
    pub address_space: AddressSpace,
    /// Subnets, declared inline: ARM has no "create the subnet too" flag,
    /// but a subnet inside the network's own body is one `PUT` rather than
    /// two.
    pub subnets: Vec<Subnet>,
}

/// A network's address prefixes.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddressSpace {
    /// CIDR blocks.
    pub address_prefixes: Vec<String>,
}

/// One subnet.
#[derive(Debug, Clone, Serialize)]
pub struct Subnet {
    /// Subnet name.
    pub name: String,
    /// Its address prefix.
    pub properties: SubnetProperties,
}

/// A subnet's properties.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubnetProperties {
    /// CIDR block.
    pub address_prefix: String,
}

// ── Network security group ──

/// Body of the workspace network security group's `PUT`.
///
/// Not optional. Basic public IPs were retired on 2025-09-30, Standard ones
/// are closed to inbound traffic by default, and a machine provisioned
/// without this comes up perfectly and answers nothing.
#[derive(Debug, Clone, Serialize)]
pub struct NetworkSecurityGroup {
    /// Region.
    pub location: String,
    /// The rules.
    pub properties: NetworkSecurityGroupProperties,
}

/// A security group's properties.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkSecurityGroupProperties {
    /// Inbound and outbound rules.
    pub security_rules: Vec<SecurityRule>,
}

/// One security rule.
#[derive(Debug, Clone, Serialize)]
pub struct SecurityRule {
    /// Rule name.
    pub name: String,
    /// What it allows.
    pub properties: SecurityRuleProperties,
}

/// A security rule's properties.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecurityRuleProperties {
    /// `Tcp`, `Udp`, or `*`.
    pub protocol: &'static str,
    /// Source ports.
    pub source_port_range: &'static str,
    /// Destination ports.
    pub destination_port_range: String,
    /// Source addresses, or a service tag such as `Internet`.
    pub source_address_prefix: String,
    /// Destination addresses.
    pub destination_address_prefix: &'static str,
    /// `Allow` or `Deny`.
    pub access: &'static str,
    /// Evaluation order; lower wins.
    pub priority: u32,
    /// `Inbound` or `Outbound`.
    pub direction: &'static str,
}

// ── Public IP ──

/// Body of a session's public IP `PUT`.
#[derive(Debug, Clone, Serialize)]
pub struct PublicIpAddress {
    /// Region.
    pub location: String,
    /// SKU. Standard is the only one left; Basic was retired.
    pub sku: PublicIpSku,
    /// Allocation and DNS.
    pub properties: PublicIpProperties,
}

/// A public IP's SKU.
#[derive(Debug, Clone, Serialize)]
pub struct PublicIpSku {
    /// `Standard`.
    pub name: &'static str,
    /// `Regional`, never zonal.
    pub tier: &'static str,
}

/// A public IP's properties.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicIpProperties {
    /// `IPv4`.
    #[serde(rename = "publicIPAddressVersion")]
    pub public_ip_address_version: &'static str,
    /// `Static`, which Standard requires — and which is why the address and
    /// its DNS label survive a deallocate/start cycle.
    #[serde(rename = "publicIPAllocationMethod")]
    pub public_ip_allocation_method: &'static str,
    /// Idle timeout in minutes.
    pub idle_timeout_in_minutes: u32,
    /// The DNS label, so a session is reachable by name rather than by an
    /// address something has to read back.
    pub dns_settings: DnsSettings,
}

/// A public IP's DNS label.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DnsSettings {
    /// Label, unique within the region.
    pub domain_name_label: String,
}

// ── Network interface ──

/// Body of a session's network interface `PUT`.
#[derive(Debug, Clone, Serialize)]
pub struct NetworkInterface {
    /// Region.
    pub location: String,
    /// What it joins.
    pub properties: NetworkInterfaceProperties,
}

/// A network interface's properties.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkInterfaceProperties {
    /// The security group that opens the port.
    pub network_security_group: ResourceRef,
    /// Its addresses.
    pub ip_configurations: Vec<IpConfiguration>,
}

/// One IP configuration on an interface.
#[derive(Debug, Clone, Serialize)]
pub struct IpConfiguration {
    /// Configuration name.
    pub name: &'static str,
    /// What it binds.
    pub properties: IpConfigurationProperties,
}

/// An IP configuration's properties.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IpConfigurationProperties {
    /// Whether this is the primary configuration.
    pub primary: bool,
    /// `Dynamic`, for the private address.
    #[serde(rename = "privateIPAllocationMethod")]
    pub private_ip_allocation_method: &'static str,
    /// The subnet it sits in.
    pub subnet: ResourceRef,
    /// The public address attached to it.
    #[serde(rename = "publicIPAddress")]
    pub public_ip_address: AttachedResource,
}

/// A referenced resource that survives its parent's deletion.
#[derive(Debug, Clone, Serialize)]
pub struct AttachedResource {
    /// The referenced resource's id.
    pub id: String,
    /// What happens to it when the parent goes.
    pub properties: DeleteOption,
}

// ── Virtual machine ──

/// Body of a session's virtual machine `PUT`.
#[derive(Debug, Clone, Serialize)]
pub struct VirtualMachine {
    /// Region.
    pub location: String,
    /// Ownership tags, so a resource group shared with other work stays
    /// legible.
    pub tags: MachineTags,
    /// Everything else.
    pub properties: VirtualMachineProperties,
}

/// Tags every flyco machine carries.
#[derive(Debug, Clone, Serialize)]
pub struct MachineTags {
    /// Always `flyco`.
    pub owner: &'static str,
    /// The session this machine serves.
    pub session: String,
    /// Flyco's machine id.
    pub machine: String,
}

/// A virtual machine's properties.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualMachineProperties {
    /// `Spot`, or absent for ordinary capacity. The three spot fields are
    /// omitted together — see [`VirtualMachineProperties::without_spot`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<&'static str>,
    /// What happens on eviction. `Deallocate` keeps the disk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eviction_policy: Option<&'static str>,
    /// The price cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub billing_profile: Option<BillingProfile>,
    /// The machine type.
    pub hardware_profile: HardwareProfile,
    /// The image and the disk.
    pub storage_profile: StorageProfile,
    /// The login and the cloud-init document.
    pub os_profile: OsProfile,
    /// The interface it is reachable on.
    pub network_profile: NetworkProfile,
    /// Boot diagnostics, so a machine that never phones home can still be
    /// looked at.
    pub diagnostics_profile: DiagnosticsProfile,
}

impl VirtualMachineProperties {
    /// The same machine, requested as ordinary on-demand capacity.
    ///
    /// The three spot fields come off together because Azure rejects any
    /// partial combination, and this is the exact body re-sent when a
    /// subscription or a SKU turns out not to support spot.
    #[must_use]
    pub const fn without_spot(mut self) -> Self {
        self.priority = None;
        self.eviction_policy = None;
        self.billing_profile = None;
        self
    }

    /// Whether this body asks for spot capacity.
    #[must_use]
    pub const fn is_spot(&self) -> bool {
        self.priority.is_some()
    }
}

/// The spot price cap.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BillingProfile {
    /// `-1` means "never evict me over price": the lesser of the current
    /// spot price and the standard price is paid, and eviction is a capacity
    /// decision rather than a bidding one.
    pub max_price: i32,
}

/// The machine type.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HardwareProfile {
    /// Provider-native machine type name.
    pub vm_size: String,
}

/// The image and the disk.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageProfile {
    /// Which image to boot.
    pub image_reference: ImageReference,
    /// The OS disk.
    pub os_disk: OsDisk,
    /// No data disks; the session's work lives on the OS disk.
    pub data_disks: Vec<ResourceRef>,
}

/// A marketplace image.
#[derive(Debug, Clone, Serialize)]
pub struct ImageReference {
    /// Publisher, e.g. `Canonical`.
    pub publisher: &'static str,
    /// Offer, e.g. `ubuntu-24_04-lts`.
    pub offer: &'static str,
    /// SKU. This is the field that has to follow the machine type's
    /// instruction set: `server` for x64, `server-arm64` for Arm64.
    pub sku: &'static str,
    /// `latest`, which ARM accepts even though the CLI does not.
    pub version: &'static str,
}

/// The OS disk.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OsDisk {
    /// Disk name, so a rebuilt machine can reattach it by name.
    pub name: String,
    /// `FromImage` on a first boot.
    pub create_option: &'static str,
    /// Host caching.
    pub caching: &'static str,
    /// Size in GiB.
    #[serde(rename = "diskSizeGB")]
    pub disk_size_gb: u32,
    /// `Detach`, so deleting the machine keeps the disk.
    pub delete_option: &'static str,
    /// Storage tier.
    pub managed_disk: ManagedDisk,
}

/// The OS disk's storage tier.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedDisk {
    /// e.g. `StandardSSD_LRS`.
    pub storage_account_type: &'static str,
}

/// The login and the first-boot document.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OsProfile {
    /// The machine's hostname.
    pub computer_name: String,
    /// The administrative login.
    pub admin_username: String,
    /// Base64 of the cloud-config. `customData` and not `userData`:
    /// cloud-init reads the former and merely exposes the latter on the
    /// metadata endpoint.
    pub custom_data: String,
    /// No VM-agent extension handshake, which is a provisioning step flyco
    /// has no use for.
    pub allow_extension_operations: bool,
    /// Public-key-only authentication.
    pub linux_configuration: LinuxConfiguration,
}

/// Linux-specific login settings.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinuxConfiguration {
    /// Always true: there is no password to disable, because none is set.
    pub disable_password_authentication: bool,
    /// The VM agent, which cloud-init and boot diagnostics rely on.
    pub provision_vm_agent: bool,
    /// The authorized keys.
    pub ssh: SshConfiguration,
}

/// The authorized keys.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SshConfiguration {
    /// One entry per key.
    pub public_keys: Vec<SshPublicKey>,
}

/// One authorized key.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SshPublicKey {
    /// Absolute path of the `authorized_keys` file it is written to.
    pub path: String,
    /// The key in OpenSSH format.
    pub key_data: String,
}

/// The interfaces a machine is reachable on.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkProfile {
    /// Exactly one.
    pub network_interfaces: Vec<AttachedInterface>,
}

/// One attached network interface.
#[derive(Debug, Clone, Serialize)]
pub struct AttachedInterface {
    /// The interface's id.
    pub id: String,
    /// Primary flag and delete option.
    pub properties: AttachedInterfaceProperties,
}

/// An attached interface's properties.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachedInterfaceProperties {
    /// Whether this is the primary interface.
    pub primary: bool,
    /// `Detach`, so deleting the machine keeps the interface.
    pub delete_option: &'static str,
}

/// Boot diagnostics.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsProfile {
    /// Serial console and screenshot capture.
    pub boot_diagnostics: BootDiagnostics,
}

/// The boot-diagnostics switch.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct BootDiagnostics {
    /// Always on: a machine that provisions and never phones home is
    /// otherwise unexaminable.
    pub enabled: bool,
}

// ── Resize ──

/// Body of the `PATCH` that changes a machine's type.
#[derive(Debug, Clone, Serialize)]
pub struct ResizePatch {
    /// The one property being changed.
    pub properties: ResizeProperties,
}

/// The hardware half of a resize.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResizeProperties {
    /// The new machine type.
    pub hardware_profile: HardwareProfile,
}
