//! The GitHub Codespaces driver.
//!
//! Plain HTTPS against `api.github.com` with the account's own OAuth token —
//! the `codespace` scope the link sign-in asks for is the whole credential,
//! so this is the simplest of the drivers: no token mint, no operation
//! resource, no signing. A mutating call answers with the codespace itself,
//! and it is the codespace's `state` field that is polled to a terminal
//! value rather than an operation resource.
//!
//! # The disk is the point
//!
//! A codespace is a [`Runtime::Vm`]: `/workspaces` — and the whole
//! container filesystem — survives a `stop`, so start and stop map onto
//! flyco's deallocate/start pair exactly. What differs from every other
//! driver is *who can stop it*: GitHub suspends a codespace once its own
//! idle timeout passes, with no notice delivered to anything inside it.
//! The control plane reconciles that out of band (see `crates/api`), and
//! this driver answers for it the same way it answers for anything else:
//! `GET /user/codespaces/{name}` is the truth about whether the machine
//! still exists.
//!
//! # There is no spot
//!
//! Codespaces has one capacity market. A spec asking for spot gets
//! on-demand capacity, and the row records what it got — which is what
//! [`CapacityMode::OnDemand`] here reports.

#[cfg(test)]
mod tests;

use flyco_core::machine::{
    CloudProviderKind, CpuArchitecture, FreeGrant, MachineCapacity, MachineCatalogEntry,
    MachineLineage, MachinePricing, MachineState, OsFamily, Runtime, StoragePricing,
};
use flyco_core::{CloudSpend, MachineId, Usd};
use serde::{Deserialize, Serialize};

use crate::clock::{SystemTimer, Timer};
use crate::http::{HttpRequest, HttpResponse, HttpTransport, Method};
use crate::polling::{MAX_POLL_ATTEMPTS, POLLS_PER_INVOCATION, poll_delay};
use crate::{
    CapacityMode, CloudProvider, Continuation, LiveTransport, Machine, ProviderError,
    ProvisionRequest, Provisioning,
};

/// Driver name, as it appears in [`ProviderError::Unsupported`].
pub const PROVIDER: &str = "codespaces";

/// The repository every codespace is created on, named in the account it
/// belongs to.
///
/// A codespace must belong to a repository, and flyco's sessions do not —
/// the session's checkout happens on the disk after boot, the same as every
/// other provider. This one holds the devcontainer that boots the session
/// image instead: it is created private at link time, because a codespace
/// on a public repository is one other people could open.
pub const ENV_REPO_NAME: &str = "flyco-sessions";

/// The `geo` values a create request may name.
///
/// GitHub's own four. `location`, the older request field, is closing down
/// and is not written.
pub const GEOS: [&str; 4] = ["UsEast", "UsWest", "EuropeWest", "SoutheastAsia"];

/// `api.github.com`.
const API: &str = "https://api.github.com";

/// The API version GitHub asks every request to pin.
const API_VERSION: &str = "2022-11-28";

/// GitHub refuses a request with no `User-Agent`.
const USER_AGENT: &str = "flyco-control-plane";

/// What one core costs for an hour: the published $0.18/hour for the
/// two-core machine, halved.
const CORE_HOUR_MICROS: u64 = 90_000;

/// What one GiB of codespace storage costs for an hour: the published
/// $0.07/GiB-month over a 730-hour month. The disk is billed while the
/// codespace exists, running or stopped — which is exactly what
/// [`StoragePricing::PerGibHourly`] says.
const STORAGE_GIB_HOUR_MICROS: u64 = 96;

/// Minutes of idleness before GitHub suspends a codespace.
///
/// The default, kept deliberately: the free monthly core-hours are the
/// reason this provider exists, and an agent idle overnight should spend
/// none of them. The control plane wakes a suspended codespace when the
/// session is next spoken to, so a suspension costs a start, not a budget.
const IDLE_TIMEOUT_MINUTES: u32 = 30;

/// Minutes after suspension before GitHub deletes a codespace.
///
/// The maximum, because the disk *is* the session's working tree and the
/// only thing standing between an idle session and a lost one is this
/// number.
const RETENTION_MINUTES: u32 = 43_200;

/// The scope the token must carry for any of this to work.
const CODESPACE_SCOPE: &str = "codespace";

/// The header GitHub reports an OAuth token's granted scopes in.
const SCOPES_HEADER: &str = "x-oauth-scopes";

/// The codespace states that mean "coming up".
const IN_FLIGHT_STATES: [&str; 8] = [
    "Created",
    "Queued",
    "Provisioning",
    "Awaiting",
    "Starting",
    "ShuttingDown",
    "Updating",
    "Rebuilding",
];

