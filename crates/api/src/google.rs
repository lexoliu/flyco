//! The Google side of "Sign in with Google".
//!
//! What linking GCP used to ask of a user — create a service account, grant
//! it `compute.admin`, download its JSON key, paste the key in — this module
//! does for them from one consent screen. The user's own grant is what makes
//! it possible: `cloud-platform` is the scope the Cloud Console itself acts
//! under, so flyco can create the service account, edit the project's IAM
//! policy, and mint the key that becomes
//! [`ProviderCredentials::Gcp`](flyco_core::ProviderCredentials::Gcp).
//!
//! The user's own token is deliberately short-lived and never stored: the
//! authorize URL asks for `access_type=online`, so no refresh token exists
//! at all. What outlives the sign-in is the service account key, which is
//! the credential flyco was after — the user's grant is spent creating it
//! and then thrown away with the attempt.
//!
//! Everything that leaves the control plane for Google goes through an
//! [`HttpTransport`](flyco_provider::HttpTransport), which is zenwave in
//! production and a table of recorded exchanges under test.

use core::future::Future;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use flyco_core::ProviderOauthChoice;
use flyco_provider::http::{HttpRequest, HttpResponse, Method};
use flyco_provider::{HttpError, HttpTransport, LiveTransport};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::jwt;

/// Where the browser approves the grant.
const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";

/// Where the authorization code is redeemed.
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Where the projects a signed-in account can see are listed.
const PROJECTS_URL: &str = "https://cloudresourcemanager.googleapis.com/v1/projects";

/// What the user consents to.
///
/// `cloud-platform` is what the Cloud Console acts under, and it is what
/// creating a service account, editing an IAM policy and minting a key all
/// need; `userinfo.email` is what names the account on the card.
pub const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform \
                         https://www.googleapis.com/auth/userinfo.email";

/// The local part of the service account flyco creates, and its display
/// name.
const SERVICE_ACCOUNT_ID: &str = "flyco";

/// What the created service account is granted.
///
/// The role a machine's whole lifecycle needs — create, start, stop, resize,
/// destroy — and nothing beyond the project it is granted in.
const COMPUTE_ADMIN_ROLE: &str = "roles/compute.admin";

/// How a member names a service account in an IAM binding.
const SERVICE_ACCOUNT_MEMBER_PREFIX: &str = "serviceAccount:";

/// The key format Google issues a downloadable JSON credential in.
const GOOGLE_CREDENTIALS_FILE: &str = "TYPE_GOOGLE_CREDENTIALS_FILE";

/// The project state flyco can provision into.
const ACTIVE: &str = "ACTIVE";

/// What Google answers when the service account is already there.
const ALREADY_EXISTS: u16 = 409;

/// What a refusal says when Google states no description.
const NO_DESCRIPTION: &str = "no description";

/// The OAuth client one deployment presents to Google.
#[derive(Debug, Clone, Copy)]
pub struct OauthClient<'a> {
    /// Client id of flyco's own registered application.
    pub id: &'a str,
    /// Client secret issued for it.
    pub secret: &'a str,
}

/// Everything the callback learned from one Google sign-in.
#[derive(Clone)]
pub struct SignIn {
    /// The address that signed in.
    pub account: String,
    /// The projects that account may link.
    pub choices: Vec<ProviderOauthChoice>,
    /// The user's own access token, which the finish spends and discards.
    pub access_token: String,
}

impl core::fmt::Debug for SignIn {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SignIn")
            .field("account", &self.account)
            .field("choices", &self.choices)
            .finish_non_exhaustive()
    }
}

