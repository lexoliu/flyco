//! Compute Engine: its URLs, the bodies flyco sends, and the asynchronous
//! operation protocol every mutating call speaks.
//!
//! Bodies are serde structures rather than hand-built JSON, for the reason
//! `azure::bodies` gives: a renamed or dropped field fails the build instead
//! of producing a request Compute Engine accepts and reads differently.
//!
//! # Every mutating call is an operation
//!
//! Unlike EC2 — which hands back the instance's own state and leaves you to
//! read it until it settles — Compute Engine answers with an **operation
//! resource** and a `selfLink` to poll, which is Azure's shape rather than
//! AWS's. The terminal status is exactly `DONE`; `PENDING` and `RUNNING` are
//! in-flight, and so is anything else the service invents, which is why the
//! non-terminal case carries the raw string rather than being an enumerated
//! list that a new value would fall off the end of.
//!
//! A `DONE` operation is not a *successful* one. Failure is reported inside
//! the finished operation's `error`, and an `httpErrorStatusCode` beside it —
//! so an operation that reached `DONE` and carries an error is a failure, and
//! reading the status alone would call it a success.

use flyco_core::machine::{CpuArchitecture, MachineLineage};
use serde::{Deserialize, Serialize};

use crate::ProviderError;
use crate::http::HttpResponse;

/// The Compute Engine API's base URL.
pub const COMPUTE_BASE: &str = "https://compute.googleapis.com/compute/v1";

/// A project-scoped URL under the Compute Engine API.
#[must_use]
pub fn project_url(project: &str, path: &str) -> String {
    format!("{COMPUTE_BASE}/projects/{project}/{path}")
}

/// A zone-scoped URL under the Compute Engine API.
#[must_use]
pub fn zone_url(project: &str, zone: &str, path: &str) -> String {
    project_url(project, &format!("zones/{zone}/{path}"))
}

/// The region a zone belongs to.
///
/// A zone name is its region with a single-letter suffix (`us-central1-a` is
/// in `us-central1`), which is what makes a quota — published per region —
/// findable from the zone a machine is asked for. Derived rather than asked
/// for separately: a caller that had to supply both could supply a pair that
/// does not go together.
///
/// # Errors
///
/// Returns [`ProviderError::Malformed`] if the name is not a zone.
pub fn region_of(zone: &str) -> Result<String, ProviderError> {
    zone.rsplit_once('-')
        .filter(|(region, suffix)| !region.is_empty() && suffix.len() == 1)
        .map(|(region, _)| region.to_owned())
        .ok_or(ProviderError::Malformed(
            "this is not a Compute Engine zone name, so its region cannot be read",
        ))
}

// ── Errors ──

/// The error document the API returns on a rejection.
#[derive(Debug, Clone, Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

/// The inner half of a Google API error.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ErrorBody {
    /// Machine-readable status, e.g. `QUOTA_EXCEEDED`.
    #[serde(default)]
    pub status: String,
    /// Human-readable explanation.
    #[serde(default)]
    pub message: String,
    /// Per-problem detail, whose `reason` is the code a driver can act on.
    #[serde(default)]
    pub errors: Vec<ErrorDetail>,
}

/// One problem inside an error document.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ErrorDetail {
    /// e.g. `ZONE_RESOURCE_POOL_EXHAUSTED`.
    #[serde(default)]
    pub reason: String,
    /// What it says about this particular problem.
    #[serde(default)]
    pub message: String,
}

impl ErrorBody {
    /// Reads the error out of a response body, if it carries one.
    ///
    /// A rejection whose body is not a Google error document — a load
    /// balancer's HTML, an auth proxy's page — yields nothing, so the caller
    /// reports the status and the raw text rather than an empty code that
    /// reads like a real one.
    #[must_use]
    pub fn of(response: &HttpResponse) -> Option<Self> {
        response
            .json::<ErrorEnvelope>()
            .ok()
            .map(|envelope| envelope.error)
    }

    /// The code a driver can act on.
    ///
    /// The per-problem `reason` where there is one — that is where
    /// `ZONE_RESOURCE_POOL_EXHAUSTED` lives — and the envelope's `status`
    /// otherwise, which is what a permission or quota refusal states.
    #[must_use]
    pub fn code(&self) -> String {
        self.errors
            .first()
            .map(|detail| detail.reason.clone())
            .filter(|reason| !reason.is_empty())
            .unwrap_or_else(|| self.status.clone())
    }
}