/// The codespace states that mean "exists but not running".
const STOPPED_STATES: [&str; 2] = ["Shutdown", "Unavailable"];

/// The codespace states that mean "gone".
const GONE_STATES: [&str; 3] = ["Deleted", "Archived", "Moved"];

/// The devcontainer path the environment repository carries.
const DEVCONTAINER_PATH: &str = ".devcontainer/devcontainer.json";

/// A codespace as GitHub reports it, to the fields flyco reads.
#[derive(Debug, Clone, Deserialize)]
pub struct Codespace {
    /// The codespace's generated name — the machine's `native_id`.
    pub name: String,
    /// The lifecycle state GitHub reports.
    pub state: String,
    /// The machine type it is on.
    #[serde(default)]
    pub machine: Option<CodespaceMachine>,
    /// `github.com/codespaces/{name}` — where the account can open it.
    #[serde(default)]
    pub web_url: Option<String>,
}

/// The machine entry inside a codespace, or one row of the machines
/// listing — the same document both places.
#[derive(Debug, Clone, Deserialize)]
pub struct CodespaceMachine {
    /// Provider-native type name, e.g. `standardLinux32gb`.
    pub name: String,
    /// What GitHub calls it in a picker.
    pub display_name: String,
    /// Core count.
    pub cpus: u32,
    /// RAM in bytes.
    pub memory_in_bytes: u64,
    /// Disk in bytes.
    pub storage_in_bytes: u64,
}

/// The answer of `GET /repos/{owner}/{repo}/codespaces/machines`.
#[derive(Debug, Deserialize)]
struct MachineListing {
    /// Every type this repository may start a codespace on.
    machines: Vec<CodespaceMachine>,
}

/// The body of `POST /user/codespaces`.
#[derive(Debug, Serialize)]
struct CreateCodespace<'a> {
    /// The environment repository's numeric id.
    repository_id: u64,
    /// The geographic area, as the `geo` enum spells it.
    geo: &'a str,
    /// The machine type.
    machine: &'a str,
    /// What the codespace calls itself in the user's list.
    display_name: &'a str,
    /// Suspend-on-idle, in minutes.
    idle_timeout_minutes: u32,
    /// Delete-after-suspension, in minutes.
    retention_period_minutes: u32,
    /// The devcontainer that boots the session.
    devcontainer_path: &'a str,
}

/// The body of `PATCH /user/codespaces/{name}`.
#[derive(Debug, Serialize)]
struct UpdateCodespace<'a> {
    /// The machine type to move to, applied at the next start.
    machine: &'a str,
}

/// One item of the billing usage report — the report's fields are
/// `camelCase` where everything else this driver reads is `snake_case`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageItem {
    /// The billed product, e.g. `codespaces`.
    product: String,
    /// What was consumed, after discounts.
    net_amount: f64,
}

/// The answer of `GET /users/{username}/settings/billing/usage`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageReport {
    /// The line items.
    #[serde(default)]
    usage_items: Vec<UsageItem>,
}

/// The `GET /user` answer's plan half.
#[derive(Debug, Clone, Deserialize)]
pub struct AccountPlan {
    /// The plan's name: `free`, `pro`, and so on.
    pub name: String,
}

/// The `GET /user` answer, to the fields flyco reads.
#[derive(Debug, Clone, Deserialize)]
pub struct CodespacesUser {
    /// The account's immutable numeric id.
    pub id: i64,
    /// The account's login.
    pub login: String,
    /// What the account is subscribed to.
    #[serde(default)]
    pub plan: Option<AccountPlan>,
}

/// The environment repository, once ensured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvRepo {
    /// Its numeric id — what a create call names it by.
    pub id: u64,
    /// `owner/name`.
    pub full_name: String,
}

/// A repository, as `GET /repos/{slug}` or `POST /user/repos` reports it.
#[derive(Debug, Deserialize)]
struct Repository {
    /// Numeric id.
    id: u64,
    /// `owner/name`.
    full_name: String,
    /// Whether it is private.
    #[serde(default)]
    private: bool,
}

/// The body of `POST /user/repos`.
#[derive(Debug, Serialize)]
struct CreateRepo<'a> {
    /// The repository name.
    name: &'a str,
    /// Always `true`: a codespace on a public repository is one other
    /// people could open, and this repository exists to run sessions.
    private: bool,
    /// Creates the first commit, which a codespace build needs.
    auto_init: bool,
    /// What the repository is for, on its own page.
    description: &'a str,
}