/// Why a call to Google did not produce what flyco asked for.
#[derive(Debug, thiserror::Error)]
pub enum GoogleError {
    /// The request never completed, or the response was not the expected
    /// JSON.
    #[error("Google request failed: {0}")]
    Transport(String),
    /// Google refused, and said why.
    #[error("{status}: {message}")]
    Rejected {
        /// Google's machine-readable status, such as `PERMISSION_DENIED`.
        status: String,
        /// Google's human-readable explanation.
        message: String,
    },
    /// Google answered with something flyco cannot use.
    #[error("Google answered with something flyco cannot use: {0}")]
    Malformed(&'static str),
    /// Google answered with a status flyco cannot interpret.
    #[error("Google responded with HTTP {0}")]
    Status(u16),
}

impl From<HttpError> for GoogleError {
    fn from(error: HttpError) -> Self {
        Self::Transport(error.to_string())
    }
}

/// The URL the browser opens to approve the grant.
///
/// # Panics
///
/// Panics if [`AUTHORIZE_URL`] is not an absolute URL, which would mean this
/// module's own constant was edited into something that is not one.
#[must_use]
pub fn authorize_url(client_id: &str, redirect_uri: &str, state: &str) -> Url {
    let mut url = Url::parse(AUTHORIZE_URL).expect("the Google authorize URL is absolute");
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", SCOPE)
        .append_pair("access_type", "online")
        .append_pair("prompt", "select_account")
        .append_pair("state", state);
    url
}

/// The OAuth error document the token endpoint returns.
#[derive(Debug, Deserialize)]
struct OauthError {
    error: String,
    error_description: Option<String>,
}

/// The error document every Google API returns.
#[derive(Debug, Deserialize)]
struct GoogleErrorDocument {
    error: GoogleErrorBody,
}

/// Its one member.
#[derive(Debug, Deserialize)]
struct GoogleErrorBody {
    message: String,
    #[serde(default)]
    status: Option<String>,
}

/// Turns a refusal into the reason Google gave for it.
///
/// Two documents, because Google has two: the token endpoint speaks
/// RFC 6749's `{error, error_description}` and every API speaks
/// `{error: {message, status}}`. The OAuth shape is tried first because its
/// `error` is a string, which the other document's object cannot be read as.
fn refusal(response: &HttpResponse) -> GoogleError {
    if let Ok(oauth) = response.json::<OauthError>() {
        return GoogleError::Rejected {
            status: oauth.error,
            message: oauth
                .error_description
                .unwrap_or_else(|| NO_DESCRIPTION.to_owned()),
        };
    }
    response.json::<GoogleErrorDocument>().map_or_else(
        |_| GoogleError::Status(response.status),
        |document| GoogleError::Rejected {
            status: document
                .error
                .status
                .unwrap_or_else(|| response.status.to_string()),
            message: document.error.message,
        },
    )
}

/// Reads a JSON body, or reports why the call failed.
fn decoded<T: serde::de::DeserializeOwned>(response: &HttpResponse) -> Result<T, GoogleError> {
    if response.is_success() {
        return response
            .json::<T>()
            .map_err(|error| GoogleError::Transport(error.to_string()));
    }
    Err(refusal(response))
}

/// What the token endpoint answers with.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    id_token: Option<String>,
}

/// The one claim flyco reads out of a Google id token.
#[derive(Debug, Default, Deserialize)]
struct IdClaims {
    #[serde(default)]
    email: Option<String>,
}

/// One project, as Cloud Resource Manager lists it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectRow {
    project_id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    lifecycle_state: Option<String>,
}

/// The project listing.
#[derive(Debug, Deserialize)]
struct ProjectList {
    #[serde(default)]
    projects: Vec<ProjectRow>,
}

/// Body of the service-account creation.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct NewServiceAccount<'a> {
    account_id: &'a str,
    service_account: ServiceAccountName<'a>,
}

/// Its one member.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServiceAccountName<'a> {
    display_name: &'a str,
}

/// What IAM answers with when it created one.
#[derive(Debug, Deserialize)]
struct ServiceAccount {
    email: String,
}

