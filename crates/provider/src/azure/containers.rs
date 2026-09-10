//! Azure Container Apps: the half of the driver that runs a session as a
//! managed container instead of a virtual machine.
//!
//! A session on [`Runtime::Container`](flyco_core::Runtime::Container) is a
//! Container Apps **job** with a manual trigger, and one *execution* of that
//! job is one running machine. The job is the definition — it survives a
//! stop, which is what makes a start a start — and the execution is the
//! compute, which does not: its filesystem ends with it, so what makes the
//! session resumable is the `workdir-patch` its daemon writes on `SIGTERM`.
//!
//! Everything Container Apps needs lives here rather than beside the
//! virtual-machine bodies in [`super::bodies`], because it is a different
//! service: its own resource provider, its own `api-version`, its own price
//! meters, and a lifecycle that has no deallocate in it. What it shares with
//! the rest of the driver is the ARM client itself — the same token, the
//! same asynchronous-operation protocol, the same resource group.
//!
//! # Consumption, and nothing else
//!
//! The environment declares exactly one workload profile,
//! [`WORKLOAD_PROFILE`]. Consumption is serverless — nothing is billed
//! between executions, and the free grant
//! ([`super::CONTAINER_APPS_FREE_GRANT`]) applies to it — while every other
//! profile is a pool of reserved instances billed by the hour whether a
//! session is running or not. A dedicated profile would bill a user for the
//! twenty-three hours a day their agent is idle.
//!
//! # The size table is flyco's, because Azure publishes none
//!
//! Container Apps sells fractional CPU and memory rather than named machine
//! types: any `cpu` from 0.25 to 4 in quarter-core steps, with memory fixed
//! at twice that many GiB. There is no SKU list to read, so the catalog
//! offers a short table of whole-core sizes ([`Size::OFFERED`]) named
//! `aca-<vcpus>x<gib>`. That name is what a resize and a provisioning
//! request carry, and [`Size::named`] is the only way back from it: a name
//! outside the table names a size flyco does not offer, not a size to
//! attempt.

use core::fmt;

use flyco_core::machine::{CpuArchitecture, MachineLineage};
use flyco_core::{MachineId, release};
use serde::{Deserialize, Serialize};

use super::bodies::MachineTags;
use crate::{DaemonBootstrap, ProviderError};

/// The workload profile every flyco environment and job is pinned to.
///
/// Serverless: an execution is billed for the seconds it runs and nothing
/// is billed between them. Its name is also its type — Azure lets an
/// environment name its profiles, and calling the Consumption profile
/// anything else would be a second thing to remember.
pub const WORKLOAD_PROFILE: &str = "Consumption";

/// Where an environment sends the logs its containers write.
///
/// `azure-monitor` rather than `log-analytics` because it is the only
/// destination that needs no Log Analytics workspace: the `log-analytics`
/// destination's body carries a workspace's customer id *and its shared
/// key*, which flyco would have to create a workspace to obtain and then
/// hold a second credential for. `azure-monitor` sends the logs to whatever
/// diagnostic setting the subscription's owner has attached, which is their
/// decision to make and costs nothing when they have made none. `none`
/// would throw a failing session's output away.
pub const LOG_DESTINATION: &str = "azure-monitor";

/// The trigger type a flyco job carries.
///
/// Manual: flyco starts an execution when a session needs a machine. The
/// alternatives are a cron schedule and an event source, and a session is
/// neither.
pub const TRIGGER_TYPE: &str = "Manual";

/// Longest one execution may run, in seconds.
///
/// A job **must** state a finite `replicaTimeout`, so this is seven days
/// rather than a judgement about how long a session should last: the
/// session's own budget, its idle timeout and the user stop it long before,
/// and every one of those is a decision flyco makes with information this
/// number does not have. What it is really guarding is the case where all
/// of them fail at once — a control plane that lost the machine — and a week
/// is short enough that such an execution stops being billed on its own.
pub const REPLICA_TIMEOUT_SECONDS: u32 = 7 * 24 * 60 * 60;

/// How many times a failed replica is retried.
///
/// None. A replica that exited is a daemon that stopped, and starting a
/// second one against the same session would put two daemons on one session
/// id — both holding the wire, both writing the same workdir. The control
/// plane decides whether a session gets another machine.
pub const REPLICA_RETRY_LIMIT: u32 = 0;

/// The single container in a job's template.
pub const CONTAINER_NAME: &str = "session";

