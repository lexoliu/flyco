//! `flycod codespace` — the `postStart` entrypoint of a GitHub Codespaces
//! session machine.
//!
//! A codespace is given no per-machine secret: GitHub's repository secrets
//! are shared by every codespace on the repository, so the daemon
//! configuration a session needs is never written ahead of it. The machine
//! fetches it instead, from `POST /v1/providers/codespaces/bootstrap`,
//! authenticated by the pair GitHub injects into every codespace —
//! `CODESPACE_NAME`, which names the machine row, and a `GITHUB_TOKEN`
//! scoped to the environment repository, which the control plane asks
//! GitHub to vouch for.
//!
//! Both halves of the fetch race the provisioning leg: `postStart` can run
//! before the codespace's name is recorded (`404`) and before the sealed
//! configuration is stored (`409`), and either way the honest answer is to
//! ask again until the deadline runs out.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::time::Instant;

use flyco_core::{CodespacesBootstrap, CodespacesBootstrapRequest};
use url::Url;
use zenwave::Client as _;
use zenwave::ResponseExt as _;

/// Where every flyco machine keeps the daemon's configuration — the path
/// the VM's cloud-init writes and the codespace's `postStart` shares.
const CONFIG_PATH: &str = flyco_provider::cloud_init::CONFIG_PATH;

/// The detached daemon's output.
///
/// A codespace has no journal and `flycod` outlives the `postStart` that
/// launched it, so its diagnostics go to a file on the machine's own disk —
/// the one place a codespace can be asked afterwards what happened.
const LOG_PATH: &str = "/var/lib/flyco/flycod.log";

/// How long a codespace waits for its session's row.
///
/// The gap the retries span is provisioning: GitHub starts the codespace
/// while the queue is still recording its name, and three minutes covers
/// that window many times over without holding `postStart` open
/// indefinitely.
const DEADLINE: Duration = Duration::from_secs(180);

/// The first wait between asks.
const RETRY_INITIAL: Duration = Duration::from_secs(1);

/// The longest wait between asks.
const RETRY_MAX: Duration = Duration::from_secs(10);

/// What a codespace's environment cannot answer for.
#[derive(Debug, thiserror::Error)]
pub enum CodespaceError {
    /// A variable GitHub or the devcontainer injects was absent.
    ///
    /// Missing rather than empty is enforced: an empty name would 404 its
    /// way to the deadline reporting a lie about why.
    #[error(
        "${0} is not set; `flycod codespace` is the postStart of a codespace the control plane provisioned"
    )]
    Environment(&'static str),
    /// `FLYCO_CONTROL_PLANE` did not parse as a URL.
    #[error("FLYCO_CONTROL_PLANE is not an absolute URL: {0}")]
    ControlPlaneUrl(String),
    /// The control plane's answer asking again cannot change: a `401`, a
    /// `403`, a `400` — a refusal the account link or the machine row is
    /// responsible for, not a provisioning leg still running.
    #[error("the control plane refused this codespace: {0}")]
    Refused(String),
    /// A success answer whose body is not a bootstrap document.
    #[error("the control plane's bootstrap answer could not be read: {0}")]
    Malformed(String),
    /// Every ask until the deadline failed.
    #[error(
        "the control plane never handed over this session's configuration in {}s: {last}",
        DEADLINE.as_secs()
    )]
    Unanswered {
        /// What the last attempt failed with.
        last: String,
    },
    /// Writing the configuration failed.
    #[error("could not write {path}: {source}")]
    Write {
        /// Where the write was attempted.
        path: &'static str,
        /// What the filesystem said.
        source: std::io::Error,
    },
    /// Launching the daemon failed.
    #[error("could not start flycod: {0}")]
    Spawn(std::io::Error),
}

/// One variable the codespace must have been injected with.
fn required(name: &'static str) -> Result<String, CodespaceError> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Ok(value),
        _ => Err(CodespaceError::Environment(name)),
    }
}

/// The body of one bootstrap ask — the codespace's own name, which is the
/// machine's provider-native id.
fn ask(codespace_name: &str) -> CodespacesBootstrapRequest {
    CodespacesBootstrapRequest {
        codespace_name: codespace_name.to_owned(),
    }
}

/// Whether a failed ask is worth repeating.
///
/// `404` means the machine's name is not recorded yet and `409` means the
/// configuration is not stored yet — both are the provisioning leg still
/// running, so they wait. A server error or a transport failure is a wait
/// for the same reason a `404` is. Everything else — a `401`, a `403`, a
/// `400` — is the control plane answering no, and asking again cannot
/// change that.
fn retryable(error: &zenwave::Error) -> bool {
    match error {
        zenwave::Error::Http { status, .. } => {
            status.as_u16() == 404 || status.as_u16() == 409 || status.is_server_error()
        }
        _ => true,
    }
}