/// One project's IAM policy, as `getIamPolicy` returns it.
///
/// Everything but the bindings rides in [`other`](Self::other) and is
/// written back untouched. That is what keeps `etag` — the optimistic lock
/// that makes a read-modify-write safe — and `auditConfigs`, which a policy
/// that dropped it would silently switch off.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct IamPolicy {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    bindings: Vec<IamBinding>,
    #[serde(flatten)]
    other: serde_json::Map<String, serde_json::Value>,
}

/// One `role → members` pair of a policy.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct IamBinding {
    role: String,
    #[serde(default)]
    members: Vec<String>,
    /// A binding's `condition`, and anything else Google adds, carried
    /// through untouched — and read, because a *conditional* binding is not
    /// one flyco may add an unconditional member to.
    #[serde(flatten)]
    other: serde_json::Map<String, serde_json::Value>,
}

/// Body of `getIamPolicy`, which takes no options.
#[derive(Debug, Serialize)]
struct GetIamPolicy {}

/// Body of `setIamPolicy`.
#[derive(Debug, Serialize)]
struct SetIamPolicy<'a> {
    policy: &'a IamPolicy,
}

/// Body of the key creation.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct NewKey<'a> {
    private_key_type: &'a str,
}

/// What IAM answers with when it minted one.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServiceAccountKey {
    /// The whole JSON key document, base64-encoded.
    private_key_data: String,
}

/// Where a project's service accounts live.
fn service_accounts_url(project_id: &str) -> String {
    format!("https://iam.googleapis.com/v1/projects/{project_id}/serviceAccounts")
}

/// Where one service account's keys live.
fn service_account_keys_url(project_id: &str, email: &str) -> String {
    format!("https://iam.googleapis.com/v1/projects/{project_id}/serviceAccounts/{email}/keys")
}

/// Where a project's IAM policy is read.
fn get_iam_policy_url(project_id: &str) -> String {
    format!("{PROJECTS_URL}/{project_id}:getIamPolicy")
}

/// Where a project's IAM policy is written.
fn set_iam_policy_url(project_id: &str) -> String {
    format!("{PROJECTS_URL}/{project_id}:setIamPolicy")
}

/// The address a service account named [`SERVICE_ACCOUNT_ID`] has in
/// `project_id`.
///
/// Google derives it rather than choosing it, so an account that already
/// exists can be addressed without listing anything.
fn service_account_email(project_id: &str) -> String {
    format!("{SERVICE_ACCOUNT_ID}@{project_id}.iam.gserviceaccount.com")
}

/// How that account is named as a policy member.
fn service_account_member(email: &str) -> String {
    format!("{SERVICE_ACCOUNT_MEMBER_PREFIX}{email}")
}

/// Redeems the authorization code.
///
/// # Errors
///
/// Returns [`GoogleError`] if the exchange fails, Google refuses the grant,
/// or the answer carries no id token — without which the account has no
/// name.
async fn exchange_over<T: HttpTransport>(
    transport: &T,
    client: OauthClient<'_>,
    code: &str,
    redirect_uri: &str,
) -> Result<(String, String), GoogleError> {
    let response = transport
        .send(
            HttpRequest::new(Method::Post, TOKEN_URL)
                .header("accept", "application/json")
                .form_body(&[
                    ("code", code),
                    ("client_id", client.id),
                    ("client_secret", client.secret),
                    ("redirect_uri", redirect_uri),
                    ("grant_type", "authorization_code"),
                ]),
        )
        .await?;
    let tokens: TokenResponse = decoded(&response)?;
    let id_token = tokens.id_token.ok_or(GoogleError::Malformed(
        "the grant carries no id token, so the account has no name",
    ))?;
    Ok((tokens.access_token, id_token))
}