/// Name of the job secret holding the `flycod` configuration.
///
/// The configuration carries the session's daemon token, the user's GitHub
/// token and the harness credential, so it travels as a *secret* rather
/// than as an environment value: a plain `env` entry is readable by anyone
/// with reader access to the job, and is echoed back by every `GET` of it.
pub const CONFIG_SECRET: &str = "daemon-config";

/// Prefix of every machine type this module offers.
pub const MACHINE_TYPE_PREFIX: &str = "aca-";

/// GiB of memory Container Apps pairs with each vCPU.
///
/// Fixed by the platform rather than chosen: a Consumption replica's memory
/// is exactly twice its cores in GiB, and any other pair is rejected.
pub const MEMORY_GIB_PER_VCPU: u32 = 2;

/// The resource provider every Container Apps type lives under.
///
/// Named because a subscription has to be *registered* for it before the
/// first environment can be created, and a freshly linked subscription is
/// not: see [`AzureProvider::ensure_container_provider`].
///
/// [`AzureProvider::ensure_container_provider`]: super::AzureProvider::ensure_container_provider
pub const PROVIDER_NAMESPACE: &str = "Microsoft.App";

/// The provider-native family key every container entry carries.
///
/// One family, because Azure sells one: the sizes below are points on a
/// single continuous Consumption dial rather than generations of anything.
/// The catalog needs a key it can group by, and this is the honest one.
pub const FAMILY: &str = "container-apps-consumption";

/// The instruction set a Container Apps replica runs.
///
/// x86-64, and not because flyco chose it: Consumption offers no Arm
/// capacity, so the session image is pulled as `linux/amd64` there whatever
/// the account would prefer.
pub const ARCHITECTURE: CpuArchitecture = CpuArchitecture::X8664;

/// One size flyco offers on Container Apps.
///
/// A single field, because memory is not independently choosable: the
/// platform pairs [`MEMORY_GIB_PER_VCPU`] GiB with every core, so a struct
/// with two fields could hold a pair Azure would refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    /// Whole virtual CPUs.
    vcpus: u32,
}

impl Size {
    /// The sizes the catalog offers, smallest first.
    ///
    /// Whole cores only, and only up to the Consumption ceiling of four.
    /// The quarter-core sizes below one exist on the platform and are left
    /// off: `AUTO_MIN_VCPUS` already says a coding agent under four cores is
    /// the wrong trade, and a fraction of a core is a machine that cannot
    /// finish a build at all.
    pub const OFFERED: [Self; 3] = [Self { vcpus: 1 }, Self { vcpus: 2 }, Self { vcpus: 4 }];

    /// How many whole cores this size holds.
    #[must_use]
    pub const fn vcpus(self) -> u32 {
        self.vcpus
    }

    /// How much memory it holds, in GiB.
    #[must_use]
    pub const fn memory_gib(self) -> u32 {
        self.vcpus * MEMORY_GIB_PER_VCPU
    }

    /// How much memory it holds, in MiB, as the catalog states capacity.
    #[must_use]
    pub const fn memory_mib(self) -> u64 {
        self.memory_gib() as u64 * 1024
    }

    /// The machine type name this size is offered and asked for under.
    #[must_use]
    pub fn machine_type(self) -> String {
        format!("{MACHINE_TYPE_PREFIX}{}x{}", self.vcpus, self.memory_gib())
    }

    /// The size a machine type name refers to, if flyco offers one.
    ///
    /// A lookup through [`OFFERED`](Self::OFFERED) rather than a parser, so
    /// there is one table and a name that round-trips through it is exactly
    /// a name the catalog published.
    #[must_use]
    pub fn named(machine_type: &str) -> Option<Self> {
        Self::OFFERED
            .into_iter()
            .find(|size| size.machine_type() == machine_type)
    }

    /// The `cpu` field of a container's resource requirements.
    ///
    /// Container Apps quotes cores as a decimal because quarter cores are
    /// buyable; a whole-core size is that decimal with nothing after the
    /// point.
    #[must_use]
    pub fn cpu_cores(self) -> f64 {
        f64::from(self.vcpus)
    }

    /// The `memory` field of a container's resource requirements.
    #[must_use]
    pub fn memory(self) -> String {
        format!("{}Gi", self.memory_gib())
    }