/// The body of `PUT /repos/{slug}/contents/{path}`.
#[derive(Debug, Serialize)]
struct PutContents<'a> {
    /// The commit message.
    message: &'a str,
    /// Base64 file content.
    content: &'a str,
}

/// Where one provision is, carried across invocations.
///
/// The codespace name is the whole of it: a create that answered is a
/// codespace that exists, and continuing means polling it again.
#[derive(Debug, Serialize, Deserialize)]
struct ContinuationState {
    /// The codespace's generated name.
    codespace: String,
    /// The geography the request asked for, so the machine row keeps the
    /// name the user picked rather than the one the object reports.
    region: String,
}

/// What `verify` proved about the credential.
#[derive(Debug)]
pub struct VerifiedAccount {
    /// The GitHub account the token belongs to.
    pub user: CodespacesUser,
}

/// The Codespaces driver.
///
/// Generic over its transport and timer so the whole of it is testable
/// against recorded exchanges.
#[derive(Debug)]
pub struct CodespacesProvider<T = LiveTransport, K = SystemTimer> {
    transport: T,
    timer: K,
    token: String,
    env_repo: String,
    env_repo_id: u64,
    included_core_hours: u32,
}

impl CodespacesProvider {
    /// The driver as it is deployed: the live transport and a real timer.
    #[must_use]
    pub fn new(token: &str, env_repo: &str, env_repo_id: u64, included_core_hours: u32) -> Self {
        Self::with_parts(
            LiveTransport::new(),
            SystemTimer::new(),
            token,
            env_repo,
            env_repo_id,
            included_core_hours,
        )
    }
}

/// The monthly core-hour grant an account's plan carries.
///
/// GitHub Free is 120 core-hours and Pro is 180; anything GitHub does not
/// call `pro` reads as the free allowance, which is the number a wrong
/// answer should err toward — overstating a grant would let a budget plan
/// on credit that is not there.
#[must_use]
pub const fn included_core_hours(plan: Option<&str>) -> u32 {
    match plan {
        Some(name) if name.eq_ignore_ascii_case("pro") => 180,
        _ => 120,
    }
}

/// The `.devcontainer/devcontainer.json` the environment repository
/// carries, as a serde shape rather than a string: a field renamed or
/// dropped fails the build instead of silently emitting a document GitHub
/// ignores.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Devcontainer<'a> {
    /// The session image — `flycod` and the agents — pinned to the wire
    /// version this control plane serves so a codespace and its control
    /// plane always speak the same protocol.
    image: &'a str,
    /// The one fact a codespace cannot learn from anything GitHub injects
    /// into it: which deployment's control plane to ask for its
    /// configuration.
    container_env: DevcontainerEnv<'a>,
    /// The entrypoint GitHub runs each time the codespace starts.
    post_start_command: &'static str,
    /// `flyco`, the image's own user — the daemon and the checkout both
    /// belong to it.
    remote_user: &'static str,
}

/// The environment a codespace's lifecycle commands run with.
#[derive(Serialize)]
struct DevcontainerEnv<'a> {
    /// Where `flycod codespace` asks for this session's configuration.
    #[serde(rename = "FLYCO_CONTROL_PLANE")]
    control_plane: &'a str,
}

/// The document [`ensure_environment`] writes into the repository.
///
/// Rendered at link time rather than kept as a fixture: it bakes in the
/// deployment's control-plane URL, which is the control plane's fact
/// rather than the driver's, so the caller passes it in.
///
/// # Panics
///
/// Never: a devcontainer is a flat document and always serializes.
#[must_use]
pub fn devcontainer_json(control_plane: &str) -> String {
    serde_json::to_string_pretty(&Devcontainer {
        image: &flyco_core::release::session_image_for_wire_protocol(),
        container_env: DevcontainerEnv { control_plane },
        post_start_command: "flycod codespace",
        remote_user: "flyco",
    })
    .expect("a devcontainer is a flat document and always serializes")
}

/// Base64 of the environment repository's `devcontainer.json`.
///
/// The document itself is assembled by the control plane at link time —
/// it bakes in this deployment's control-plane URL — and this constant is
/// the encoding the contents API takes rather than the document: building
/// JSON by hand is what the [`serde`] types above are for, and the caller
/// encodes.
#[must_use]
pub fn devcontainer_b64(document: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(document.as_bytes())
}

impl<T: HttpTransport, K: Timer> CodespacesProvider<T, K> {
    /// The driver over an explicit transport and timer.
    pub fn with_parts(
        transport: T,
        timer: K,
        token: &str,
        env_repo: &str,
        env_repo_id: u64,
        included_core_hours: u32,
    ) -> Self {
        Self {
            transport,
            timer,
            token: token.to_owned(),
            env_repo: env_repo.to_owned(),
            env_repo_id,
            included_core_hours,
        }
    }

