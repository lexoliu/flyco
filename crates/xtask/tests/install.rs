//! The installer assets, checked as what they are: shell and units a
//! machine runs as root.
//!
//! `crates/xtask/src/release.rs` proves the *names* a publish uploads. This
//! proves the *files* behind those names exist, parse, and agree with each
//! other — the installer and the host unit have to name the same
//! configuration file, because one writes it and the other reads it, and a
//! disagreement between them is a machine that enrols and then fails to
//! start.

use std::path::{Path, PathBuf};
use std::process::Command;

use flyco_core::release::{ASSETS, HOST_UNIT, INSTALLER, UNIT};

/// Where the assets live, relative to this crate.
fn install_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("install")
}

/// Reads one published asset out of the repository.
fn asset(name: &str) -> String {
    let path = install_dir().join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "{} is published but not in the repository: {error}",
            path.display()
        )
    })
}

/// The value of a `name=value` assignment in the installer, with any
/// `${...}` default expansion left as written.
fn assignment(script: &str, name: &str) -> String {
    let prefix = format!("{name}=");
    script
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("the installer assigns `{name}`"))
        .to_owned()
}

#[test]
fn every_published_asset_is_a_file_in_the_repository() {
    for published in ASSETS {
        assert!(
            !asset(published.name).is_empty(),
            "{} is empty",
            published.name
        );
    }
}

#[test]
fn the_installer_is_a_shell_script_the_shell_accepts() {
    let status = Command::new("/bin/sh")
        .arg("-n")
        .arg(install_dir().join(INSTALLER.name))
        .status()
        .expect("running `sh -n`");

    assert!(status.success(), "`sh -n {}` failed", INSTALLER.name);
}

#[test]
fn the_installer_enrols_a_host_from_the_one_line_the_wizard_prints() {
    let script = asset(INSTALLER.name);

    // `sh -s -- host enroll fh_…`: the two words the control plane's command
    // template writes, dispatched in that order.
    assert!(
        script.contains("host)"),
        "the installer dispatches on `host`"
    );
    assert!(
        script.contains(r#"= "enroll" ] || usage"#),
        "`host` takes exactly the `enroll` subcommand: {script}"
    );
    assert!(
        script.contains("flycod\" host enroll"),
        "the enrollment runs `flycod host enroll`: {script}"
    );
    for flag in ["--token", "--control-plane", "--volume-root", "--config"] {
        assert!(
            script.contains(flag),
            "`flycod host enroll` is passed {flag}: {script}"
        );
    }
}

#[test]
fn enrolling_installs_podman_the_flyco_user_and_the_host_unit() {
    let script = asset(INSTALLER.name);

    assert!(
        script.contains(
            "apt-get install --yes --no-install-recommends podman uidmap dbus-user-session"
        ),
        "Podman is installed from apt when the machine has none: {script}"
    );
    assert!(
        script.contains("command -v podman >/dev/null 2>&1"),
        "and only when the machine has none: {script}"
    );
    assert!(
        script.contains("--add-subuids") && script.contains("--add-subgids"),
        "rootless Podman needs subordinate id ranges: {script}"
    );
    assert!(
        script.contains("loginctl enable-linger"),
        "the flyco user's systemd instance has to outlive a login: {script}"
    );
    assert!(
        script.contains(&format!("install_unit {}", HOST_UNIT.name)),
        "the host unit is installed: {script}"
    );
    assert!(
        script.contains("systemctl enable --now flycod-host.service"),
        "and started: {script}"
    );
}

#[test]
fn a_session_image_is_still_built_by_the_bare_installer() {
    let script = asset(INSTALLER.name);

    assert!(
        script.contains(r#""") install_session ;;"#),
        "an argument-less run is the cloud image cloud-init builds: {script}"
    );
    assert!(
        script.contains(&format!("install_unit {}", UNIT.name)),
        "which installs the session unit: {script}"
    );
}

/// The installer writes the configuration; the unit reads it. One path.
#[test]
fn the_installer_and_the_host_unit_name_the_same_configuration() {
    let script = asset(INSTALLER.name);
    let directory = assignment(&script, "host_config_dir");
    let configured = assignment(&script, "host_config");

    assert_eq!(configured, "$host_config_dir/host.toml");
    let path = format!("{directory}/host.toml");

    let unit = asset(HOST_UNIT.name);
    assert!(
        unit.contains(&format!(
            "ExecStart=/usr/local/bin/flycod host run --config {path}"
        )),
        "the unit runs `flycod host run` against {path}: {unit}"
    );
}

#[test]
fn the_host_unit_runs_as_root_so_the_host_token_can_stay_root_only() {
    let unit = asset(HOST_UNIT.name);

    assert!(unit.contains("User=root"), "{unit}");
    assert!(
        unit.contains("Restart=on-failure"),
        "a revoked host exits 0 and must stay stopped: {unit}"
    );
}
