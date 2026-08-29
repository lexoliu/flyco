//! The PTY terminal against a stand-in shell.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use flyco_daemon::terminal::{Terminal, TerminalSession as _};

fn script(name: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(name);
    let mut permissions = std::fs::metadata(&path)
        .unwrap_or_else(|error| panic!("{} must exist: {error}", path.display()))
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).expect("make the script executable");
    path
}

#[tokio::test]
async fn a_pty_relays_input_to_output() {
    let work = std::env::temp_dir().join(format!(
        "flycod-pty-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&work).expect("workdir");
    let (mut terminal, mut outputs) =
        Terminal::spawn(&script("fake-shell.sh"), &work).expect("start the stand-in shell");

    let mut seen = String::new();
    while !seen.contains("ready") {
        let chunk = tokio::time::timeout(Duration::from_secs(5), outputs.recv())
            .await
            .expect("the shell announced itself")
            .expect("the PTY closed before ready");
        seen.push_str(&chunk);
    }

    terminal.write("hello\n").expect("write");
    let mut echoed = String::new();
    while !echoed.contains("hello") {
        let chunk = tokio::time::timeout(Duration::from_secs(5), outputs.recv())
            .await
            .expect("the shell echoed")
            .expect("the PTY closed before echo");
        echoed.push_str(&chunk);
    }

    terminal.shutdown().expect("stop");
    let _ = std::fs::remove_dir_all(&work);
}