    /// The transport this driver sends through, so a test can read back
    /// exactly what was put on the wire.
    pub const fn transport(&self) -> &T {
        &self.transport
    }

    /// The timer this driver waits on, for the same reason.
    pub const fn timer(&self) -> &K {
        &self.timer
    }

    /// One authenticated request, with the headers GitHub requires.
    fn request(&self, method: Method, url: String) -> HttpRequest {
        HttpRequest::new(method, url)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", API_VERSION)
            .header("user-agent", USER_AGENT)
            .bearer(&self.token)
    }

    /// Sends one authenticated request.
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, ProviderError> {
        Ok(self.transport.send(request).await?)
    }

    /// Sends one request, refuses anything but a success, and decodes what
    /// came back.
    async fn fetch<R: serde::de::DeserializeOwned>(
        &self,
        request: HttpRequest,
    ) -> Result<R, ProviderError> {
        let response = self.send(request).await?;
        if !response.is_success() {
            return Err(refusal(&response));
        }
        Ok(response.json()?)
    }

    /// `GET` one URL.
    async fn get<R: serde::de::DeserializeOwned>(&self, url: String) -> Result<R, ProviderError> {
        self.fetch(self.request(Method::Get, url)).await
    }

    /// The codespace under this name, or `None` when GitHub no longer has
    /// it.
    ///
    /// A 404 is an answer rather than a failure here: a codespace past its
    /// retention period is deleted outright, and the reconcile path asks
    /// exactly this question.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] on any refusal that is not a missing
    /// codespace.
    pub async fn inspect(&self, name: &str) -> Result<Option<Codespace>, ProviderError> {
        let url = format!("{API}/user/codespaces/{name}");
        let response = self.send(self.request(Method::Get, url)).await?;
        if response.status == 404 {
            return Ok(None);
        }
        if !response.is_success() {
            return Err(refusal(&response));
        }
        Ok(Some(response.json()?))
    }

    /// Proves the credential works and the environment repository is still
    /// what link time left it.
    ///
    /// Three reads, all of them cheap: the account the token belongs to,
    /// whether GitHub says the token carries the `codespace` scope, and
    /// whether the environment repository is still there and still private.
    /// The third is what a verify is *for*: the repository is where every
    /// provision is created, and a user who deleted or published it has an
    /// account that would fail at the first session rather than here.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Rejected`] when the token is refused, when
    /// the `codespace` scope is missing, or when the environment repository
    /// is gone or public.
    pub async fn verify(&self) -> Result<VerifiedAccount, ProviderError> {
        let response = self
            .send(self.request(Method::Get, format!("{API}/user")))
            .await?;
        if !response.is_success() {
            return Err(refusal(&response));
        }
        let user: CodespacesUser = response.json()?;
        let scoped = response.header_value(SCOPES_HEADER).is_some_and(|scopes| {
            scopes
                .split(',')
                .any(|scope| scope.trim().eq_ignore_ascii_case(CODESPACE_SCOPE))
        });
        if !scoped {
            return Err(ProviderError::Rejected(
                "this token was not granted the `codespace` scope; link the account again \
                 so GitHub asks for it"
                    .to_owned(),
            ));
        }

        let repo: Repository = self.get(format!("{API}/repos/{}", self.env_repo)).await?;
        if !repo.private {
            return Err(ProviderError::Rejected(format!(
                "{} is public — a codespace on a public repository can be opened by other \
                 accounts, so flyco will not provision on it",
                self.env_repo
            )));
        }

        Ok(VerifiedAccount { user })
    }

