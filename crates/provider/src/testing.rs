//! Recorded exchanges, so a driver can be tested without a cloud account.
//!
//! No credentials exist in flyco's development or CI environments and no
//! test may create a cloud resource, so the drivers are pinned against
//! fixtures instead: [`RecordedTransport`] answers each request in turn from
//! a scripted list and keeps every request it was given, which is what makes
//! "the exact URL, the exact `api-version`, the exact JSON body" an
//! assertion rather than a hope.
//!
//! The responses themselves live in `crates/provider/fixtures/` as JSON
//! documents, copied from what the provider's API actually returns, rather
//! than inline in the tests: a fixture that is a file can be diffed against
//! the vendor's documentation, and the no-multi-line-string-literal rule
//! points the same way.

use core::cell::RefCell;

use crate::clock::Timer;
use crate::http::{HttpError, HttpRequest, HttpResponse, HttpTransport};
use crate::{GitIdentity, RepoCheckout};

/// The GitHub token every fixture bootstrap carries.
///
/// Named rather than inlined because what the driver tests assert about it
/// is that it is *absent*: it must not appear in a rendered cloud-init
/// document's logs, in a `Debug` rendering, or anywhere but the config the
/// machine reads.
pub const GITHUB_TOKEN: &str = "gho_a-user-access-token";

/// The repository every fixture bootstrap checks out.
///
/// One fixture rather than one per driver: the drivers all embed the same
/// rendered configuration, and five copies of this would be five places to
/// forget when a field is added to [`RepoCheckout`].
///
/// # Panics
///
/// Panics if the constants above stop being a valid slug and branch, which
/// would be this fixture being wrong rather than anything under test.
#[must_use]
pub fn checkout() -> RepoCheckout {
    RepoCheckout {
        slug: "lexoliu/flyco".parse().expect("a valid repository slug"),
        branch: "dev".parse().expect("a valid branch name"),
        token: GITHUB_TOKEN.to_owned(),
        identity: GitIdentity {
            name: "lexoliu".to_owned(),
            email: "4242+lexoliu@users.noreply.github.com".to_owned(),
        },
    }
}

/// A transport that answers from a script and records what it was asked.
///
/// `RefCell` rather than a lock: a test is single-threaded, the borrows
/// never cross an await, and a lock here would be ceremony around a `Vec`.
#[derive(Debug)]
pub struct RecordedTransport {
    responses: RefCell<Vec<HttpResponse>>,
    requests: RefCell<Vec<HttpRequest>>,
}

impl RecordedTransport {
    /// Scripts the responses, in the order the driver will receive them.
    #[must_use]
    pub fn new(responses: Vec<HttpResponse>) -> Self {
        // Reversed once so each answer is a `pop`, which keeps `send`
        // free of an index the two `RefCell`s would have to agree on.
        let mut responses = responses;
        responses.reverse();
        Self {
            responses: RefCell::new(responses),
            requests: RefCell::new(Vec::new()),
        }
    }

    /// The `index`th request, which must exist.
    ///
    /// # Panics
    ///
    /// Panics when the driver made fewer requests than the test expected —
    /// which is the assertion, phrased as an index.
    #[must_use]
    pub fn request(&self, index: usize) -> HttpRequest {
        self.requests
            .borrow()
            .get(index)
            .unwrap_or_else(|| {
                panic!(
                    "the driver made {} requests, not {}",
                    self.requests.borrow().len(),
                    index + 1
                )
            })
            .clone()
    }

    /// How many requests were made.
    #[must_use]
    pub fn request_count(&self) -> usize {
        self.requests.borrow().len()
    }
}

impl HttpTransport for RecordedTransport {
    /// Answers from the script without suspending, and without being
    /// `Send`: a scripted answer is already in memory, and the `RefCell`
    /// that records the request is what makes the double single-threaded on
    /// purpose.
    fn send(&self, request: HttpRequest) -> impl Future<Output = Result<HttpResponse, HttpError>> {
        let response = self.responses.borrow_mut().pop();
        self.requests.borrow_mut().push(request.clone());
        core::future::ready(response.ok_or_else(|| {
            HttpError::Transport(format!(
                "the driver made an unscripted {} request to {}",
                request.method, request.url
            ))
        }))
    }
}