/// Asks the control plane for this session's daemon configuration until it
/// answers or the deadline passes.
async fn fetch(
    control_plane: &Url,
    codespace_name: &str,
    token: &str,
    deadline: Duration,
) -> Result<String, CodespaceError> {
    let url = control_plane
        .join("v1/providers/codespaces/bootstrap")
        .map_err(|_| CodespaceError::ControlPlaneUrl(control_plane.to_string()))?
        .to_string();
    let started = Instant::now();
    let mut wait = RETRY_INITIAL;

    loop {
        let mut client = zenwave::client();
        let asked = client
            .post(&url)
            .map(|request| request.bearer_auth(token.to_owned()))
            .and_then(|request| request.json_body(&ask(codespace_name)));
        match asked {
            Ok(request) => match request.await {
                Ok(response) => {
                    return response
                        .into_json::<CodespacesBootstrap>()
                        .await
                        .map(|body| body.config_toml)
                        .map_err(|error| CodespaceError::Malformed(error.to_string()));
                }
                Err(error) if retryable(&error) => {
                    tracing::debug!(%error, "the control plane is not ready for this codespace");
                    if started.elapsed() >= deadline {
                        return Err(CodespaceError::Unanswered {
                            last: error.to_string(),
                        });
                    }
                }
                Err(error) => return Err(CodespaceError::Refused(error.to_string())),
            },
            Err(error) => {
                // A request that cannot even be built — a malformed URL or
                // header — is the same on every retry; fail now.
                return Err(CodespaceError::Refused(error.to_string()));
            }
        }

        tokio::time::sleep(wait.min(deadline.saturating_sub(started.elapsed()))).await;
        wait = (wait * 2).min(RETRY_MAX);
    }
}

/// Fetches this session's configuration, writes it where `flycod run`
/// reads it, and launches the daemon detached.
///
/// Detached rather than `exec` or a supervised child: `postStart` is
/// GitHub's hook and it is expected to return, while `flycod` is the
/// codespace's reason to exist and runs for the session's whole life. The
/// child is given its own process group and a log file so nothing about
/// the hook that launched it can take it down.
///
/// # Errors
///
/// Returns [`CodespaceError`] describing which of the three steps failed.
pub async fn bootstrap() -> Result<(), CodespaceError> {
    let control_plane = required("FLYCO_CONTROL_PLANE")?;
    let codespace_name = required("CODESPACE_NAME")?;
    let token = required("GITHUB_TOKEN")?;

    let control_plane =
        Url::parse(&control_plane).map_err(|_| CodespaceError::ControlPlaneUrl(control_plane))?;
    let config_toml = fetch(&control_plane, &codespace_name, &token, DEADLINE).await?;

    write_config(Path::new(CONFIG_PATH), &config_toml)?;
    launch()
}

/// Writes the configuration `0600` — the document carries the session's
/// daemon token, and the codespace's other users have no business reading
/// it.
fn write_config(path: &Path, contents: &str) -> Result<(), CodespaceError> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, contents.as_bytes()))
        .map_err(|source| CodespaceError::Write {
            path: CONFIG_PATH,
            source,
        })
}