/// Turns a refused response into an error that keeps its code.
#[must_use]
pub fn refusal(response: &HttpResponse) -> ProviderError {
    ErrorBody::of(response).map_or_else(
        || {
            ProviderError::Rejected(format!(
                "Compute Engine answered HTTP {}: {}",
                response.status,
                response.body_text()
            ))
        },
        |error| ProviderError::Refused {
            code: error.code(),
            message: error.message,
        },
    )
}

// ── Operations ──

/// Where an asynchronous operation has got to.
///
/// The terminal set is exactly `{DONE}`. Anything else — `PENDING`,
/// `RUNNING`, or a value the service invents tomorrow — means keep polling,
/// which is why the non-terminal case carries the raw string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationStatus {
    /// Finished, one way or another.
    Done,
    /// Anything else: still running.
    Running(String),
}

impl OperationStatus {
    /// Classifies a status string.
    #[must_use]
    pub fn parse(status: &str) -> Self {
        if status == "DONE" {
            Self::Done
        } else {
            Self::Running(status.to_owned())
        }
    }

    /// Whether the operation has finished.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Done)
    }
}

/// One operation resource.
#[derive(Debug, Clone, Deserialize)]
pub struct Operation {
    /// Its name, which is also how it is polled.
    #[serde(default)]
    pub name: String,
    /// `PENDING`, `RUNNING` or `DONE`.
    #[serde(default)]
    pub status: String,
    /// Where to poll it. Present on every operation the API returns.
    #[serde(rename = "selfLink", default)]
    pub self_link: Option<String>,
    /// The zone it belongs to, as a URL.
    #[serde(default)]
    pub zone: Option<String>,
    /// Present only on an operation that finished unsuccessfully.
    #[serde(default)]
    pub error: Option<OperationError>,
    /// HTTP status of the failure, when there was one.
    #[serde(rename = "httpErrorStatusCode", default)]
    pub http_error_status_code: Option<u16>,
    /// Message of the failure, when there was one.
    #[serde(rename = "httpErrorMessage", default)]
    pub http_error_message: Option<String>,
}

/// The errors a finished operation carries.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OperationError {
    /// One entry per problem.
    #[serde(default)]
    pub errors: Vec<OperationErrorDetail>,
}

/// One problem inside a failed operation.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OperationErrorDetail {
    /// e.g. `ZONE_RESOURCE_POOL_EXHAUSTED`.
    #[serde(default)]
    pub code: String,
    /// What it says.
    #[serde(default)]
    pub message: String,
}

impl Operation {
    /// This operation's status.
    #[must_use]
    pub fn state(&self) -> OperationStatus {
        OperationStatus::parse(&self.status)
    }

    /// The failure this operation finished with, if it failed.
    ///
    /// A `DONE` operation is not a successful one: failure is reported
    /// *inside* the finished operation, so reading the status alone would
    /// call a failed provision a success.
    #[must_use]
    pub fn failure(&self) -> Option<ProviderError> {
        let detail = self
            .error
            .as_ref()
            .and_then(|error| error.errors.first())
            .cloned();
        let http = self.http_error_status_code.filter(|status| *status >= 400);

        if detail.is_none() && http.is_none() {
            return None;
        }

        let detail = detail.unwrap_or_default();
        Some(ProviderError::OperationFailed {
            status: self.status.clone(),
            code: detail.code,
            message: if detail.message.is_empty() {
                self.http_error_message.clone().unwrap_or_default()
            } else {
                detail.message
            },
        })
    }
}

// ── Instances ──

/// Body of `instances.insert`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Instance {
    /// The instance's name, which is also its hostname.
    pub name: String,
    /// The machine type, as a full zonal URL.
    pub machine_type: String,
    /// Spot, or ordinary capacity — see [`Instance::without_spot`].
    pub scheduling: Scheduling,
    /// The boot disk, and nothing else.
    pub disks: Vec<AttachedDisk>,
    /// The one interface it is reachable on.
    pub network_interfaces: Vec<NetworkInterface>,
    /// The cloud-config, as a metadata item.
    pub metadata: Metadata,
    /// Ownership labels, so a project shared with other work stays legible.
    pub labels: std::collections::BTreeMap<String, String>,
}