/// Lists the active projects the token can see.
///
/// A project being deleted is not a choice: nothing can be provisioned into
/// it, so offering it would be offering a link that cannot work.
///
/// # Errors
///
/// Returns [`GoogleError`] if the call fails or Google refuses it.
pub async fn projects_over<T: HttpTransport>(
    transport: &T,
    token: &str,
) -> Result<Vec<ProviderOauthChoice>, GoogleError> {
    let response = transport
        .send(
            HttpRequest::new(Method::Get, PROJECTS_URL)
                .header("accept", "application/json")
                .bearer(token),
        )
        .await?;
    let listed: ProjectList = decoded(&response)?;

    Ok(listed
        .projects
        .into_iter()
        .filter(|row| row.lifecycle_state.as_deref() == Some(ACTIVE))
        .map(|row| ProviderOauthChoice {
            // Google lets a project carry no display name, and then its id
            // is the only name it has.
            name: row.name.clone().unwrap_or_else(|| row.project_id.clone()),
            id: row.project_id,
        })
        .collect())
}

/// Runs the whole callback conversation: redeem, read, list.
///
/// # Errors
///
/// Returns [`GoogleError`] if any of the three steps fails.
pub async fn sign_in_over<T: HttpTransport>(
    transport: &T,
    client: OauthClient<'_>,
    code: &str,
    redirect_uri: &str,
) -> Result<SignIn, GoogleError> {
    let (access_token, id_token) = exchange_over(transport, client, code, redirect_uri).await?;

    let claims: IdClaims =
        jwt::claims(&id_token).map_err(|error| GoogleError::Malformed(error.detail()))?;
    let account = claims
        .email
        .ok_or(GoogleError::Malformed("the id token names no address"))?;

    let choices = projects_over(transport, &access_token).await?;
    Ok(SignIn {
        account,
        choices,
        access_token,
    })
}

/// Creates the service account flyco will provision with, grants it
/// `compute.admin` on `project_id`, and mints its key.
///
/// Answers with the JSON key document itself, which is exactly what
/// [`ProviderCredentials::Gcp`](flyco_core::ProviderCredentials::Gcp)
/// carries and what the user would otherwise have downloaded by hand.
///
/// # Errors
///
/// Returns [`GoogleError`] if any call fails, or if the minted key is not
/// the base64 of a UTF-8 document.
pub async fn create_identity_over<T: HttpTransport>(
    transport: &T,
    token: &str,
    project_id: &str,
) -> Result<String, GoogleError> {
    let email = ensure_service_account(transport, token, project_id).await?;
    grant_compute_admin(transport, token, project_id, &email).await?;
    mint_key(transport, token, project_id, &email).await
}

/// Creates the service account, or names the one that is already there.
///
/// A second link of the same project must not fail on an account flyco
/// itself created the first time, and Google derives the address rather than
/// choosing it — so a `409` is the answer, not an error.
async fn ensure_service_account<T: HttpTransport>(
    transport: &T,
    token: &str,
    project_id: &str,
) -> Result<String, GoogleError> {
    let response = transport
        .send(
            HttpRequest::new(Method::Post, service_accounts_url(project_id))
                .header("accept", "application/json")
                .bearer(token)
                .json_body(&NewServiceAccount {
                    account_id: SERVICE_ACCOUNT_ID,
                    service_account: ServiceAccountName {
                        display_name: SERVICE_ACCOUNT_ID,
                    },
                })?,
        )
        .await?;

    if response.status == ALREADY_EXISTS {
        tracing::debug!(
            project = project_id,
            "reusing the service account flyco already made"
        );
        return Ok(service_account_email(project_id));
    }
    let created: ServiceAccount = decoded(&response)?;
    Ok(created.email)
}