/// Starts `flycod run` as a new session leader and returns.
///
/// The executable is this same binary — `flycod codespace` is `flycod` —
/// resolved through `/proc/self/exe` rather than `PATH`, so the entrypoint
/// always launches the exact build it shipped as.
fn launch() -> Result<(), CodespaceError> {
    use std::os::unix::process::CommandExt as _;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_PATH)
        .map_err(|source| CodespaceError::Write {
            path: LOG_PATH,
            source,
        })?;
    let executable = std::env::current_exe().map_err(CodespaceError::Spawn)?;

    std::process::Command::new(executable)
        .args(["run", "--config", CONFIG_PATH])
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(CodespaceError::Spawn)?)
        .stderr(log)
        // A new process group: when the postStart hook reaps its own
        // children there is nothing of flycod's left in it to take down.
        .process_group(0)
        .spawn()
        .map_err(CodespaceError::Spawn)?;

    tracing::info!(config = CONFIG_PATH, log = LOG_PATH, "flycod started");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    /// An `Error::Http` with the given status, as zenwave reports a refused
    /// response.
    fn http_error(status: u16) -> zenwave::Error {
        let status = zenwave::StatusCode::from_u16(status).expect("a status code");
        let mut response = zenwave::Response::new(zenwave::Body::empty());
        *response.status_mut() = status;
        zenwave::Error::Http {
            status,
            message: "refused".to_owned(),
            response: Box::new(zenwave::error::HttpErrorResponse {
                response,
                body_text: None,
            }),
        }
    }

    /// A one-shot HTTP/1.1 server: answers each request with the next
    /// `(status, body)` pair — the last repeats — and records every request
    /// verbatim so a test can check the bearer and the body.
    async fn serve(
        answers: &'static [(u16, &'static str)],
    ) -> (Url, Arc<AtomicUsize>, Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a local listener");
        let port = listener.local_addr().expect("an address").port();
        let hits = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        tokio::spawn({
            let hits = hits.clone();
            let requests = requests.clone();
            async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let request = read_request(&mut socket).await;
                    requests.lock().expect("the requests").push(request);
                    let n = hits.fetch_add(1, Ordering::SeqCst);
                    let (status, body) = answers
                        .get(n)
                        .or_else(|| answers.last())
                        .copied()
                        .expect("serve is never called with no answers");
                    let response = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    if socket.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                }
            }
        });
        (
            Url::parse(&format!("http://127.0.0.1:{port}/")).expect("a URL"),
            hits,
            requests,
        )
    }

    /// Reads one request — headers and the `content-length` body — so the
    /// response a test sends is read by a client that has finished asking.
    async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let read = socket.read(&mut chunk).await.expect("a readable socket");
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(headers) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                let body_start = headers + 4;
                let wanted = std::str::from_utf8(&buffer[..headers])
                    .ok()
                    .and_then(|head| {
                        head.lines()
                            .find_map(|line| line.strip_prefix("content-length: "))
                    })
                    .and_then(|length| length.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if buffer.len() >= body_start + wanted {
                    break;
                }
            }
        }
        String::from_utf8(buffer).expect("a request is text")
    }

    #[test]
    fn a_variable_the_codespace_was_not_given_is_named() {
        let error = required("FLYCO_TEST_NEVER_SET").expect_err("unset is an error");
        assert!(matches!(
            error,
            CodespaceError::Environment("FLYCO_TEST_NEVER_SET")
        ));
    }

    #[test]
    fn an_empty_variable_is_missing() {
        // SAFETY: a test-private variable name, set and removed by this test
        // alone.
        unsafe {
            std::env::set_var("FLYCO_TEST_EMPTY", "");
            assert!(matches!(
                required("FLYCO_TEST_EMPTY"),
                Err(CodespaceError::Environment("FLYCO_TEST_EMPTY"))
            ));
            std::env::set_var("FLYCO_TEST_EMPTY", "given");
            assert_eq!(required("FLYCO_TEST_EMPTY").expect("present"), "given");
            std::env::remove_var("FLYCO_TEST_EMPTY");
        }
    }

    #[test]
    fn a_still_provisioning_answer_is_worth_repeating() {
        for status in [404u16, 409, 500, 503] {
            assert!(retryable(&http_error(status)), "{status} retries");
        }
        for status in [400u16, 401, 403] {
            assert!(!retryable(&http_error(status)), "{status} is a refusal");
        }
    }

    #[test]
    fn the_configuration_lands_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let path = std::env::temp_dir().join(format!("flycod-test-{}.toml", uuid::Uuid::new_v4()));
        write_config(&path, "token = \"secret\"\n").expect("written");
        let metadata = std::fs::metadata(&path).expect("readable");
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            "token = \"secret\"\n"
        );
        std::fs::remove_file(&path).expect("cleaned up");
    }

    #[tokio::test]
    async fn a_codespace_waits_out_the_provisioning_leg() {
        let (url, hits, requests) = serve(&[
            (404, "{\"error\":\"not yet\"}"),
            (200, "{\"config_toml\":\"token = \\\"sealed\\\"\"}"),
        ])
        .await;

        let toml = fetch(&url, "lexo-flyco-abc", "gho_injected", DEADLINE)
            .await
            .expect("the second ask answers");

        assert_eq!(toml, "token = \"sealed\"");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        let request = &requests.lock().expect("requests")[0];
        assert!(request.contains("authorization: Bearer gho_injected"));
        assert!(request.contains("\"codespace_name\":\"lexo-flyco-abc\""));
        assert!(request.starts_with("POST /v1/providers/codespaces/bootstrap"));
    }

    #[tokio::test]
    async fn a_refusal_is_not_retried() {
        let (url, hits, _) = serve(&[(401, "{\"error\":\"denied\"}")]).await;

        let error = fetch(&url, "lexo-flyco-abc", "gho_bad", DEADLINE)
            .await
            .expect_err("a 401 is final");

        assert!(matches!(error, CodespaceError::Refused(_)));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_deadline_ends_a_silent_control_plane() {
        let (url, _, _) = serve(&[(404, "{}")]).await;

        let error = fetch(
            &url,
            "lexo-flyco-abc",
            "gho_injected",
            Duration::from_millis(50),
        )
        .await
        .expect_err("the deadline passes");

        assert!(matches!(error, CodespaceError::Unanswered { .. }));
    }

    #[tokio::test]
    async fn a_success_that_is_not_a_bootstrap_is_malformed() {
        let (url, _, _) = serve(&[(200, "{\"unexpected\":true}")]).await;

        let error = fetch(&url, "lexo-flyco-abc", "gho_injected", DEADLINE)
            .await
            .expect_err("the body does not parse");

        assert!(matches!(error, CodespaceError::Malformed(_)));
    }
}