    /// What the provider's own meter says this account has been billed for
    /// Codespaces over the current calendar month.
    ///
    /// `None` is an answer rather than a failure: the usage endpoint is a
    /// feature of GitHub's enhanced billing platform, and an account
    /// without it answers 403 — which is the same silence a GCP project
    /// gives, rather than a reason to fail the usage page.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the read fails for a reason other
    /// than the platform being unavailable to this account.
    pub async fn billing_period_cost(
        &self,
        now_unix: u64,
    ) -> Result<Option<CloudSpend>, ProviderError> {
        let (year, month) = crate::datetime::year_month(now_unix)?;
        let owner = self
            .env_repo
            .split('/')
            .next()
            .ok_or(ProviderError::Malformed(
                "this account's environment repository is not owner/name",
            ))?;
        let url = format!(
            "{API}/users/{owner}/settings/billing/usage?year={year}&month={month}&product=codespaces"
        );
        let response = self.send(self.request(Method::Get, url)).await?;
        if matches!(response.status, 403 | 404) {
            return Ok(None);
        }
        if !response.is_success() {
            return Err(refusal(&response));
        }
        let report: UsageReport = response.json()?;
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a billing line is a positive dollar amount well inside u64 micros"
        )]
        let micros = report
            .usage_items
            .iter()
            .filter(|item| item.product.eq_ignore_ascii_case("codespaces"))
            .map(|item| (item.net_amount * 1_000_000.0) as u64)
            .fold(0_u64, u64::saturating_add);

        let (start, end) = crate::datetime::month_to_date(now_unix)?;
        Ok(Some(CloudSpend {
            period_start_unix: start,
            period_end_unix: end,
            spent: Usd::from_micros(micros),
            remaining_credit: None,
        }))
    }

    /// Reads a codespace until it leaves the in-flight states, or reports
    /// that the polls ran out.
    ///
    /// Codespaces has no operation resource: the codespace object itself is
    /// the thing to poll, and `state` is its progress. `Failed` is the one
    /// terminal state that is an error; `Deleted`, `Archived` and `Moved`
    /// mean the machine ceased to exist mid-operation, which the same
    /// error reports. `Ok(None)` is "still in flight" — the caller decides
    /// whether that is a continuation or an answer.
    async fn await_codespace(
        &self,
        name: &str,
        attempts: usize,
    ) -> Result<Option<Codespace>, ProviderError> {
        for attempt in 0..attempts {
            let Some(codespace) = self.inspect(name).await? else {
                return Err(ProviderError::Gone(format!(
                    "codespace {name} was deleted while it was being waited on"
                )));
            };
            match codespace.state.as_str() {
                state if IN_FLIGHT_STATES.contains(&state) => {
                    self.timer.sleep(poll_delay(None, attempt)).await;
                }
                "Failed" => {
                    return Err(ProviderError::OperationFailed {
                        status: "Failed".to_owned(),
                        code: String::new(),
                        message: format!("codespace {name} failed"),
                    });
                }
                state if GONE_STATES.contains(&state) => {
                    return Err(ProviderError::Gone(format!(
                        "codespace {name} reached {state} while it was being waited on"
                    )));
                }
                // Available, Shutdown, Unavailable and anything unlisted:
                // settled enough for the caller to judge.
                _ => return Ok(Some(codespace)),
            }
        }
        Ok(None)
    }

    /// Turns a codespace into the machine row it backs.
    fn machine(id: MachineId, region: &str, codespace: &Codespace) -> Machine {
        Machine {
            id,
            native_id: codespace.name.clone(),
            runtime: Runtime::Vm,
            region: region.to_owned(),
            state: machine_state(&codespace.state),
            capacity_mode: CapacityMode::OnDemand,
            address: codespace.web_url.clone(),
        }
    }

    /// Drives a created codespace to `Available`, yielding a continuation
    /// when the invocation's polls run out.
    ///
    /// A codespace observed `Shutdown` mid-build is started rather than
    /// failed: creation that partially failed is retried by GitHub in the
    /// background and can surface as stopped, and a start is exactly what
    /// the codespace needs either way.
    async fn poll_build(
        &self,
        machine_id: MachineId,
        region: &str,
        codespace: Codespace,
        attempts: usize,
    ) -> Result<Provisioning, ProviderError> {
        let mut current = codespace;
        for attempt in 0..attempts {
            match current.state.as_str() {
                "Available" => {
                    return Ok(Provisioning::Ready(Self::machine(
                        machine_id, region, &current,
                    )));
                }
                "Failed" => {
                    return Err(ProviderError::OperationFailed {
                        status: "Failed".to_owned(),
                        code: String::new(),
                        message: format!("codespace {} failed to build", current.name),
                    });
                }
                state if GONE_STATES.contains(&state) => {
                    return Err(ProviderError::Gone(format!(
                        "codespace {} reached {state} while it was being created",
                        current.name
                    )));
                }
                state if STOPPED_STATES.contains(&state) => {
                    // Came up stopped — a partial create lands here. Start
                    // it and keep polling.
                    let url = format!("{API}/user/codespaces/{}/start", current.name);
                    let response = self.send(self.request(Method::Post, url)).await?;
                    if response.status == 404 {
                        return Err(ProviderError::Gone(format!(
                            "codespace {} was deleted while it was being created",
                            current.name
                        )));
                    }
                    if !response.is_success() {
                        return Err(refusal(&response));
                    }
                    self.timer.sleep(poll_delay(None, attempt)).await;
                }
                state if IN_FLIGHT_STATES.contains(&state) => {
                    self.timer.sleep(poll_delay(None, attempt)).await;
                }
                other => {
                    return Err(ProviderError::Rejected(format!(
                        "codespace {} reported an unrecognised state {other}",
                        current.name
                    )));
                }
            }
            current = self.inspect(&current.name).await?.ok_or_else(|| {
                ProviderError::Gone(format!(
                    "codespace {} was deleted while it was being created",
                    current.name
                ))
            })?;
        }

        Ok(Provisioning::Pending {
            machine: Self::machine(machine_id, region, &current),
            continuation: Continuation::write(&ContinuationState {
                codespace: current.name,
                region: region.to_owned(),
            })?,
        })
    }
}