/// Adds the service account to the project's `compute.admin` binding.
async fn grant_compute_admin<T: HttpTransport>(
    transport: &T,
    token: &str,
    project_id: &str,
    email: &str,
) -> Result<(), GoogleError> {
    let mut policy: IamPolicy = decoded(
        &transport
            .send(
                HttpRequest::new(Method::Post, get_iam_policy_url(project_id))
                    .header("accept", "application/json")
                    .bearer(token)
                    .json_body(&GetIamPolicy {})?,
            )
            .await?,
    )?;

    let member = service_account_member(email);
    // An unconditional binding only: adding a member to a *conditional*
    // `compute.admin` would grant something narrower than the role reads,
    // so a project that has one gets its own unconditional binding beside
    // it rather than a silently-limited grant.
    let existing = policy
        .bindings
        .iter_mut()
        .find(|binding| binding.role == COMPUTE_ADMIN_ROLE && binding.other.is_empty());

    match existing {
        Some(binding) => {
            if binding.members.iter().any(|held| held == &member) {
                tracing::debug!(
                    project = project_id,
                    "the service account already holds the role"
                );
                return Ok(());
            }
            binding.members.push(member);
        }
        None => policy.bindings.push(IamBinding {
            role: COMPUTE_ADMIN_ROLE.to_owned(),
            members: vec![member],
            other: serde_json::Map::new(),
        }),
    }

    let response = transport
        .send(
            HttpRequest::new(Method::Post, set_iam_policy_url(project_id))
                .header("accept", "application/json")
                .bearer(token)
                .json_body(&SetIamPolicy { policy: &policy })?,
        )
        .await?;
    if response.is_success() {
        return Ok(());
    }
    Err(refusal(&response))
}

/// Mints the service account's JSON key.
async fn mint_key<T: HttpTransport>(
    transport: &T,
    token: &str,
    project_id: &str,
    email: &str,
) -> Result<String, GoogleError> {
    let key: ServiceAccountKey = decoded(
        &transport
            .send(
                HttpRequest::new(Method::Post, service_account_keys_url(project_id, email))
                    .header("accept", "application/json")
                    .bearer(token)
                    .json_body(&NewKey {
                        private_key_type: GOOGLE_CREDENTIALS_FILE,
                    })?,
            )
            .await?,
    )?;

    let decoded_key = BASE64
        .decode(key.private_key_data)
        .map_err(|_| GoogleError::Malformed("the minted key is not base64"))?;
    String::from_utf8(decoded_key)
        .map_err(|_| GoogleError::Malformed("the minted key is not a UTF-8 document"))
}

/// The two conversations the Google sign-in has.
///
/// Behind a trait for the same reason [`crate::microsoft::MicrosoftOauth`]
/// is: the happy path is otherwise untestable, because a route handler
/// cannot be exercised without standing in for `accounts.google.com`.
pub trait GoogleOauth: Send + Sync + Clone + 'static {
    /// Redeems the code and reports who signed in and what they may link.
    ///
    /// # Errors
    ///
    /// Returns [`GoogleError`] if any step of the exchange fails.
    fn sign_in(
        &self,
        client: OauthClient<'_>,
        code: &str,
        redirect_uri: &str,
    ) -> impl Future<Output = Result<SignIn, GoogleError>> + Send;

    /// Creates the service account flyco provisions with, and mints its key.
    ///
    /// # Errors
    ///
    /// Returns [`GoogleError`] if any step of the creation fails.
    fn create_identity(
        &self,
        token: &str,
        project_id: &str,
    ) -> impl Future<Output = Result<String, GoogleError>> + Send;
}

/// The production client, speaking HTTP through zenwave — hyper natively,
/// Fetch inside the Worker.
#[derive(Debug, Clone, Copy, Default)]
pub struct ZenwaveGoogle {
    transport: LiveTransport,
}

impl ZenwaveGoogle {
    /// Creates the client.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            transport: LiveTransport::new(),
        }
    }
}

impl GoogleOauth for ZenwaveGoogle {
    async fn sign_in(
        &self,
        client: OauthClient<'_>,
        code: &str,
        redirect_uri: &str,
    ) -> Result<SignIn, GoogleError> {
        sign_in_over(&self.transport, client, code, redirect_uri).await
    }

    async fn create_identity(&self, token: &str, project_id: &str) -> Result<String, GoogleError> {
        create_identity_over(&self.transport, token, project_id).await
    }
}