impl Instance {
    /// The same machine, requested as ordinary on-demand capacity.
    ///
    /// Only the scheduling changes: everything else — the disk, the network,
    /// the metadata — is identical, and that is what makes the fallback a
    /// retry rather than a second, different machine.
    #[must_use]
    pub const fn without_spot(mut self) -> Self {
        self.scheduling = Scheduling::on_demand();
        self
    }

    /// Whether this body asks for spot capacity.
    #[must_use]
    pub fn is_spot(&self) -> bool {
        self.scheduling.provisioning_model == SPOT_MODEL
    }
}

/// The provisioning model of interruptible capacity.
pub const SPOT_MODEL: &str = "SPOT";

/// The provisioning model of ordinary capacity.
pub const STANDARD_MODEL: &str = "STANDARD";

/// How an instance is scheduled, and on which market.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Scheduling {
    /// `SPOT` or `STANDARD`.
    pub provisioning_model: &'static str,
    /// `TERMINATE` on spot, which the API requires and refuses to infer;
    /// `MIGRATE` on ordinary capacity, which is the default and is what
    /// keeps a machine alive through host maintenance.
    pub on_host_maintenance: &'static str,
    /// Whether the instance comes back by itself. Never on spot — the API
    /// refuses `true` there — and true otherwise, so a host failure does not
    /// end a session.
    pub automatic_restart: bool,
    /// Absent on ordinary capacity. `false` is what keeps a spot instance
    /// stopped rather than deleted when it is preempted, so its boot disk
    /// survives the interruption.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_termination_action: Option<&'static str>,
}

impl Scheduling {
    /// Interruptible capacity that stops rather than being deleted.
    #[must_use]
    pub const fn spot() -> Self {
        Self {
            provisioning_model: SPOT_MODEL,
            // Not a preference: a `SPOT` instance may not be live-migrated,
            // and the API refuses the insert outright if this says otherwise.
            on_host_maintenance: "TERMINATE",
            automatic_restart: false,
            // `STOP` rather than `DELETE`: a preempted machine that was
            // deleted takes the session's work with it.
            instance_termination_action: Some("STOP"),
        }
    }

    /// Ordinary capacity.
    #[must_use]
    pub const fn on_demand() -> Self {
        Self {
            provisioning_model: STANDARD_MODEL,
            on_host_maintenance: "MIGRATE",
            automatic_restart: true,
            instance_termination_action: None,
        }
    }
}

/// One disk attached to an instance.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachedDisk {
    /// Whether this is the disk the machine boots from.
    pub boot: bool,
    /// Always `false`, which is this driver's `Detach`: deleting the
    /// instance leaves the disk, and `destroy` is what deletes it.
    pub auto_delete: bool,
    /// How it comes into existence.
    pub initialize_params: DiskParams,
}

/// A boot disk created with the instance.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskParams {
    /// The disk's name, so a rebuilt machine can reattach it by name.
    pub disk_name: String,
    /// Size in GiB.
    pub disk_size_gb: u32,
    /// The image family to boot, as a URL.
    pub source_image: String,
    /// The storage tier, as a full zonal URL.
    pub disk_type: String,
}

/// One network interface.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkInterface {
    /// The VPC network, as a URL.
    pub network: String,
    /// Where its public address comes from. Without this the machine has no
    /// external address at all and answers nothing.
    pub access_configs: Vec<AccessConfig>,
}

/// An interface's external address.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessConfig {
    /// `ONE_TO_ONE_NAT`, the only type there is.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Its name, which the API requires.
    pub name: &'static str,
    /// `PREMIUM`, which is the only tier a spot instance may use in most
    /// regions and the only one that carries global routing.
    pub network_tier: &'static str,
}

/// An instance's metadata.
#[derive(Debug, Clone, Serialize)]
pub struct Metadata {
    /// One entry per key.
    pub items: Vec<MetadataItem>,
}

/// One metadata key and its value.
#[derive(Debug, Clone, Serialize)]
pub struct MetadataItem {
    /// The key. `user-data` is what the guest's cloud-init reads.
    pub key: &'static str,
    /// The value.
    pub value: String,
}

/// Body of `instances.setMachineType`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetMachineType {
    /// The new machine type, as a full zonal URL.
    pub machine_type: String,
}

/// What `instances.get` answers, in the parts flyco reads.
#[derive(Debug, Clone, Deserialize)]
pub struct InstanceStatus {
    /// The instance's name.
    #[serde(default)]
    pub name: String,
    /// `PROVISIONING`, `STAGING`, `RUNNING`, `STOPPING`, `TERMINATED`, …
    #[serde(default)]
    pub status: String,
    /// Its interfaces, which is where the external address is.
    #[serde(rename = "networkInterfaces", default)]
    pub network_interfaces: Vec<InstanceInterface>,
}

