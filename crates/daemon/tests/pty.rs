//! The PTY terminal against a stand-in shell.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use flyco_daemon::terminal::{Terminal, TerminalEvent, TerminalSession as _};
use portable_pty::CommandBuilder;

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

fn workdir(tag: &str) -> PathBuf {
    let work = std::env::temp_dir().join(format!(
        "flycod-pty-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&work).expect("workdir");
    work
}

/// A `sh -c` foreground command.
fn sh(script: &str) -> CommandBuilder {
    let mut command = CommandBuilder::new("sh");
    command.arg("-c");
    command.arg(script);
    command
}

/// Output events until `seen` contains `marker`; exits pass through.
async fn output_until(outputs: &mut tokio::sync::mpsc::Receiver<TerminalEvent>, marker: &str) {
    let mut seen = String::new();
    while !seen.contains(marker) {
        match tokio::time::timeout(Duration::from_secs(5), outputs.recv())
            .await
            .unwrap_or_else(|_| panic!("{marker} never arrived"))
            .expect("the PTY closed early")
        {
            TerminalEvent::Output(chunk) => seen.push_str(&chunk),
            TerminalEvent::Exited { code } => {
                panic!("the foreground exited ({code:?}) before {marker}")
            }
        }
    }
}

/// Events until the foreground exits; the output seen on the way.
async fn until_exit(
    outputs: &mut tokio::sync::mpsc::Receiver<TerminalEvent>,
) -> (String, Option<i32>) {
    let mut seen = String::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), outputs.recv())
            .await
            .expect("the foreground never exited")
            .expect("the PTY closed early")
        {
            TerminalEvent::Output(chunk) => seen.push_str(&chunk),
            TerminalEvent::Exited { code } => return (seen, code),
        }
    }
}

#[tokio::test]
async fn a_pty_relays_input_to_output() {
    let work = workdir("echo");
    let (mut terminal, mut outputs) =
        Terminal::spawn(&script("fake-shell.sh"), &work).expect("start the stand-in shell");

    output_until(&mut outputs, "ready").await;

    terminal.write("hello\n").expect("write");
    output_until(&mut outputs, "hello").await;

    terminal.shutdown().expect("stop");
    let _ = std::fs::remove_dir_all(&work);
}

#[tokio::test]
async fn a_harness_exit_reports_and_returns_the_shell() {
    let work = workdir("exit");
    let (mut terminal, mut outputs) =
        Terminal::spawn(&script("fake-shell.sh"), &work).expect("start the stand-in shell");
    output_until(&mut outputs, "ready").await;

    terminal
        .launch_harness(sh("echo tui-screen; exit 7"))
        .expect("launch");

    let (seen, code) = until_exit(&mut outputs).await;
    assert_eq!(code, Some(7), "the exit code reaches the wire");
    assert!(
        seen.contains("tui-screen"),
        "its last bytes landed first: {seen}"
    );

    // The shell is back in the foreground and answers keystrokes.
    output_until(&mut outputs, "ready").await;
    terminal.write("still-here\n").expect("write");
    output_until(&mut outputs, "still-here").await;

    terminal.shutdown().expect("stop");
    let _ = std::fs::remove_dir_all(&work);
}

#[tokio::test]
async fn a_second_launch_leaves_the_running_harness_alone() {
    let work = workdir("ensure");
    let (mut terminal, mut outputs) =
        Terminal::spawn(&script("fake-shell.sh"), &work).expect("start the stand-in shell");
    output_until(&mut outputs, "ready").await;

    // A harness that announces itself then sleeps; the second launch asks
    // for a different marker that must never appear.
    terminal
        .launch_harness(sh("echo first-tui; sleep 2"))
        .expect("launch");
    output_until(&mut outputs, "first-tui").await;

    terminal
        .launch_harness(sh("echo second-tui"))
        .expect("a second launch is accepted and ignored");

    // The sleeping harness's exit — not the second command's output — is
    // what the stream reports next.
    let (seen, code) = until_exit(&mut outputs).await;
    assert_eq!(code, Some(0));
    assert!(
        !seen.contains("second-tui"),
        "the ensure did not restart the TUI: {seen}"
    );

    terminal.shutdown().expect("stop");
    let _ = std::fs::remove_dir_all(&work);
}