    /// Where this size sits in the provider's line-up.
    #[must_use]
    pub fn lineage(self) -> MachineLineage {
        MachineLineage {
            architecture: ARCHITECTURE,
            family: FAMILY.to_owned(),
            // Azure numbers no generations of the Consumption profile, and
            // an invented one would hide every size but the highest.
            generation: None,
        }
    }
}

/// The names Container Apps resources answer to.
pub mod names {
    use flyco_core::MachineId;

    /// The Container Apps environment one region's jobs run in.
    ///
    /// One per region, beside the virtual network and security group the
    /// virtual machines share, and for the same reason: an environment's
    /// location is fixed at creation, so a single name would pin every
    /// region's jobs to whichever region asked first.
    #[must_use]
    pub fn environment(region: &str) -> String {
        format!("flyco-{region}-env")
    }

    /// The most characters Azure allows in a Container Apps job name.
    ///
    /// A virtual machine takes the whole hyphenated machine id after the
    /// `flyco-` prefix; a job may not (`ContainerAppInvalidName`, issue
    /// #250), so the job carries as much of the id's hex form as fits.
    pub const JOB_NAME_MAX: usize = 32;

    /// A machine's job.
    ///
    /// Derived from the machine id rather than allocated, so a job is
    /// recoverable from the machines table without a column to store it
    /// in: `flyco-` and as many leading hex characters of the id as
    /// [`JOB_NAME_MAX`] leaves room for — 104 bits of a random uuid, no likelier to collide
    /// than any two machine ids. Named separately from
    /// [`super::super::names::machine`] because the two differ — a job is
    /// a different resource provider, with a length rule a VM does not
    /// have — and a caller should not have to know how.
    #[must_use]
    pub fn job(id: MachineId) -> String {
        const PREFIX: &str = "flyco-";
        let hex = id.as_uuid().simple().to_string();
        format!("{PREFIX}{}", &hex[..JOB_NAME_MAX - PREFIX.len()])
    }
}

/// A running execution of one job: what a container machine's `native_id`
/// records.
///
/// Both halves are needed and neither is derivable from the other: the job
/// is the definition every later call is addressed to, and the execution is
/// the one running replica, which Azure names — it is not flyco's to choose
/// and it changes with every start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Execution<'a> {
    /// The job this is an execution of.
    pub job: &'a str,
    /// Azure's name for the execution.
    pub name: &'a str,
}

/// Separates the two halves of a container machine's `native_id`.
///
/// A slash, which neither half may contain: a job name is
/// `^[-\w\._\(\)]+$` and an execution name is that job name plus a suffix
/// Azure generates.
const NATIVE_ID_SEPARATOR: char = '/';

impl<'a> Execution<'a> {
    /// The `native_id` a machine records this execution as.
    #[must_use]
    pub fn native_id(job: &str, name: &str) -> String {
        format!("{job}{NATIVE_ID_SEPARATOR}{name}")
    }

    /// Reads back what [`native_id`](Self::native_id) wrote.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Malformed`] for a `native_id` that names no
    /// execution, which is a row written by something other than this
    /// driver rather than a state to recover from.
    pub fn parse(native_id: &'a str) -> Result<Self, ProviderError> {
        let (job, name) =
            native_id
                .split_once(NATIVE_ID_SEPARATOR)
                .ok_or(ProviderError::Malformed(
                    "this machine's provider-native id names no Container Apps execution",
                ))?;
        if job.is_empty() || name.is_empty() {
            return Err(ProviderError::Malformed(
                "this machine's provider-native id names no Container Apps execution",
            ));
        }
        Ok(Self { job, name })
    }
}

// ── Managed environment ──

/// Body of a region's managed-environment `PUT`.
#[derive(Debug, Clone, Serialize)]
pub struct ManagedEnvironment {
    /// Region. Fixed at creation.
    pub location: String,
    /// Its profiles and its logging.
    pub properties: ManagedEnvironmentProperties,
}

/// What `GET …/managedEnvironments/{name}` answers, trimmed to the one
/// property the driver decides on.
#[derive(Debug, Clone, Deserialize)]
pub struct ManagedEnvironmentRecord {
    /// Its state, among other things.
    pub properties: ManagedEnvironmentRecordProperties,
}

/// The properties of an environment ARM answers with.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedEnvironmentRecordProperties {
    /// `Succeeded`, `Failed`, `Canceled`, `Waiting`,
    /// `InitializationInProgress`, `InfrastructureSetupInProgress`,
    /// `InfrastructureSetupComplete`, `ScheduledForDelete`,
    /// `UpgradeRequested` or `UpgradeFailed`.
    pub provisioning_state: String,
}