/// The Google client the router carries.
///
/// An enum rather than a type parameter for the reason
/// [`GithubClient`](crate::github::GithubClient) is one: `#[skyzen::openapi]`
/// cannot annotate a generic handler.
#[derive(Debug, Clone)]
pub enum GoogleClient {
    /// Talks to `accounts.google.com`, `cloudresourcemanager.googleapis.com`
    /// and `iam.googleapis.com`.
    Live(ZenwaveGoogle),
    /// Answers from fixtures, for tests.
    #[cfg(test)]
    Fake(crate::testing::TestGoogle),
}

impl Default for GoogleClient {
    fn default() -> Self {
        Self::Live(ZenwaveGoogle::new())
    }
}

impl GoogleOauth for GoogleClient {
    async fn sign_in(
        &self,
        client: OauthClient<'_>,
        code: &str,
        redirect_uri: &str,
    ) -> Result<SignIn, GoogleError> {
        match self {
            Self::Live(live) => live.sign_in(client, code, redirect_uri).await,
            #[cfg(test)]
            Self::Fake(fake) => fake.sign_in(client, code, redirect_uri).await,
        }
    }

    async fn create_identity(&self, token: &str, project_id: &str) -> Result<String, GoogleError> {
        match self {
            Self::Live(live) => live.create_identity(token, project_id).await,
            #[cfg(test)]
            Self::Fake(fake) => fake.create_identity(token, project_id).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use flyco_provider::http::HttpResponse;
    use flyco_provider::testing::RecordedTransport;

    use super::{
        GoogleError, OauthClient, SCOPE, authorize_url, create_identity_over, projects_over,
        sign_in_over,
    };

    /// The grant, as the token endpoint returns one.
    const TOKEN_BODY: &str = include_str!("../fixtures/google/token.json");
    /// Two projects, one of them being deleted.
    const PROJECTS_BODY: &str = include_str!("../fixtures/google/projects.json");
    /// The created service account.
    const SERVICE_ACCOUNT_BODY: &str = include_str!("../fixtures/google/service_account.json");
    /// Google refusing to create one that is already there.
    const ALREADY_EXISTS_BODY: &str =
        include_str!("../fixtures/google/service_account_exists.json");
    /// A policy that already grants `compute.admin` to somebody.
    const POLICY_BODY: &str = include_str!("../fixtures/google/iam_policy.json");
    /// A policy with no `compute.admin` binding at all.
    const POLICY_WITHOUT_ROLE_BODY: &str =
        include_str!("../fixtures/google/iam_policy_without_compute_admin.json");
    /// The minted key.
    const KEY_BODY: &str = include_str!("../fixtures/google/service_account_key.json");
    /// Google refusing the call outright.
    const FORBIDDEN_BODY: &str = include_str!("../fixtures/google/forbidden.json");

    const CLIENT: OauthClient<'static> = OauthClient {
        id: "the-client-id.apps.googleusercontent.com",
        secret: "the-client-secret",
    };

    const REDIRECT_URI: &str = "https://flyco.test/v1/providers/gcp/oauth/callback";

    const PROJECT: &str = "flyco-dev-4821";

    const EMAIL: &str = "flyco@flyco-dev-4821.iam.gserviceaccount.com";

    fn query(url: &url::Url, name: &str) -> String {
        url.query_pairs()
            .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
            .unwrap_or_else(|| panic!("the authorize URL carries `{name}`"))
    }

    fn field(body: &str, name: &str) -> String {
        url::form_urlencoded::parse(body.as_bytes())
            .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
            .unwrap_or_else(|| panic!("the form body carries `{name}`"))
    }

    fn json(body: &str) -> serde_json::Value {
        serde_json::from_str(body).expect("a JSON body")
    }

    #[test]
    fn the_authorize_url_asks_for_the_console_scope_and_no_refresh_token() {
        let url = authorize_url(CLIENT.id, REDIRECT_URI, "the-state");

        assert_eq!(url.host_str(), Some("accounts.google.com"));
        assert_eq!(url.path(), "/o/oauth2/v2/auth");
        assert_eq!(query(&url, "client_id"), CLIENT.id);
        assert_eq!(query(&url, "redirect_uri"), REDIRECT_URI);
        assert_eq!(query(&url, "response_type"), "code");
        assert_eq!(query(&url, "scope"), SCOPE);
        assert_eq!(query(&url, "prompt"), "select_account");
        assert_eq!(query(&url, "state"), "the-state");
        assert_eq!(
            query(&url, "access_type"),
            "online",
            "the user's grant is spent creating the key and never stored"
        );
    }

    #[skyzen::test]
    async fn a_sign_in_redeems_the_code_reads_the_address_and_lists_active_projects() {
        let transport = RecordedTransport::new(vec![
            HttpResponse::new(200, TOKEN_BODY),
            HttpResponse::new(200, PROJECTS_BODY),
        ]);

        let signed_in = sign_in_over(&transport, CLIENT, "the-code", REDIRECT_URI)
            .await
            .expect("the sign-in completes");

        let exchange = transport.request(0);
        assert_eq!(exchange.method.as_str(), "POST");
        assert_eq!(exchange.url, "https://oauth2.googleapis.com/token");
        let body = exchange.body_text().expect("UTF-8");
        assert_eq!(field(body, "grant_type"), "authorization_code");
        assert_eq!(field(body, "code"), "the-code");
        assert_eq!(field(body, "client_id"), CLIENT.id);
        assert_eq!(field(body, "client_secret"), CLIENT.secret);
        assert_eq!(field(body, "redirect_uri"), REDIRECT_URI);

        assert_eq!(signed_in.account, "me@lexo.cool");
        assert_eq!(signed_in.access_token, "the-google-access-token");
        assert_eq!(
            signed_in.choices.len(),
            1,
            "a deleted project is not a choice"
        );
        assert_eq!(signed_in.choices[0].id, PROJECT);
        assert_eq!(signed_in.choices[0].name, "flyco dev");
    }

    #[skyzen::test]
    async fn the_project_listing_is_a_bearer_read() {
        let transport = RecordedTransport::new(vec![HttpResponse::new(200, PROJECTS_BODY)]);
        projects_over(&transport, "the-token")
            .await
            .expect("the listing succeeds");

        let request = transport.request(0);
        assert_eq!(request.method.as_str(), "GET");
        assert_eq!(
            request.url,
            "https://cloudresourcemanager.googleapis.com/v1/projects"
        );
        assert!(
            request
                .headers
                .contains(&("authorization".to_owned(), "Bearer the-token".to_owned()))
        );
    }

    #[skyzen::test]
    async fn creating_an_identity_makes_an_account_a_binding_and_a_key() {
        let transport = RecordedTransport::new(vec![
            HttpResponse::new(200, SERVICE_ACCOUNT_BODY),
            HttpResponse::new(200, POLICY_BODY),
            HttpResponse::new(200, POLICY_BODY),
            HttpResponse::new(200, KEY_BODY),
        ]);

        let key = create_identity_over(&transport, "the-token", PROJECT)
            .await
            .expect("the identity is created");

        let created = transport.request(0);
        assert_eq!(
            created.url,
            "https://iam.googleapis.com/v1/projects/flyco-dev-4821/serviceAccounts"
        );
        let body = json(created.body_text().expect("UTF-8"));
        assert_eq!(body["accountId"], "flyco");
        assert_eq!(body["serviceAccount"]["displayName"], "flyco");

        assert_eq!(
            transport.request(1).url,
            "https://cloudresourcemanager.googleapis.com/v1/projects/flyco-dev-4821:getIamPolicy"
        );

        let written = transport.request(2);
        assert_eq!(
            written.url,
            "https://cloudresourcemanager.googleapis.com/v1/projects/flyco-dev-4821:setIamPolicy"
        );
        let policy = json(written.body_text().expect("UTF-8"));
        assert_eq!(
            policy["policy"]["etag"], "BwYcQ1s2ZkE=",
            "the optimistic lock is written back untouched"
        );
        assert!(
            policy["policy"]["auditConfigs"].is_array(),
            "a policy flyco did not write must survive the round trip: {policy}"
        );
        let binding = policy["policy"]["bindings"]
            .as_array()
            .expect("bindings is an array")
            .iter()
            .find(|binding| binding["role"] == "roles/compute.admin")
            .expect("the compute.admin binding");
        assert!(
            binding["members"]
                .as_array()
                .expect("members is an array")
                .iter()
                .any(|member| member.as_str()
                    == Some("serviceAccount:flyco@flyco-dev-4821.iam.gserviceaccount.com")),
            "the service account joins the binding that was already there: {binding}"
        );

        let minted = transport.request(3);
        assert_eq!(
            minted.url,
            "https://iam.googleapis.com/v1/projects/flyco-dev-4821/serviceAccounts/\
             flyco@flyco-dev-4821.iam.gserviceaccount.com/keys"
        );
        assert_eq!(
            json(minted.body_text().expect("UTF-8"))["privateKeyType"],
            "TYPE_GOOGLE_CREDENTIALS_FILE"
        );

        // The key is the JSON document the user would have downloaded.
        let document = json(&key);
        assert_eq!(document["type"], "service_account");
        assert_eq!(document["client_email"], EMAIL);
    }

    #[skyzen::test]
    async fn a_project_with_no_compute_admin_binding_gains_one() {
        let transport = RecordedTransport::new(vec![
            HttpResponse::new(200, SERVICE_ACCOUNT_BODY),
            HttpResponse::new(200, POLICY_WITHOUT_ROLE_BODY),
            HttpResponse::new(200, POLICY_WITHOUT_ROLE_BODY),
            HttpResponse::new(200, KEY_BODY),
        ]);

        create_identity_over(&transport, "the-token", PROJECT)
            .await
            .expect("the identity is created");

        let policy = json(transport.request(2).body_text().expect("UTF-8"));
        let bindings = policy["policy"]["bindings"]
            .as_array()
            .expect("bindings is an array");
        assert_eq!(bindings.len(), 2, "the owner binding is kept: {policy}");
        assert!(
            bindings
                .iter()
                .any(|binding| binding["role"] == "roles/compute.admin")
        );
    }

    #[skyzen::test]
    async fn an_account_flyco_already_made_is_reused_rather_than_failing() {
        let transport = RecordedTransport::new(vec![
            HttpResponse::new(409, ALREADY_EXISTS_BODY),
            HttpResponse::new(200, POLICY_BODY),
            HttpResponse::new(200, POLICY_BODY),
            HttpResponse::new(200, KEY_BODY),
        ]);

        create_identity_over(&transport, "the-token", PROJECT)
            .await
            .expect("linking the same project twice works");

        assert!(
            transport.request(3).url.contains(EMAIL),
            "the derived address is the one Google gave the first time"
        );
    }

    #[skyzen::test]
    async fn a_refusal_keeps_googles_own_reason() {
        let transport = RecordedTransport::new(vec![HttpResponse::new(403, FORBIDDEN_BODY)]);
        let error = create_identity_over(&transport, "the-token", PROJECT)
            .await
            .expect_err("a project the account may not act in");

        assert!(matches!(
            error,
            GoogleError::Rejected { status, message }
                if status == "PERMISSION_DENIED" && message.contains("iam.serviceAccounts.create")
        ));
    }

    #[skyzen::test]
    async fn an_unreadable_failure_is_reported_as_its_status() {
        let transport = RecordedTransport::new(vec![HttpResponse::new(503, "<html>nope</html>")]);
        let error = projects_over(&transport, "the-token")
            .await
            .expect_err("an outage page");

        assert!(matches!(error, GoogleError::Status(503)));
    }
}
