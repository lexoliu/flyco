//! The host⇄control-plane wire protocol.
//!
//! One outbound WebSocket per enrolled machine, relayed through that host's
//! Durable Object. Every frame is one JSON-encoded message from this module,
//! internally tagged on `type` — and **every variant is a struct variant**,
//! including the ones carrying a single value, for the reason
//! [`flyco_core::wire`] spells out: a newtype variant holding another
//! internally-tagged enum emits a tag twice and cannot be read back.
//!
//! Two enums, one per direction:
//!
//! | Enum | From | To |
//! |---|---|---|
//! | [`HostToControl`] | `flycod host` | the host room |
//! | [`ControlToHost`] | the host room | `flycod host` |
//!
//! Why they live in `flyco_provider` rather than in `flyco_core` beside the
//! session protocol is in the [module documentation](super).

use flyco_core::MachineId;
use flyco_core::host::{HostFacts, JobOutcome};
use serde::{Deserialize, Serialize};

use super::ContainerJob;

/// Messages from an enrolled machine to the control plane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostToControl {
    /// First frame after connecting: what this machine currently is.
    ///
    /// The facts are re-reported on every connection rather than only at
    /// enrollment, because they change: memory is added, a disk fills,
    /// Podman is upgraded. The room records them and marks the host online.
    Hello {
        /// What the machine measured about itself.
        facts: Box<HostFacts>,
    },
    /// What came of one container job.
    ///
    /// The room forgets the job it was holding; the *durable* half of the
    /// same fact — the machine row's container and volume names — arrives at
    /// the Worker over REST as a
    /// [`ReportJobResult`](flyco_core::host::ReportJobResult), because a
    /// Durable Object can reach neither D1 nor the provisioning queue.
    JobResult {
        /// The machine the job acted on, which is the job's identity.
        job_id: MachineId,
        /// What came of it.
        outcome: JobOutcome,
    },
    /// Nothing has happened and the machine is still here.
    ///
    /// A hibernating socket can sit idle for hours, and an idle socket is
    /// indistinguishable from a machine that was unplugged until something
    /// is written to it. This is what moves `last_seen`.
    Heartbeat,
}

/// Messages from the control plane to an enrolled machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlToHost {
    /// Perform one container job.
    ///
    /// Every machine operation is one of these — creating a session's
    /// container, stopping it, starting it again, removing it — because
    /// [`ContainerJob`] is already the whole vocabulary of work a host does.
    /// A second set of variants naming stop and start would be the same four
    /// operations written twice, free to disagree with the planner.
    Run {
        /// The job to perform.
        job: ContainerJob,
    },
    /// This machine's token has been revoked; stop and stay stopped.
    ///
    /// Sent as a host is removed, so its unit exits rather than reconnecting
    /// every few seconds against a credential that will never work again.
    Revoked,
}

impl ControlToHost {
    /// Whether this command must be kept for a host that is not connected.
    ///
    /// Container work must: a job dropped because the machine was briefly
    /// offline is a session that never gets its container, or a container
    /// nobody ever removes. A revocation must not — a host that reconnects
    /// after one is refused at the upgrade, because its token no longer
    /// authenticates, so holding the frame would be waiting for a socket
    /// that cannot open.
    #[must_use]
    pub const fn survives_a_disconnect(&self) -> bool {
        matches!(self, Self::Run { .. })
    }
}

#[cfg(test)]
mod tests {
    use flyco_core::MachineId;
    use flyco_core::host::JobOutcome;

    use super::{ControlToHost, HostToControl};
    use crate::host::tests::{HOSTNAME, facts, host, provision};

    /// Round-trips through the JSON *text*, not through a `Value`: only the
    /// text form proves a tagged frame survives the wire.
    fn round_trip<T>(value: &T)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + core::fmt::Debug,
    {
        let json = serde_json::to_string(value).expect("serialize");
        let back: T = serde_json::from_str(&json)
            .unwrap_or_else(|error| panic!("{json} did not deserialize: {error}"));
        assert_eq!(&back, value);
    }

    fn every_host_frame() -> Vec<HostToControl> {
        vec![
            HostToControl::Hello {
                facts: Box::new(facts()),
            },
            HostToControl::JobResult {
                job_id: MachineId::generate(),
                outcome: JobOutcome::Running {
                    container: "flyco-9d0f".to_owned(),
                    volume: "flyco-9d0f-work".to_owned(),
                },
            },
            HostToControl::JobResult {
                job_id: MachineId::generate(),
                outcome: JobOutcome::Failed {
                    message: "podman: no space left on device".to_owned(),
                },
            },
            HostToControl::Heartbeat,
        ]
    }

    fn every_control_frame() -> Vec<ControlToHost> {
        vec![
            ControlToHost::Run {
                job: host().plan(&provision(HOSTNAME)).expect("plan a create"),
            },
            ControlToHost::Revoked,
        ]
    }

    #[test]
    fn every_host_frame_survives_the_wire() {
        for frame in every_host_frame() {
            round_trip(&frame);
        }
    }

    #[test]
    fn every_control_frame_survives_the_wire() {
        for frame in every_control_frame() {
            round_trip(&frame);
        }
    }

    #[test]
    fn a_nested_job_keeps_both_tags_apart() {
        let command = ControlToHost::Run {
            job: host().plan(&provision(HOSTNAME)).expect("plan a create"),
        };
        let json = serde_json::to_value(&command).expect("serialize");

        assert_eq!(json["type"], "run");
        assert_eq!(
            json["job"]["job"], "create",
            "the job's own tag must nest rather than collide with the frame's"
        );
        round_trip(&command);
    }

    #[test]
    fn a_hello_reports_the_machines_current_facts() {
        let json = serde_json::to_value(HostToControl::Hello {
            facts: Box::new(facts()),
        })
        .expect("serialize");
        assert_eq!(json["type"], "hello");
        assert_eq!(json["facts"]["hostname"], HOSTNAME);
    }

    #[test]
    fn only_container_work_is_kept_for_a_host_that_is_away() {
        assert!(
            ControlToHost::Run {
                job: host().plan(&provision(HOSTNAME)).expect("plan a create"),
            }
            .survives_a_disconnect()
        );
        assert!(
            !ControlToHost::Revoked.survives_a_disconnect(),
            "a revoked host never opens another socket to be told twice"
        );
    }
}