/// Where an environment is in its life, as far as a job creation cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentState {
    /// Jobs can be created in it.
    Ready,
    /// Azure is still building or upgrading it. A `PUT` now is refused
    /// with `ManagedEnvironmentOperationInProgress` — which is what the
    /// second attempt at the first container session on dev hit, when the
    /// first attempt's `PUT` was still being carried out — so the driver
    /// waits instead of writing.
    InProgress(String),
    /// The last operation on it failed, or it is being deleted; a fresh
    /// `PUT` is the way forward.
    Failed(String),
}

impl ManagedEnvironmentRecord {
    /// Reads the state the way the driver acts on it.
    #[must_use]
    pub fn state(&self) -> EnvironmentState {
        let state = self.properties.provisioning_state.as_str();
        match state {
            "Succeeded" | "InfrastructureSetupComplete" => EnvironmentState::Ready,
            "Failed" | "Canceled" | "UpgradeFailed" | "ScheduledForDelete" => {
                EnvironmentState::Failed(state.to_owned())
            }
            _ => EnvironmentState::InProgress(state.to_owned()),
        }
    }
}

/// A managed environment's properties.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedEnvironmentProperties {
    /// The profiles jobs in this environment may be pinned to.
    pub workload_profiles: Vec<WorkloadProfile>,
    /// Where the containers' logs go.
    pub app_logs_configuration: AppLogsConfiguration,
}

/// One workload profile an environment offers.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkloadProfile {
    /// What jobs refer to it as.
    pub name: &'static str,
    /// Which profile it is. `Consumption` takes no instance counts: it is
    /// serverless, so there is nothing to keep warm.
    pub workload_profile_type: &'static str,
}

/// Where an environment's application logs are sent.
#[derive(Debug, Clone, Serialize)]
pub struct AppLogsConfiguration {
    /// `log-analytics`, `azure-monitor` or `none` — see
    /// [`LOG_DESTINATION`].
    pub destination: &'static str,
}

// ── Job ──

/// Body of a machine's job `PUT`.
#[derive(Debug, Clone, Serialize)]
pub struct Job {
    /// Region. Must match the environment's.
    pub location: String,
    /// Ownership tags, so a resource group shared with other work stays
    /// legible — the same three a virtual machine carries.
    pub tags: MachineTags,
    /// Everything else.
    pub properties: JobProperties,
}

/// A job's properties.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobProperties {
    /// Full ARM id of the environment it runs in.
    pub environment_id: String,
    /// Which of that environment's profiles its executions run on.
    pub workload_profile_name: &'static str,
    /// Trigger, timeout, retries and secrets.
    pub configuration: JobConfiguration,
    /// The container to run.
    pub template: JobTemplate,
}

/// The non-versioned half of a job.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobConfiguration {
    /// Always [`TRIGGER_TYPE`].
    pub trigger_type: &'static str,
    /// Longest one replica may run — see [`REPLICA_TIMEOUT_SECONDS`].
    pub replica_timeout: u32,
    /// How many times a failed replica is retried — see
    /// [`REPLICA_RETRY_LIMIT`].
    pub replica_retry_limit: u32,
    /// How many replicas one manual start runs.
    pub manual_trigger_config: ManualTriggerConfig,
    /// The secrets the container's environment reads from.
    pub secrets: Vec<Secret>,
}

/// What one manual start runs.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManualTriggerConfig {
    /// Replicas started at once. One: a session is one machine.
    pub parallelism: u32,
    /// Replicas that must finish for the execution to have succeeded, which
    /// is the same one.
    pub replica_completion_count: u32,
}