/// One interface on a running instance.
#[derive(Debug, Clone, Deserialize)]
pub struct InstanceInterface {
    /// Its external addresses.
    #[serde(rename = "accessConfigs", default)]
    pub access_configs: Vec<InstanceAccessConfig>,
}

/// One external address on a running instance.
#[derive(Debug, Clone, Deserialize)]
pub struct InstanceAccessConfig {
    /// The address itself, once one is assigned.
    #[serde(rename = "natIP", default)]
    pub nat_ip: Option<String>,
}

impl InstanceStatus {
    /// The address the daemon bootstrap reaches this machine on.
    #[must_use]
    pub fn address(&self) -> Option<String> {
        self.network_interfaces
            .iter()
            .flat_map(|interface| &interface.access_configs)
            .find_map(|config| config.nat_ip.clone())
    }
}

// ── The catalog's reads ──

/// One page of `machineTypes.list`.
#[derive(Debug, Clone, Deserialize)]
pub struct MachineTypePage {
    /// The types on this page.
    #[serde(default)]
    pub items: Vec<MachineType>,
    /// The next page, when the list is longer than one.
    #[serde(rename = "nextPageToken", default)]
    pub next_page_token: Option<String>,
}

/// One machine type, as Compute Engine reports it.
#[derive(Debug, Clone, Deserialize)]
pub struct MachineType {
    /// e.g. `e2-standard-2`.
    pub name: String,
    /// Virtual CPU count.
    #[serde(rename = "guestCpus", default)]
    pub guest_cpus: u32,
    /// Memory in MiB, which is what the API states it in.
    #[serde(rename = "memoryMb", default)]
    pub memory_mb: u64,
    /// The instruction set, as Compute Engine states it: `X86_64`, `ARM64`,
    /// or `ARCHITECTURE_UNSPECIFIED`.
    ///
    /// Read rather than parsed out of the name, for the reason every other
    /// driver reads it: `t2a` is Arm and `t2d` is x86, and pairing either
    /// with the other's image fails at launch.
    #[serde(default)]
    pub architecture: Option<String>,
    /// Present only on a type that is on its way out.
    #[serde(default)]
    pub deprecated: Option<Deprecation>,
}

/// A machine type's deprecation notice.
#[derive(Debug, Clone, Deserialize)]
pub struct Deprecation {
    /// `DEPRECATED`, `OBSOLETE` or `DELETED`.
    #[serde(default)]
    pub state: String,
}

impl MachineType {
    /// Whether a session can still be started on this type.
    ///
    /// `DEPRECATED` still works and is offered; `OBSOLETE` and `DELETED` are
    /// refused by the API, so offering one would put a machine on the menu
    /// that cannot be created.
    #[must_use]
    pub fn is_offerable(&self) -> bool {
        self.deprecated
            .as_ref()
            .is_none_or(|deprecation| !matches!(deprecation.state.as_str(), "OBSOLETE" | "DELETED"))
    }

    /// The machine family this type belongs to: `e2-standard-2` is `e2`.
    ///
    /// The family is what the price list publishes a rate for — there is no
    /// per-machine-type price — so this is how a type is priced at all.
    #[must_use]
    pub fn family(&self) -> Option<&str> {
        self.name.split_once('-').map(|(family, _)| family)
    }

    /// The instruction set this type runs.
    ///
    /// `None` for `ARCHITECTURE_UNSPECIFIED` and for a type the API
    /// described without the field: both mean Compute Engine did not say,
    /// and a boot image cannot be chosen for a machine whose instruction set
    /// nobody stated.
    #[must_use]
    pub fn cpu_architecture(&self) -> Option<CpuArchitecture> {
        match self.architecture.as_deref()? {
            "X86_64" => Some(CpuArchitecture::X8664),
            "ARM64" => Some(CpuArchitecture::Arm64),
            _ => None,
        }
    }