/// The lifecycle state GitHub's name maps to, from flyco's point of view.
///
/// `Failed` reads as destroyed rather than in flight: a codespace that
/// failed is dead where it sits — it bills storage until it is deleted,
/// and nothing flyco can send starts it — so the reconcile treats it the
/// way it treats a deleted one, releasing the row after asking GitHub to
/// delete what is left.
#[must_use]
pub fn machine_state(state: &str) -> MachineState {
    if state == "Available" {
        MachineState::Running
    } else if STOPPED_STATES.contains(&state) {
        MachineState::Deallocated
    } else if GONE_STATES.contains(&state) || state == "Failed" {
        MachineState::Destroyed
    } else {
        MachineState::Provisioning
    }
}

/// A GitHub refusal as a provider error.
///
/// The message is GitHub's own, which is the actionable half of every
/// refusal; the error code inside `errors[]` is lifted out when there is
/// one, since that is what a driver can act on.
fn refusal(response: &HttpResponse) -> ProviderError {
    #[derive(Deserialize)]
    struct Body {
        message: String,
        #[serde(default)]
        errors: Vec<ErrorDetail>,
    }
    #[derive(Deserialize)]
    struct ErrorDetail {
        code: Option<String>,
    }

    let status = response.status;
    let body = response.body_text();
    match serde_json::from_str::<Body>(&body) {
        Ok(parsed) => {
            let code = parsed
                .errors
                .iter()
                .find_map(|error| error.code.clone())
                .unwrap_or_else(|| format!("http-{status}"));
            ProviderError::Refused {
                code,
                message: parsed.message,
            }
        }
        Err(_) => ProviderError::Rejected(format!("GitHub answered HTTP {status}")),
    }
}

impl<T: HttpTransport, K: Timer> CloudProvider for CodespacesProvider<T, K> {
    /// Every machine type this account may start, in every geography.
    ///
    /// GitHub's machine list is per repository rather than per region — a
    /// codespace's geography is chosen at create time from the four `geo`
    /// values, not from a region's own catalog — so one read covers the
    /// account and each entry is emitted once per geography.
    async fn catalog(&mut self) -> Result<Vec<MachineCatalogEntry>, ProviderError> {
        let listing: MachineListing = self
            .get(format!("{API}/repos/{}/codespaces/machines", self.env_repo))
            .await?;

        let grant = FreeGrant {
            vcpu_seconds_per_month: u64::from(self.included_core_hours) * 3_600,
            // Codespaces meters core-hours alone — memory is part of the
            // core-hour rather than a second meter — so there is no memory
            // grant to exhaust. `u64::MAX` says "unmetered" rather than
            // claiming a number the provider does not publish.
            gib_seconds_per_month: u64::MAX,
        };
        let mut entries = Vec::with_capacity(listing.machines.len() * GEOS.len());
        for machine in listing.machines {
            for geo in GEOS {
                entries.push(MachineCatalogEntry {
                    provider: CloudProviderKind::Codespaces,
                    account: None,
                    region: geo.to_owned(),
                    machine_type: machine.name.clone(),
                    runtime: Runtime::Vm,
                    free_grant: Some(grant),
                    os: OsFamily::Linux,
                    capacity: Some(MachineCapacity {
                        vcpus: machine.cpus,
                        memory_mib: machine.memory_in_bytes / (1 << 20),
                    }),
                    lineage: Some(MachineLineage {
                        architecture: CpuArchitecture::X8664,
                        family: "codespaces".to_owned(),
                        generation: None,
                    }),
                    pricing: MachinePricing::Metered {
                        on_demand_hourly: Usd::from_micros(
                            CORE_HOUR_MICROS.saturating_mul(u64::from(machine.cpus)),
                        ),
                        spot_hourly: None,
                        minimum: None,
                        storage: StoragePricing::PerGibHourly {
                            rate: Usd::from_micros(STORAGE_GIB_HOUR_MICROS),
                        },
                    },
                });
            }
        }
        Ok(entries)
    }