/// One secret a job's containers may read.
///
/// [`fmt::Debug`] is written rather than derived, and prints the name only:
/// the value is the session's whole `flycod` configuration, which carries
/// the daemon token, the user's GitHub token and the harness credential.
#[derive(Clone, Serialize)]
pub struct Secret {
    /// What an environment variable refers to it as.
    pub name: &'static str,
    /// The secret itself.
    pub value: String,
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Secret")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// The versioned half of a job: what actually runs.
#[derive(Debug, Clone, Serialize)]
pub struct JobTemplate {
    /// Exactly one container. A session is one daemon supervising one
    /// harness, and a sidecar would be a second thing to keep alive inside
    /// a replica that dies as a unit.
    pub containers: Vec<Container>,
}

/// The container a job runs.
#[derive(Debug, Clone, Serialize)]
pub struct Container {
    /// Container name inside the replica.
    pub name: &'static str,
    /// Image tag — see
    /// [`release::session_image_for_wire_protocol`].
    pub image: String,
    /// How big the replica is.
    pub resources: ContainerResources,
    /// Its environment.
    pub env: Vec<EnvironmentVar>,
}

/// One replica's size.
///
/// Built only by [`template`], out of a [`Size`]: the platform rejects any
/// memory that is not [`MEMORY_GIB_PER_VCPU`] GiB per core, and going
/// through the size table is what keeps the two fields agreeing.
#[derive(Debug, Clone, Serialize)]
pub struct ContainerResources {
    /// Cores, as a decimal.
    pub cpu: f64,
    /// Memory, e.g. `2Gi`.
    pub memory: String,
}

/// One environment variable of a container.
///
/// Only the secret form is modelled: everything a session's daemon needs is
/// in its configuration, and the configuration is a secret.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentVar {
    /// Variable name.
    pub name: &'static str,
    /// Which of the job's secrets holds its value.
    pub secret_ref: &'static str,
}

// ── Resize ──

/// Body of the `PATCH` that moves a job to another size.
///
/// A `PATCH` rather than a second `PUT`, because a `PUT` is a full replace
/// and the driver does not hold the session's `flycod` configuration at
/// resize time: re-sending the job without
/// [`JobConfiguration::secrets`] would delete the secret the container
/// reads its configuration from, and the next execution would come up with
/// no way to reach the control plane.
#[derive(Debug, Clone, Serialize)]
pub struct JobPatch {
    /// The one thing that changes.
    pub properties: JobPatchProperties,
}

/// The properties a resize touches.
#[derive(Debug, Clone, Serialize)]
pub struct JobPatchProperties {
    /// The whole container, restated at the new size.
    ///
    /// Whole rather than just its `resources`, because ARM replaces the
    /// containers array rather than merging into its elements — a patch
    /// naming only the size would leave a container with no image.
    pub template: JobTemplate,
}

// ── Responses ──

/// What `POST .../jobs/{job}/start` answers with.
#[derive(Debug, Clone, Deserialize)]
pub struct StartedExecution {
    /// Azure's name for the execution that was started.
    #[serde(default)]
    pub name: String,
}

/// The job a machine's provisioning request describes.
///
/// # Errors
///
/// Returns [`ProviderError::Malformed`] if the session's configuration does
/// not render.
pub fn job_body(
    machine: MachineId,
    bootstrap: &DaemonBootstrap,
    region: &str,
    environment_id: String,
    size: Size,
) -> Result<Job, ProviderError> {
    Ok(Job {
        location: region.to_owned(),
        tags: MachineTags {
            owner: "flyco",
            session: bootstrap.session.to_string(),
            machine: machine.to_string(),
        },
        properties: JobProperties {
            environment_id,
            workload_profile_name: WORKLOAD_PROFILE,
            configuration: JobConfiguration {
                trigger_type: TRIGGER_TYPE,
                replica_timeout: REPLICA_TIMEOUT_SECONDS,
                replica_retry_limit: REPLICA_RETRY_LIMIT,
                manual_trigger_config: ManualTriggerConfig {
                    parallelism: 1,
                    replica_completion_count: 1,
                },
                secrets: vec![Secret {
                    name: CONFIG_SECRET,
                    value: crate::host::encoded_config(bootstrap)?,
                }],
            },
            template: template(size),
        },
    })
}

/// The one container a job of the given size runs.
///
/// Shared by the create and the resize so the two cannot describe different
/// containers: a resize that dropped the environment entry would start a
/// replica with no configuration to read.
#[must_use]
pub fn template(size: Size) -> JobTemplate {
    JobTemplate {
        containers: vec![Container {
            name: CONTAINER_NAME,
            image: release::session_image_for_wire_protocol(),
            resources: ContainerResources {
                cpu: size.cpu_cores(),
                memory: size.memory(),
            },
            env: vec![EnvironmentVar {
                name: crate::host::CONFIG_ENV,
                secret_ref: CONFIG_SECRET,
            }],
        }],
    }
}

