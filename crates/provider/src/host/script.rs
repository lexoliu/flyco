//! The shell one [`ContainerJob`] becomes on the machine.
//!
//! One script per job, rendered from a template under `templates/host/`
//! with every interpolated value shell-quoted first. Podman is invoked,
//! nothing else; flyco never writes to the host's filesystem, and the
//! session's `flycod` configuration — which carries its daemon token —
//! reaches the container through an env-file on the shell's stdin rather
//! than as a command-line argument a `ps` would show.
//!
//! Rendered here rather than on the machine, and tested here, because the
//! templates *are* the contract with Podman: `flycod host` runs what this
//! produces, and a quoting mistake in it is a container name that becomes
//! another command on somebody's real hardware.

use askama::Template;
use base64::Engine as _;

use super::{CONFIG_ENV, ContainerJob};
use crate::{ProviderError, flycod};

/// Renders the script for one job.
///
/// Every interpolated value is shell-quoted before it reaches the template,
/// so a container name or an image reference cannot become another command.
///
/// # Errors
///
/// Returns [`ProviderError::Malformed`] if the daemon configuration or the
/// template itself would not render, both of which are flyco's own bug
/// rather than anything the host did.
pub fn render(job: &ContainerJob) -> Result<String, ProviderError> {
    let rendered = match job {
        ContainerJob::Create {
            container,
            volume,
            image,
            machine,
            bootstrap,
        } => {
            let config = flycod::render(bootstrap)
                .map_err(|_| ProviderError::Malformed("the flycod configuration did not render"))?;
            CreateScript {
                container: quote(container),
                volume: quote(volume),
                workdir: quote(flycod::WORKDIR),
                image: quote(image),
                machine: quote(&machine.to_string()),
                session: quote(&bootstrap.session.to_string()),
                config_env: CONFIG_ENV,
                config_base64: base64::engine::general_purpose::STANDARD.encode(config),
            }
            .render()
        }
        ContainerJob::Stop { container } => StopScript {
            container: quote(container),
        }
        .render(),
        ContainerJob::Start { container } => StartScript {
            container: quote(container),
        }
        .render(),
        ContainerJob::Remove {
            container,
            volume,
            keep_volume,
        } => RemoveScript {
            container: quote(container),
            volume: quote(volume),
            keep_volume: *keep_volume,
        }
        .render(),
    };

    rendered.map_err(|_| ProviderError::Malformed("a podman script template did not render"))
}

/// POSIX-quotes one value for the host's shell.
fn quote(value: &str) -> String {
    shell_escape::unix::escape(value.into()).into_owned()
}

#[derive(Template)]
#[template(path = "host/create.sh", escape = "none")]
struct CreateScript {
    container: String,
    volume: String,
    workdir: String,
    image: String,
    machine: String,
    session: String,
    config_env: &'static str,
    config_base64: String,
}

#[derive(Template)]
#[template(path = "host/stop.sh", escape = "none")]
struct StopScript {
    container: String,
}

#[derive(Template)]
#[template(path = "host/start.sh", escape = "none")]
struct StartScript {
    container: String,
}

#[derive(Template)]
#[template(path = "host/remove.sh", escape = "none")]
struct RemoveScript {
    container: String,
    volume: String,
    keep_volume: bool,
}

#[cfg(test)]
mod tests {
    use flyco_core::MachineId;

    use super::render;
    use crate::MachineOperation;
    use crate::host::tests::{HOSTNAME, bootstrap, host};
    use crate::host::{CONFIG_ENV, ContainerJob, container_name, volume_name};
    use crate::{ProvisionRequest, flycod};
    use flyco_core::machine::{CloudProviderKind, MachineSpec, Runtime};

    const TOKEN: &str = "fd_a-live-daemon-token";

    fn create() -> ContainerJob {
        let mut bootstrap = bootstrap();
        bootstrap.daemon_token = TOKEN.to_owned();
        host()
            .plan(&MachineOperation::Provision(Box::new(ProvisionRequest {
                machine: MachineId::generate(),
                spec: MachineSpec {
                    provider: CloudProviderKind::Host,
                    machine_type: HOSTNAME.to_owned(),
                    runtime: Runtime::Container,
                    region: HOSTNAME.to_owned(),
                    spot: true,
                    disk_gib: 0,
                },
                bootstrap,
            })))
            .expect("plan a create")
    }

    #[test]
    fn creating_mounts_the_sessions_volume_at_the_checkout() {
        let script = render(&create()).expect("render");
        assert!(script.contains("podman run"));
        assert!(
            script.contains(&format!(":{}", flycod::WORKDIR)),
            "the volume must be mounted where the checkout lives: {script}"
        );
        assert!(
            script.contains("podman volume"),
            "the volume has to exist before the container mounts it: {script}"
        );
    }

    #[test]
    fn the_daemon_token_never_reaches_the_command_line() {
        let script = render(&create()).expect("render");

        let (command_lines, heredoc): (Vec<&str>, Vec<&str>) = script
            .lines()
            .partition(|line| !line.starts_with(CONFIG_ENV));

        assert!(
            command_lines.iter().all(|line| !line.contains(TOKEN)),
            "the token must not appear in any podman argument"
        );
        assert_eq!(heredoc.len(), 1, "the config rides one env-file line");
        assert!(heredoc[0].starts_with(CONFIG_ENV));
        assert!(
            !heredoc[0].contains(TOKEN),
            "the config is base64, so even the env-file line is not the token verbatim"
        );
    }

    #[test]
    fn a_hostile_container_name_cannot_become_another_command() {
        let script = render(&ContainerJob::Stop {
            container: "flyco-x; rm -rf /".to_owned(),
        })
        .expect("render");

        assert!(script.contains("'flyco-x; rm -rf /'"));
        assert!(!script.contains("podman stop flyco-x; rm"));
    }

    #[test]
    fn a_removal_keeps_the_volume_exactly_when_it_was_told_to() {
        let machine = MachineId::generate();
        let volume = volume_name(machine);
        let removal = |keep_volume| ContainerJob::Remove {
            container: container_name(machine),
            volume: volume.clone(),
            keep_volume,
        };

        let kept = render(&removal(true)).expect("render");
        assert!(kept.contains("podman rm"));
        assert!(
            !kept.contains("podman volume rm"),
            "a machine destroyed with its disk kept must not lose the work on it: {kept}"
        );

        let released = render(&removal(false)).expect("render");
        assert!(released.contains("podman volume rm"));
        assert!(
            released.contains(&volume),
            "the volume being released has to be named: {released}"
        );
    }
}