    /// Where this type sits in Compute Engine's line-up.
    ///
    /// A name is `<series>-<shape>[-<size>]`: `e2-standard-2`,
    /// `c3d-highmem-4`, `f1-micro`, `e2-custom-4-8192`. The series carries
    /// the generation (`e2` is the second `e`), and the shape is what makes
    /// two types of one series different machines rather than two sizes of
    /// one — so the family key is the series' letters plus the shape, and
    /// the purely numeric segments, which are the size, are dropped.
    #[must_use]
    pub fn lineage(&self) -> Option<MachineLineage> {
        let (series, rest) = self.name.split_once('-')?;
        let parsed = crate::naming::series(series)?;
        let shape: Vec<&str> = rest
            .split('-')
            .filter(|segment| !segment.chars().all(|c| c.is_ascii_digit()))
            .collect();
        if shape.is_empty() {
            return None;
        }

        Some(MachineLineage {
            architecture: self.cpu_architecture()?,
            family: format!("{}-{}", parsed.family, shape.join("-")),
            generation: parsed.generation,
        })
    }
}

/// What `regions.get` answers, in the part flyco reads.
#[derive(Debug, Clone, Deserialize)]
pub struct RegionInfo {
    /// The region's name.
    #[serde(default)]
    pub name: String,
    /// `UP` or `DOWN`.
    #[serde(default)]
    pub status: String,
    /// Its quotas, which state both the limit and what is used — unlike
    /// AWS's, and like Azure's.
    #[serde(default)]
    pub quotas: Vec<Quota>,
    /// The zones it holds, as URLs.
    #[serde(default)]
    pub zones: Vec<String>,
}

/// One project quota in one region.
#[derive(Debug, Clone, Deserialize)]
pub struct Quota {
    /// e.g. `CPUS` or `PREEMPTIBLE_CPUS`.
    #[serde(default)]
    pub metric: String,
    /// How much is already in use.
    #[serde(default)]
    pub usage: f64,
    /// The limit.
    #[serde(default)]
    pub limit: f64,
}

#[cfg(test)]
mod tests {
    use super::{
        CpuArchitecture, ErrorBody, MachineType, MachineTypePage, Operation, OperationStatus,
        region_of, zone_url,
    };
    use crate::http::HttpResponse;

    fn operation(fixture: &str) -> Operation {
        serde_json::from_str(fixture).expect("the operation fixture parses")
    }

    #[test]
    fn a_zonal_url_names_the_project_and_the_zone() {
        assert_eq!(
            zone_url("flyco-sessions", "us-central1-a", "instances"),
            "https://compute.googleapis.com/compute/v1/projects/flyco-sessions\
             /zones/us-central1-a/instances"
        );
    }

    #[test]
    fn a_zones_region_is_its_name_without_the_suffix() {
        assert_eq!(region_of("us-central1-a").expect("a region"), "us-central1");
        assert_eq!(
            region_of("europe-west4-b").expect("a region"),
            "europe-west4"
        );
        // A region name is not a zone, and treating it as one would read
        // `us-central` out of `us-central1`.
        region_of("us-central1").expect_err("a region is not a zone");
        region_of("").expect_err("neither is nothing");
    }

    #[test]
    fn only_done_is_terminal() {
        assert!(OperationStatus::parse("DONE").is_terminal());
        // A value the service invents tomorrow means keep polling, never
        // "finished".
        for running in ["PENDING", "RUNNING", "SCHEDULING"] {
            assert!(
                !OperationStatus::parse(running).is_terminal(),
                "`{running}` must not be read as terminal"
            );
        }
    }

    #[test]
    fn a_done_operation_that_carries_an_error_is_a_failure() {
        // The trap: the status says the operation finished, and the failure
        // is inside it. Reading the status alone would call this a success.
        let failed = operation(include_str!("../../fixtures/gcp/operation_failed.json"));
        assert!(failed.state().is_terminal());

        let failure = failed.failure().expect("a finished failure is a failure");
        assert_eq!(failure.code(), Some("ZONE_RESOURCE_POOL_EXHAUSTED"));
    }

    #[test]
    fn a_done_operation_with_no_error_is_a_success() {
        let done = operation(include_str!("../../fixtures/gcp/operation_done.json"));
        assert!(done.state().is_terminal());
        assert!(done.failure().is_none());
    }

    #[test]
    fn an_in_flight_operation_names_where_to_poll_it() {
        let running = operation(include_str!("../../fixtures/gcp/operation_running.json"));
        assert!(!running.state().is_terminal());
        assert_eq!(
            running.self_link.as_deref(),
            Some(
                "https://compute.googleapis.com/compute/v1/projects/flyco-sessions\
                 /zones/us-central1-a/operations/operation-1788004800000-flyco"
            )
        );
    }