/// The environment every region's jobs run in.
#[must_use]
pub fn environment_body(region: &str) -> ManagedEnvironment {
    ManagedEnvironment {
        location: region.to_owned(),
        properties: ManagedEnvironmentProperties {
            workload_profiles: vec![WorkloadProfile {
                name: WORKLOAD_PROFILE,
                workload_profile_type: WORKLOAD_PROFILE,
            }],
            app_logs_configuration: AppLogsConfiguration {
                destination: LOG_DESTINATION,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{Execution, MACHINE_TYPE_PREFIX, Size, names, template};
    use flyco_core::{MachineId, release};

    #[test]
    fn every_offered_size_pairs_two_gibibytes_with_each_core() {
        for size in Size::OFFERED {
            assert_eq!(size.memory_gib(), size.vcpus() * 2);
            assert_eq!(size.memory_mib(), u64::from(size.memory_gib()) * 1024);
        }
    }

    #[test]
    fn a_job_name_fits_the_limit_azure_puts_on_it() {
        // 6 + 26 of the id's 32 hex characters, and nothing Azure refuses:
        // lower-case alphanumerics and single hyphens, starting with a
        // letter and ending with an alphanumeric.
        let id = MachineId::generate();
        let job = names::job(id);
        assert_eq!(job.len(), names::JOB_NAME_MAX);
        assert!(job.starts_with("flyco-"));
        assert!(job.ends_with(|c: char| c.is_ascii_alphanumeric()));
        assert!(!job.contains("--"));
        assert!(
            job.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        );
        assert!(id.as_uuid().simple().to_string().starts_with(&job[6..]));
    }

    #[test]
    fn a_machine_type_name_round_trips_through_the_table() {
        for size in Size::OFFERED {
            let name = size.machine_type();
            assert!(name.starts_with(MACHINE_TYPE_PREFIX));
            assert_eq!(Size::named(&name), Some(size));
        }
        assert_eq!(
            Size::OFFERED.map(Size::machine_type),
            ["aca-1x2", "aca-2x4", "aca-4x8"].map(str::to_owned)
        );
    }

    #[test]
    fn a_size_the_catalog_never_published_is_not_a_size() {
        // Container Apps sells quarter cores and 8-core replicas exist on
        // other profiles; neither is on flyco's menu, so neither may be
        // provisioned or resized to.
        for name in ["aca-8x16", "aca-1x4", "aca-0.5x1", "Standard_D2als_v6", ""] {
            assert_eq!(Size::named(name), None, "`{name}` is not an offered size");
        }
    }

    #[test]
    fn a_native_id_round_trips_through_its_two_halves() {
        let native = Execution::native_id("flyco-abc", "flyco-abc-xk29p");
        let execution = Execution::parse(&native).expect("the id names an execution");
        assert_eq!(execution.job, "flyco-abc");
        assert_eq!(execution.name, "flyco-abc-xk29p");
    }

    #[test]
    fn an_id_that_names_no_execution_is_refused_rather_than_guessed_at() {
        // A virtual machine's `native_id` is a full ARM resource id, and a
        // container operation reaching one is a row this driver did not
        // write.
        for native in ["flyco-abc", "", "/", "flyco-abc/"] {
            Execution::parse(native).expect_err("an id without both halves is unusable");
        }
    }

    #[test]
    fn the_container_is_pinned_to_the_wire_this_control_plane_speaks() {
        let image = release::session_image_for_wire_protocol();
        assert!(image.starts_with("ghcr.io/lexoliu/flyco-session:wire-"));
        assert!(!image.ends_with(":latest"));

        let rendered = serde_json::to_value(template(Size::OFFERED[2])).expect("serialize");
        let container = &rendered["containers"][0];
        assert_eq!(container["image"], image);
        assert_eq!(container["resources"]["cpu"], 4.0);
        assert_eq!(container["resources"]["memory"], "8Gi");
        assert_eq!(container["env"][0]["name"], "FLYCO_DAEMON_CONFIG");
        assert_eq!(container["env"][0]["secretRef"], "daemon-config");
    }

    #[test]
    fn a_secret_never_debug_prints_the_configuration_it_holds() {
        let secret = super::Secret {
            name: super::CONFIG_SECRET,
            value: "ZmRfYS1saXZlLWRhZW1vbi10b2tlbg==".to_owned(),
        };
        let rendered = format!("{secret:?}");
        assert!(rendered.contains("daemon-config"));
        assert!(!rendered.contains("ZmRfYS1saXZl"));
    }
}