    /// Creates the codespace and waits for it to come up.
    ///
    /// The daemon configuration is *not* delivered here — GitHub has no
    /// per-codespace secret channel, and a repository-level secret would be
    /// shared by every concurrent session. Instead the codespace itself
    /// fetches its configuration on `postStart`, authenticated by the
    /// `CODESPACE_NAME`/`GITHUB_TOKEN` pair GitHub injects; the control
    /// plane stores what it fetches against the machine row.
    async fn provision(
        &mut self,
        request: &ProvisionRequest,
    ) -> Result<Provisioning, ProviderError> {
        let spec = &request.spec;
        let display_name = format!("flyco-{}", request.machine);
        let body = CreateCodespace {
            repository_id: self.env_repo_id,
            geo: &spec.region,
            machine: &spec.machine_type,
            display_name: &display_name,
            idle_timeout_minutes: IDLE_TIMEOUT_MINUTES,
            retention_period_minutes: RETENTION_MINUTES,
            devcontainer_path: DEVCONTAINER_PATH,
        };
        let request_body = self
            .request(Method::Post, format!("{API}/user/codespaces"))
            .json_body(&body)?;
        // 201 is a created codespace; 202 is one GitHub is still retrying
        // in the background, which the same poll loop waits on.
        let response = self.send(request_body).await?;
        if !matches!(response.status, 201 | 202) {
            return Err(refusal(&response));
        }
        let codespace: Codespace = response.json()?;

        self.poll_build(
            request.machine,
            &spec.region,
            codespace,
            POLLS_PER_INVOCATION,
        )
        .await
    }

    /// Polls a codespace the create already named, under a fresh budget.
    async fn resume(
        &mut self,
        machine: &Machine,
        continuation: &Continuation,
    ) -> Result<Provisioning, ProviderError> {
        let state: ContinuationState = continuation.read()?;
        let codespace = self.inspect(&state.codespace).await?.ok_or_else(|| {
            ProviderError::Gone(format!(
                "codespace {} was deleted before it finished provisioning",
                state.codespace
            ))
        })?;
        self.poll_build(machine.id, &state.region, codespace, MAX_POLL_ATTEMPTS)
            .await
    }

    /// Moves the codespace to another machine type, keeping its disk.
    ///
    /// A `PATCH` applies at the codespace's *next* start, so a resize of a
    /// running codespace is `PATCH` → `stop` → `start`: the disk survives
    /// all three, which is the whole of what a resize promises.
    async fn resize(
        &mut self,
        machine: &Machine,
        new_machine_type: &str,
    ) -> Result<Machine, ProviderError> {
        let url = format!("{API}/user/codespaces/{}", machine.native_id);
        let response = self
            .send(
                self.request(Method::Patch, url)
                    .json_body(&UpdateCodespace {
                        machine: new_machine_type,
                    })?,
            )
            .await?;
        if response.status == 404 {
            return Err(ProviderError::Gone(format!(
                "codespace {} no longer exists",
                machine.native_id
            )));
        }
        if !response.is_success() {
            return Err(refusal(&response));
        }
        let updated: Codespace = response.json()?;

        if machine.state == MachineState::Running {
            self.deallocate(machine).await?;
            return self.start(machine).await;
        }
        Ok(Self::machine(machine.id, &machine.region, &updated))
    }

    /// Stops the codespace, keeping its disk.
    ///
    /// A codespace still stopping when the polls run out is not a failure:
    /// GitHub finishes the stop on its own, and the row's `Deallocated`
    /// records the intent either way.
    async fn deallocate(&mut self, machine: &Machine) -> Result<(), ProviderError> {
        let url = format!("{API}/user/codespaces/{}/stop", machine.native_id);
        let response = self.send(self.request(Method::Post, url)).await?;
        if response.status == 404 {
            return Err(ProviderError::Gone(format!(
                "codespace {} no longer exists",
                machine.native_id
            )));
        }
        if !response.is_success() {
            return Err(refusal(&response));
        }
        let Some(codespace) = self
            .await_codespace(&machine.native_id, POLLS_PER_INVOCATION)
            .await?
        else {
            return Ok(());
        };
        if machine_state(&codespace.state) != MachineState::Deallocated {
            return Err(ProviderError::Rejected(format!(
                "codespace {} stopped as {}, not Shutdown",
                codespace.name, codespace.state
            )));
        }
        Ok(())
    }