/// A timer that never waits and remembers what it was asked to wait for.
///
/// A polling loop's obedience to `Retry-After` is exactly the kind of thing
/// that is easy to write and easy to quietly drop, so it is asserted rather
/// than observed as elapsed wall time.
#[derive(Debug, Default)]
pub struct RecordingTimer {
    slept: RefCell<Vec<u32>>,
}

impl RecordingTimer {
    /// A timer that has waited for nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every delay the caller asked for, in order.
    #[must_use]
    pub fn delays(&self) -> Vec<u32> {
        self.slept.borrow().clone()
    }
}

impl Timer for RecordingTimer {
    fn sleep(&self, seconds: u32) -> impl Future<Output = ()> {
        self.slept.borrow_mut().push(seconds);
        core::future::ready(())
    }
}

impl<T: Timer> Timer for &T {
    fn sleep(&self, seconds: u32) -> impl Future<Output = ()> {
        (*self).sleep(seconds)
    }
}

/// The MCP registry a bootstrap fixture carries.
///
/// One server of each transport, because the two are rendered differently
/// into every harness's configuration and a fixture with only one of them
/// would let the other rot. Written once here for the same reason
/// [`session_machine`] is.
#[must_use]
pub fn mcp_servers() -> Vec<flyco_core::McpServerMount> {
    vec![
        flyco_core::McpServerMount {
            name: "deepwiki".to_owned(),
            config: flyco_core::McpServerConfig::Http {
                url: "https://mcp.deepwiki.com/mcp".to_owned(),
                headers: vec![flyco_core::HeaderEntry {
                    name: "authorization".to_owned(),
                    value: "Bearer a-registered-token".to_owned(),
                }],
            },
        },
        flyco_core::McpServerMount {
            name: "git".to_owned(),
            config: flyco_core::McpServerConfig::Stdio {
                command: "bunx".to_owned(),
                args: vec![
                    "-y".to_owned(),
                    "@modelcontextprotocol/server-git".to_owned(),
                ],
                env: vec![flyco_core::EnvEntry {
                    key: "GIT_DIR".to_owned(),
                    value: "/srv/flyco/work/.git".to_owned(),
                }],
            },
        },
    ]
}

/// The model a bootstrap fixture's session runs on.
///
/// Beside [`session_machine`] and for the same reason: every driver's tests
/// build a [`DaemonBootstrap`](crate::DaemonBootstrap) and none of them
/// cares which model it names. An effort is included, because a fixture
/// that omitted one would leave the half of the rendering that writes it
/// untested everywhere but the one test that checks it.
#[must_use]
pub fn session_model() -> flyco_core::ModelChoice {
    flyco_core::ModelChoice {
        model: "sonnet".to_owned(),
        effort: Some("high".to_owned()),
    }
}

/// The machine a bootstrap fixture describes.
///
/// Every driver's tests build a [`DaemonBootstrap`](crate::DaemonBootstrap),
/// and all of them want the same uninteresting answer to "which machine is
/// this": a mid-sized metered Linux type with a spot rate and no licence
/// minimum. Written once here so that adding a fact to the bootstrap is one
/// edit rather than eight, and so a test that cares about a *different*
/// machine — a license-bound one — says so by constructing its own.
#[must_use]
pub fn session_machine() -> flyco_core::SessionMachine {
    flyco_core::SessionMachine {
        machine_type: "Standard_D4s_v6".to_owned(),
        hourly: Some(flyco_core::Usd::from_cents(19)),
        spot: true,
        capacity: Some(flyco_core::MachineCapacity {
            vcpus: 4,
            memory_mib: 16 * 1024,
        }),
        minimum: None,
    }
}