    #[test]
    fn a_google_error_document_yields_the_reason_a_driver_can_act_on() {
        let response = HttpResponse::new(
            400,
            include_bytes!("../../fixtures/gcp/error_zone_exhausted.json").to_vec(),
        );
        let error = ErrorBody::of(&response).expect("a Google error document");
        assert_eq!(error.code(), "ZONE_RESOURCE_POOL_EXHAUSTED");
    }

    #[test]
    fn a_rejection_that_is_not_a_google_document_yields_no_code() {
        assert!(ErrorBody::of(&HttpResponse::new(502, b"<html>gateway</html>".to_vec())).is_none());
    }

    #[test]
    fn a_machine_type_reports_its_family_and_whether_it_can_still_be_used() {
        let page: MachineTypePage =
            serde_json::from_str(include_str!("../../fixtures/gcp/machine_types.json"))
                .expect("the machine-type fixture parses");
        let of = |name: &str| {
            page.items
                .iter()
                .find(|entry| entry.name == name)
                .unwrap_or_else(|| panic!("the fixture holds `{name}`"))
        };

        let standard = of("e2-standard-2");
        assert_eq!(standard.family(), Some("e2"));
        assert_eq!(standard.guest_cpus, 2);
        assert_eq!(standard.memory_mb, 8_192);
        assert!(standard.is_offerable());

        // Deprecated still works and is offered; obsolete is refused by the
        // API, so offering it would put an uncreatable machine on the menu.
        assert!(of("n1-standard-1").is_offerable());
        assert!(!of("f1-micro").is_offerable());
    }

    /// A machine type built by hand, for names the fixture has no example
    /// of. The lineage reads only the name and the architecture.
    fn named(name: &str, architecture: &str) -> MachineType {
        MachineType {
            name: name.to_owned(),
            guest_cpus: 4,
            memory_mb: 16_384,
            architecture: Some(architecture.to_owned()),
            deprecated: None,
        }
    }

    #[test]
    fn a_lineage_is_the_series_letters_plus_the_shape() {
        let lineage = |name: &str| named(name, "X86_64").lineage().expect("a lineage");

        assert_eq!(lineage("e2-standard-2").family, "e-standard");
        assert_eq!(lineage("e2-standard-2").generation, Some(2));
        // The size is dropped and the shape is kept: `n2-standard` and
        // `n2-highmem` are different machines, `n2-standard-2` and
        // `n2-standard-16` are two sizes of one.
        assert_eq!(lineage("n2-standard-16").family, "n-standard");
        assert_eq!(lineage("n2-highmem-16").family, "n-highmem");
        // The series qualifier names the silicon and stays in the key.
        assert_eq!(lineage("n2d-standard-2").family, "nd-standard");
        assert_eq!(lineage("c3d-highmem-4").family, "cd-highmem");
        // A shape with no size at all, and a custom shape with two.
        assert_eq!(lineage("f1-micro").family, "f-micro");
        assert_eq!(lineage("e2-custom-4-8192").family, "e-custom");
    }

    #[test]
    fn generations_of_one_shape_share_a_family_key() {
        let n1 = named("n1-standard-4", "X86_64").lineage().expect("lineage");
        let n2 = named("n2-standard-4", "X86_64").lineage().expect("lineage");
        assert_eq!(n1.family, n2.family);
        assert_eq!((n1.generation, n2.generation), (Some(1), Some(2)));
    }

    #[test]
    fn the_architecture_is_read_and_never_guessed_from_the_name() {
        assert_eq!(
            named("t2a-standard-4", "ARM64").cpu_architecture(),
            Some(CpuArchitecture::Arm64)
        );
        assert_eq!(
            named("t2d-standard-4", "X86_64").cpu_architecture(),
            Some(CpuArchitecture::X8664)
        );
    }

    #[test]
    fn a_type_compute_engine_did_not_place_has_no_lineage() {
        // `ARCHITECTURE_UNSPECIFIED` and a missing field are the same claim:
        // Google did not say, so flyco does not either.
        assert!(
            named("e2-standard-2", "ARCHITECTURE_UNSPECIFIED")
                .lineage()
                .is_none()
        );
        assert!(
            MachineType {
                architecture: None,
                ..named("e2-standard-2", "X86_64")
            }
            .lineage()
            .is_none()
        );
        // A name with no shape at all is not a machine type name.
        assert!(named("e2", "X86_64").lineage().is_none());
    }
}