    /// Starts a stopped codespace on the disk it kept.
    async fn start(&mut self, machine: &Machine) -> Result<Machine, ProviderError> {
        let url = format!("{API}/user/codespaces/{}/start", machine.native_id);
        let response = self.send(self.request(Method::Post, url)).await?;
        if response.status == 404 {
            return Err(ProviderError::Gone(format!(
                "codespace {} no longer exists",
                machine.native_id
            )));
        }
        if !response.is_success() {
            return Err(refusal(&response));
        }
        let Some(codespace) = self
            .await_codespace(&machine.native_id, POLLS_PER_INVOCATION)
            .await?
        else {
            return Err(ProviderError::Rejected(format!(
                "codespace {} was still starting after {POLLS_PER_INVOCATION} polls",
                machine.native_id
            )));
        };
        if machine_state(&codespace.state) != MachineState::Running {
            return Err(ProviderError::Rejected(format!(
                "codespace {} started as {}, not Available",
                codespace.name, codespace.state
            )));
        }
        Ok(Self::machine(machine.id, &machine.region, &codespace))
    }

    /// Deletes the codespace, disk included.
    ///
    /// A 404 is a success: retention deletes a suspended codespace without
    /// asking, and a delete that finds nothing has achieved what it asked.
    async fn destroy(&mut self, machine: &Machine) -> Result<(), ProviderError> {
        let url = format!("{API}/user/codespaces/{}", machine.native_id);
        let response = self.send(self.request(Method::Delete, url)).await?;
        if response.status == 404 {
            return Ok(());
        }
        if !response.is_success() {
            return Err(refusal(&response));
        }
        Ok(())
    }
}

/// Ensures the account holds the private environment repository, creating
/// it and its devcontainer when it does not.
///
/// Link-time work rather than lifecycle work, and on a free function for
/// exactly that reason: the credentials this would hang off are the ones
/// the link is still building — there is no `CodespacesProvider` yet, only
/// a token. The devcontainer document is a parameter for the same reason:
/// it bakes in the deployment's control-plane URL, which is the control
/// plane's fact rather than the driver's.
///
/// Existing repositories are respected rather than replaced: a repository
/// the user already has under this name is verified private and its
/// devcontainer rewritten, because a stale devcontainer is a session that
/// boots wrong, not a customization to preserve.
///
/// # Errors
///
/// Returns [`ProviderError`] when the account cannot be read, the
/// repository cannot be created or written, or an existing repository of
/// this name is public.
pub async fn ensure_environment<T: HttpTransport>(
    transport: &T,
    token: &str,
    owner: &str,
    devcontainer_json: &str,
) -> Result<EnvRepo, ProviderError> {
    let request = |method: Method, url: String| {
        HttpRequest::new(method, url)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", API_VERSION)
            .header("user-agent", USER_AGENT)
            .bearer(token)
    };

    let slug = format!("{owner}/{ENV_REPO_NAME}");
    let repo_url = format!("{API}/repos/{slug}");
    let response = transport.send(request(Method::Get, repo_url)).await?;
    let repo: Repository = if response.status == 404 {
        let created = transport
            .send(
                request(Method::Post, format!("{API}/user/repos")).json_body(&CreateRepo {
                    name: ENV_REPO_NAME,
                    private: true,
                    auto_init: true,
                    description: "Where flyco sessions boot their Codespaces",
                })?,
            )
            .await?;
        if !created.is_success() {
            return Err(refusal(&created));
        }
        created.json()?
    } else {
        if !response.is_success() {
            return Err(refusal(&response));
        }
        response.json()?
    };

    if !repo.private {
        return Err(ProviderError::Rejected(format!(
            "{slug} is public — a codespace on a public repository can be opened by other \
             accounts, so flyco will not provision on it"
        )));
    }
    if repo.full_name != slug {
        return Err(ProviderError::Rejected(format!(
            "{} answered for {slug} — the repository was renamed or transferred; \
             link the account again",
            repo.full_name
        )));
    }

    // The devcontainer is always rewritten: it pins the session image and
    // the control-plane URL, and a stale one is a codespace that boots
    // without its daemon.
    let put = transport
        .send(
            request(
                Method::Put,
                format!("{API}/repos/{slug}/contents/{DEVCONTAINER_PATH}"),
            )
            .json_body(&PutContents {
                message: "Boot flyco sessions",
                content: &devcontainer_b64(devcontainer_json),
            })?,
        )
        .await?;
    if !put.is_success() {
        return Err(refusal(&put));
    }

    Ok(EnvRepo {
        id: repo.id,
        full_name: repo.full_name,
    })
}
