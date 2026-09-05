//! The Microsoft side of "Sign in with Microsoft".
//!
//! Linking Azure used to mean running `az ad sp create-for-rbac` in a
//! terminal and pasting four strings into a form. This module is what
//! replaces that: the user approves one consent screen, and the control
//! plane does by API exactly what the CLI would have done — create an
//! application, mint it a client secret, give it a service principal, and
//! make that principal `Contributor` on the subscription they chose.
//!
//! Two grants come out of one consent. Microsoft issues an access token for
//! a single resource at a time, so the authorize URL asks for both scopes
//! ([`SCOPE`]), the code is redeemed for the ARM token plus a refresh token,
//! and the refresh token is immediately redeemed again for the Graph token.
//! That is Microsoft's own documented way to hold two resources' tokens from
//! one sign-in, and it is why [`sign_in_over`] makes two calls to the token
//! endpoint rather than one.
//!
//! Everything that leaves the control plane for Microsoft goes through an
//! [`HttpTransport`](flyco_provider::HttpTransport), which is zenwave in
//! production and a table of recorded exchanges under test — the same
//! arrangement [`crate::anthropic`] and [`crate::openai`] use, and for the
//! same reason: it is the only way to pin what actually goes on the wire.

use core::future::Future;

use flyco_core::ProviderOauthChoice;
use flyco_provider::clock::{SystemTimer, Timer};
use flyco_provider::http::{HttpRequest, HttpResponse, Method};
use flyco_provider::polling::poll_delay;
use flyco_provider::{HttpError, HttpTransport, LiveTransport};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::jwt;

/// Where the browser approves the grant.
///
/// The `organizations` authority rather than `common`: Azure Resource
/// Manager refuses a consumer sign-in outright, and `common` signs a
/// personal address in as a consumer first. Every Azure subscription lives
/// in a directory, and `organizations` signs the same address in as the
/// directory identity that owns it — for a personal account, the "work or
/// school" identity Microsoft made when the subscription was created.
const AUTHORIZE_URL: &str = "https://login.microsoftonline.com/organizations/oauth2/v2.0/authorize";

/// Where an authorization code and a refresh token are both redeemed.
const TOKEN_URL: &str = "https://login.microsoftonline.com/organizations/oauth2/v2.0/token";

/// What the user consents to, once, for both resources.
///
/// `offline_access` is what makes the refresh token exist, and the refresh
/// token is how the second resource's access token is obtained — see the
/// module documentation.
pub const SCOPE: &str = "openid offline_access https://management.azure.com/user_impersonation \
                         https://graph.microsoft.com/Application.ReadWrite.All";

/// The resource the authorization code is redeemed for: Azure Resource
/// Manager, which lists subscriptions and assigns roles.
const ARM_SCOPE: &str = "https://management.azure.com/user_impersonation offline_access openid";

/// The resource the refresh token is then redeemed for: Microsoft Graph,
/// which creates the application, its secret and its service principal.
const GRAPH_SCOPE: &str = "https://graph.microsoft.com/Application.ReadWrite.All";

/// Where the subscriptions a signed-in account can see are listed.
const SUBSCRIPTIONS_URL: &str = "https://management.azure.com/subscriptions?api-version=2022-12-01";

/// Root of the Graph applications collection.
const APPLICATIONS_URL: &str = "https://graph.microsoft.com/v1.0/applications";

/// Root of the Graph service-principals collection.
const SERVICE_PRINCIPALS_URL: &str = "https://graph.microsoft.com/v1.0/servicePrincipals";

/// What the application, its secret and its principal are all called in the
/// user's directory, so they are recognisable as flyco's and removable by
/// hand.
const APPLICATION_NAME: &str = "flyco";

/// Single-tenant, because the identity exists to act inside exactly one
/// directory: the one that consented.
const SIGN_IN_AUDIENCE: &str = "AzureADMyOrg";

/// Azure's built-in `Contributor` role, whose id is the same in every
/// tenant.
const CONTRIBUTOR_ROLE_ID: &str = "b24988ac-6180-42a0-ab88-20f7382dd24c";

/// `api-version` of the role-assignment endpoint.
const ROLE_ASSIGNMENT_API_VERSION: &str = "2022-04-01";

/// What a role assignment names the thing it grants to.
const SERVICE_PRINCIPAL_TYPE: &str = "ServicePrincipal";

/// The subscription state flyco can provision into.
const ENABLED: &str = "Enabled";

/// What ARM calls a principal Graph created moments ago and it cannot see
/// yet.
const PRINCIPAL_NOT_FOUND: &str = "PrincipalNotFound";

/// Longest a role assignment is retried for while ARM catches up with
/// Graph.
///
/// A freshly created service principal takes a few seconds to replicate into
/// ARM's view of the directory, and until it does the role assignment fails
/// with [`PRINCIPAL_NOT_FOUND`]. A minute is an order of magnitude beyond
/// what that takes in practice, and past it the failure is real.
const PRINCIPAL_VISIBILITY_SECONDS: u32 = 60;

/// What a refusal says when the vendor states no description.
const NO_DESCRIPTION: &str = "no description";

/// The OAuth client one deployment presents to Microsoft.
///
/// A struct rather than two `&str` arguments so a call site reads which half
/// is which; both are the deployment's, never the user's.
#[derive(Debug, Clone, Copy)]
pub struct OauthClient<'a> {
    /// Application (client) id of flyco's own registered application.
    pub id: &'a str,
    /// Client secret issued for it.
    pub secret: &'a str,
}

/// The tokens one Microsoft sign-in holds, and the directory they act in.
///
/// Kept in the key-value store between the callback and the finish, which is
/// why it serializes — and why `Debug` shows the tenant and nothing else.
#[derive(Clone, Serialize, Deserialize)]
pub struct AzureTokens {
    /// Directory (tenant) the signed-in account belongs to, read from the
    /// id token.
    pub tenant_id: String,
    /// Bearer token for `management.azure.com`.
    pub arm_token: String,
    /// Bearer token for `graph.microsoft.com`.
    pub graph_token: String,
}

impl core::fmt::Debug for AzureTokens {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AzureTokens")
            .field("tenant_id", &self.tenant_id)
            .finish_non_exhaustive()
    }
}

/// What redeeming the authorization code yields.
///
/// Private to the sign-in: the refresh token exists only to be traded for
/// the Graph token a moment later, and nothing outside this module ever
/// holds one.
#[expect(
    clippy::struct_field_names,
    reason = "three tokens is what the grant is; the shared suffix is Microsoft's own \
              naming, and dropping it would leave `arm`, `refresh` and `id`"
)]
struct CodeGrant {
    /// Bearer token for `management.azure.com`.
    arm_token: String,
    /// Redeemed once, for the Graph token.
    refresh_token: String,
    /// Names the account and its tenant.
    id_token: String,
}

/// Everything the callback learned from one Microsoft sign-in.
#[derive(Debug, Clone)]
pub struct SignIn {
    /// The account that signed in, as Microsoft names it.
    pub account: String,
    /// The subscriptions that account may link.
    pub choices: Vec<ProviderOauthChoice>,
    /// The tokens the finish will act with.
    pub tokens: AzureTokens,
}

/// The service principal flyco created, as a credential to store.
///
/// Exactly the four strings `az ad sp create-for-rbac` prints, minus the
/// subscription the caller already chose.
#[derive(Clone)]
pub struct AzureIdentity {
    /// Directory the principal belongs to.
    pub tenant_id: String,
    /// Application (client) id of the created application.
    pub client_id: String,
    /// Client secret minted for it.
    pub client_secret: String,
}

impl core::fmt::Debug for AzureIdentity {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AzureIdentity")
            .field("tenant_id", &self.tenant_id)
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

/// Why a call to Microsoft did not produce what flyco asked for.
#[derive(Debug, thiserror::Error)]
pub enum MicrosoftError {
    /// The request never completed, or the response was not the expected
    /// JSON.
    #[error("Microsoft request failed: {0}")]
    Transport(String),
    /// Microsoft refused, and said why.
    #[error("{code}: {description}")]
    Rejected {
        /// Microsoft's machine-readable error code.
        code: String,
        /// Microsoft's human-readable explanation.
        description: String,
    },
    /// Microsoft answered with something flyco cannot use.
    #[error("Microsoft answered with something flyco cannot use: {0}")]
    Malformed(&'static str),
    /// Microsoft answered with a status flyco cannot interpret.
    #[error("Microsoft responded with HTTP {0}")]
    Status(u16),
}

impl From<HttpError> for MicrosoftError {
    fn from(error: HttpError) -> Self {
        Self::Transport(error.to_string())
    }
}

impl MicrosoftError {
    /// Whether this is ARM saying it cannot see a principal Graph has
    /// already created.
    fn is_principal_not_found(&self) -> bool {
        matches!(self, Self::Rejected { code, .. } if code == PRINCIPAL_NOT_FOUND)
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
    let mut url = Url::parse(AUTHORIZE_URL).expect("the Microsoft authorize URL is absolute");
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_mode", "query")
        .append_pair("state", state)
        .append_pair("prompt", "select_account")
        .append_pair("scope", SCOPE);
    url
}

/// The OAuth error document the token endpoint returns.
#[derive(Debug, Deserialize)]
struct OauthError {
    error: String,
    error_description: Option<String>,
}

/// The error document ARM and Graph return.
#[derive(Debug, Deserialize)]
struct AzureErrorDocument {
    error: AzureErrorBody,
}

/// Its one member.
#[derive(Debug, Deserialize)]
struct AzureErrorBody {
    code: String,
    message: String,
}

/// Turns a refusal into the reason Microsoft gave for it.
///
/// Two documents, because Microsoft has two: the token endpoint speaks
/// RFC 6749's `{error, error_description}` and everything else speaks
/// `{error: {code, message}}`. The OAuth shape is tried first because its
/// `error` is a string, which the other document's object cannot be read as.
fn refusal(response: &HttpResponse) -> MicrosoftError {
    if let Ok(oauth) = response.json::<OauthError>() {
        return MicrosoftError::Rejected {
            code: oauth.error,
            description: oauth
                .error_description
                .unwrap_or_else(|| NO_DESCRIPTION.to_owned()),
        };
    }
    response.json::<AzureErrorDocument>().map_or_else(
        |_| MicrosoftError::Status(response.status),
        |document| MicrosoftError::Rejected {
            code: document.error.code,
            description: document.error.message,
        },
    )
}

/// Reads a JSON body, or reports why the call failed.
fn decoded<T: serde::de::DeserializeOwned>(response: &HttpResponse) -> Result<T, MicrosoftError> {
    if response.is_success() {
        return response
            .json::<T>()
            .map_err(|error| MicrosoftError::Transport(error.to_string()));
    }
    Err(refusal(response))
}

/// Accepts a response that carries nothing worth reading.
fn accepted(response: &HttpResponse) -> Result<(), MicrosoftError> {
    if response.is_success() {
        return Ok(());
    }
    Err(refusal(response))
}

/// What the token endpoint answers with.
#[expect(
    clippy::struct_field_names,
    reason = "the field names are the wire format's, and renaming them would mean \
              spelling each one twice in a serde attribute"
)]
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

/// The claims flyco reads out of a Microsoft id token.
#[derive(Debug, Default, Deserialize)]
struct IdClaims {
    /// Directory (tenant) the account signed in against.
    #[serde(default)]
    tid: Option<String>,
    /// The address the account signs in with, when it has one.
    #[serde(default)]
    preferred_username: Option<String>,
    /// The display name, which is all a personal account may state.
    #[serde(default)]
    name: Option<String>,
}

/// One subscription, as ARM lists it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubscriptionRow {
    subscription_id: String,
    display_name: String,
    state: String,
}

/// The subscription listing.
#[derive(Debug, Deserialize)]
struct SubscriptionList {
    #[serde(default)]
    value: Vec<SubscriptionRow>,
}

/// Body of the application creation.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct NewApplication<'a> {
    display_name: &'a str,
    sign_in_audience: &'a str,
}

/// What Graph answers with when it created one.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Application {
    /// The directory object id, which is what the secret is added to.
    id: String,
    /// The application (client) id, which is what a token is requested with.
    app_id: String,
}

/// Body of the secret creation.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct NewPassword<'a> {
    password_credential: PasswordName<'a>,
}

/// Its one member.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PasswordName<'a> {
    display_name: &'a str,
}

/// What Graph answers with when it minted one.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PasswordCredential {
    /// The secret itself, which Graph states exactly once.
    secret_text: String,
}

/// Body of the service-principal creation.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct NewServicePrincipal<'a> {
    app_id: &'a str,
}

/// What Graph answers with when it created one.
#[derive(Debug, Deserialize)]
struct ServicePrincipal {
    /// The principal's own object id, which is what a role is assigned to.
    id: String,
}

/// Body of the role assignment.
#[derive(Debug, Serialize)]
struct RoleAssignment<'a> {
    properties: RoleAssignmentProperties<'a>,
}

/// What it grants, to whom.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RoleAssignmentProperties<'a> {
    role_definition_id: String,
    principal_id: &'a str,
    principal_type: &'a str,
}

/// Where a secret is added to an application.
fn add_password_url(application_id: &str) -> String {
    format!("{APPLICATIONS_URL}/{application_id}/addPassword")
}

/// Where one role assignment lives.
///
/// Its name is a GUID the caller mints: ARM addresses an assignment by a
/// name rather than creating one, so a `PUT` to a fresh GUID is how an
/// assignment is created.
fn role_assignment_url(subscription_id: &str, assignment: uuid::Uuid) -> String {
    format!(
        "https://management.azure.com/subscriptions/{subscription_id}/providers/\
         Microsoft.Authorization/roleAssignments/{assignment}\
         ?api-version={ROLE_ASSIGNMENT_API_VERSION}"
    )
}

/// The `Contributor` role, as ARM names it inside one subscription.
fn contributor_role(subscription_id: &str) -> String {
    format!(
        "/subscriptions/{subscription_id}/providers/Microsoft.Authorization/\
         roleDefinitions/{CONTRIBUTOR_ROLE_ID}"
    )
}

/// Describes one token-endpoint exchange as an [`HttpRequest`].
fn token_request(fields: &[(&str, &str)]) -> HttpRequest {
    HttpRequest::new(Method::Post, TOKEN_URL)
        .header("accept", "application/json")
        .form_body(fields)
}

/// Redeems the authorization code for the ARM token and a refresh token.
///
/// # Errors
///
/// Returns [`MicrosoftError`] if the exchange fails, Microsoft refuses the
/// grant, or the answer carries no refresh token or id token — both of which
/// the sign-in cannot continue without.
async fn exchange_over<T: HttpTransport>(
    transport: &T,
    client: OauthClient<'_>,
    code: &str,
    redirect_uri: &str,
) -> Result<CodeGrant, MicrosoftError> {
    let response = transport
        .send(token_request(&[
            ("grant_type", "authorization_code"),
            ("client_id", client.id),
            ("client_secret", client.secret),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("scope", ARM_SCOPE),
        ]))
        .await?;
    let tokens: TokenResponse = decoded(&response)?;

    let refresh_token = tokens.refresh_token.ok_or(MicrosoftError::Malformed(
        "the grant carries no refresh token, so the Graph token cannot be obtained",
    ))?;
    let id_token = tokens.id_token.ok_or(MicrosoftError::Malformed(
        "the grant carries no id token, so the account and tenant are unknown",
    ))?;
    Ok(CodeGrant {
        arm_token: tokens.access_token,
        refresh_token,
        id_token,
    })
}

/// Redeems the refresh token for a Microsoft Graph access token.
///
/// # Errors
///
/// Returns [`MicrosoftError`] if the exchange fails or Microsoft refuses it.
pub async fn graph_token_over<T: HttpTransport>(
    transport: &T,
    client: OauthClient<'_>,
    refresh_token: &str,
) -> Result<String, MicrosoftError> {
    let response = transport
        .send(token_request(&[
            ("grant_type", "refresh_token"),
            ("client_id", client.id),
            ("client_secret", client.secret),
            ("refresh_token", refresh_token),
            ("scope", GRAPH_SCOPE),
        ]))
        .await?;
    let tokens: TokenResponse = decoded(&response)?;
    Ok(tokens.access_token)
}

/// Lists the enabled subscriptions the ARM token can see.
///
/// A disabled subscription is not a choice: nothing can be provisioned into
/// it, so offering it would be offering a link that cannot work.
///
/// # Errors
///
/// Returns [`MicrosoftError`] if the call fails or ARM refuses it.
pub async fn subscriptions_over<T: HttpTransport>(
    transport: &T,
    arm_token: &str,
) -> Result<Vec<ProviderOauthChoice>, MicrosoftError> {
    let response = transport
        .send(
            HttpRequest::new(Method::Get, SUBSCRIPTIONS_URL)
                .header("accept", "application/json")
                .bearer(arm_token),
        )
        .await?;
    let listed: SubscriptionList = decoded(&response)?;

    Ok(listed
        .value
        .into_iter()
        .filter(|row| row.state == ENABLED)
        .map(|row| ProviderOauthChoice {
            id: row.subscription_id,
            name: row.display_name,
        })
        .collect())
}

/// Runs the whole callback conversation: redeem, refresh, read, list.
///
/// # Errors
///
/// Returns [`MicrosoftError`] if any of the four steps fails.
pub async fn sign_in_over<T: HttpTransport>(
    transport: &T,
    client: OauthClient<'_>,
    code: &str,
    redirect_uri: &str,
) -> Result<SignIn, MicrosoftError> {
    let grant = exchange_over(transport, client, code, redirect_uri).await?;
    let graph_token = graph_token_over(transport, client, &grant.refresh_token).await?;

    let claims: IdClaims =
        jwt::claims(&grant.id_token).map_err(|error| MicrosoftError::Malformed(error.detail()))?;
    let tenant_id = claims.tid.ok_or(MicrosoftError::Malformed(
        "the id token names no tenant, so no directory can be acted in",
    ))?;
    let account = claims
        .preferred_username
        .or(claims.name)
        .ok_or(MicrosoftError::Malformed("the id token names no account"))?;

    let choices = subscriptions_over(transport, &grant.arm_token).await?;
    Ok(SignIn {
        account,
        choices,
        tokens: AzureTokens {
            tenant_id,
            arm_token: grant.arm_token,
            graph_token,
        },
    })
}

/// Creates the service principal flyco will provision with, and makes it
/// `Contributor` on `subscription_id`.
///
/// Four calls, in the order `az ad sp create-for-rbac` makes them: the
/// application, its secret, its principal, and the role. The last one is
/// retried while ARM catches up with Graph — see
/// [`PRINCIPAL_VISIBILITY_SECONDS`].
///
/// # Errors
///
/// Returns [`MicrosoftError`] if any call fails, or if the principal is
/// still invisible to ARM after [`PRINCIPAL_VISIBILITY_SECONDS`].
pub async fn create_identity_over<T: HttpTransport, K: Timer>(
    transport: &T,
    timer: &K,
    tokens: &AzureTokens,
    subscription_id: &str,
) -> Result<AzureIdentity, MicrosoftError> {
    let created: Application = decoded(
        &transport
            .send(
                HttpRequest::new(Method::Post, APPLICATIONS_URL)
                    .header("accept", "application/json")
                    .bearer(&tokens.graph_token)
                    .json_body(&NewApplication {
                        display_name: APPLICATION_NAME,
                        sign_in_audience: SIGN_IN_AUDIENCE,
                    })?,
            )
            .await?,
    )?;

    let password: PasswordCredential = decoded(
        &transport
            .send(
                HttpRequest::new(Method::Post, add_password_url(&created.id))
                    .header("accept", "application/json")
                    .bearer(&tokens.graph_token)
                    .json_body(&NewPassword {
                        password_credential: PasswordName {
                            display_name: APPLICATION_NAME,
                        },
                    })?,
            )
            .await?,
    )?;

    let principal: ServicePrincipal = decoded(
        &transport
            .send(
                HttpRequest::new(Method::Post, SERVICE_PRINCIPALS_URL)
                    .header("accept", "application/json")
                    .bearer(&tokens.graph_token)
                    .json_body(&NewServicePrincipal {
                        app_id: &created.app_id,
                    })?,
            )
            .await?,
    )?;

    assign_contributor(transport, timer, tokens, subscription_id, &principal.id).await?;

    Ok(AzureIdentity {
        tenant_id: tokens.tenant_id.clone(),
        client_id: created.app_id,
        client_secret: password.secret_text,
    })
}

/// Makes `principal_id` `Contributor` on the subscription, waiting out the
/// window in which ARM cannot see it yet.
async fn assign_contributor<T: HttpTransport, K: Timer>(
    transport: &T,
    timer: &K,
    tokens: &AzureTokens,
    subscription_id: &str,
    principal_id: &str,
) -> Result<(), MicrosoftError> {
    // One name for the assignment across every attempt: a retry that minted
    // a fresh GUID each time would create a second assignment if the first
    // had in fact been accepted.
    let assignment = uuid::Uuid::new_v4();
    let body = RoleAssignment {
        properties: RoleAssignmentProperties {
            role_definition_id: contributor_role(subscription_id),
            principal_id,
            principal_type: SERVICE_PRINCIPAL_TYPE,
        },
    };
    let url = role_assignment_url(subscription_id, assignment);

    let mut waited = 0_u32;
    let mut attempt = 0_usize;
    loop {
        let response = transport
            .send(
                HttpRequest::new(Method::Put, url.clone())
                    .header("accept", "application/json")
                    .bearer(&tokens.arm_token)
                    .json_body(&body)?,
            )
            .await?;
        let Err(error) = accepted(&response) else {
            return Ok(());
        };
        if !error.is_principal_not_found() {
            return Err(error);
        }

        let delay = poll_delay(None, attempt);
        if waited.saturating_add(delay) > PRINCIPAL_VISIBILITY_SECONDS {
            return Err(error);
        }
        tracing::debug!(seconds = delay, "waiting for Azure to see a new principal");
        timer.sleep(delay).await;
        waited = waited.saturating_add(delay);
        attempt = attempt.saturating_add(1);
    }
}

/// The two conversations the Microsoft sign-in has.
///
/// Behind a trait for the same reason [`crate::anthropic::ClaudeOauth`] is:
/// the happy path is otherwise untestable, because a route handler cannot be
/// exercised without standing in for `login.microsoftonline.com`.
pub trait MicrosoftOauth: Send + Sync + Clone + 'static {
    /// Redeems the code and reports who signed in and what they may link.
    ///
    /// # Errors
    ///
    /// Returns [`MicrosoftError`] if any step of the exchange fails.
    fn sign_in(
        &self,
        client: OauthClient<'_>,
        code: &str,
        redirect_uri: &str,
    ) -> impl Future<Output = Result<SignIn, MicrosoftError>> + Send;

    /// Creates the service principal flyco provisions with.
    ///
    /// # Errors
    ///
    /// Returns [`MicrosoftError`] if any step of the creation fails.
    fn create_identity(
        &self,
        tokens: &AzureTokens,
        subscription_id: &str,
    ) -> impl Future<Output = Result<AzureIdentity, MicrosoftError>> + Send;
}

/// The production client, speaking HTTP through zenwave — hyper natively,
/// Fetch inside the Worker.
#[derive(Debug, Clone, Copy, Default)]
pub struct ZenwaveMicrosoft {
    transport: LiveTransport,
    timer: SystemTimer,
}

impl ZenwaveMicrosoft {
    /// Creates the client.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            transport: LiveTransport::new(),
            timer: SystemTimer::new(),
        }
    }
}

impl MicrosoftOauth for ZenwaveMicrosoft {
    async fn sign_in(
        &self,
        client: OauthClient<'_>,
        code: &str,
        redirect_uri: &str,
    ) -> Result<SignIn, MicrosoftError> {
        sign_in_over(&self.transport, client, code, redirect_uri).await
    }

    async fn create_identity(
        &self,
        tokens: &AzureTokens,
        subscription_id: &str,
    ) -> Result<AzureIdentity, MicrosoftError> {
        create_identity_over(&self.transport, &self.timer, tokens, subscription_id).await
    }
}

/// The Microsoft client the router carries.
///
/// An enum rather than a type parameter for the same reason
/// [`GithubClient`](crate::github::GithubClient) is one: `#[skyzen::openapi]`
/// cannot annotate a generic handler, and one concrete type keeps every
/// operation id stable.
#[derive(Debug, Clone)]
pub enum MicrosoftClient {
    /// Talks to `login.microsoftonline.com`, `graph.microsoft.com` and
    /// `management.azure.com`.
    Live(ZenwaveMicrosoft),
    /// Answers from fixtures, for tests.
    #[cfg(test)]
    Fake(crate::testing::TestMicrosoft),
}

impl Default for MicrosoftClient {
    fn default() -> Self {
        Self::Live(ZenwaveMicrosoft::new())
    }
}

impl MicrosoftOauth for MicrosoftClient {
    async fn sign_in(
        &self,
        client: OauthClient<'_>,
        code: &str,
        redirect_uri: &str,
    ) -> Result<SignIn, MicrosoftError> {
        match self {
            Self::Live(live) => live.sign_in(client, code, redirect_uri).await,
            #[cfg(test)]
            Self::Fake(fake) => fake.sign_in(client, code, redirect_uri).await,
        }
    }

    async fn create_identity(
        &self,
        tokens: &AzureTokens,
        subscription_id: &str,
    ) -> Result<AzureIdentity, MicrosoftError> {
        match self {
            Self::Live(live) => live.create_identity(tokens, subscription_id).await,
            #[cfg(test)]
            Self::Fake(fake) => fake.create_identity(tokens, subscription_id).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use flyco_provider::http::HttpResponse;
    use flyco_provider::testing::{RecordedTransport, RecordingTimer};

    use super::{
        AzureTokens, MicrosoftError, OauthClient, SCOPE, authorize_url, create_identity_over,
        sign_in_over, subscriptions_over,
    };

    /// The ARM grant, as the token endpoint returns one.
    const ARM_TOKEN_BODY: &str = include_str!("../fixtures/microsoft/arm_token.json");
    /// The Graph grant the refresh yields.
    const GRAPH_TOKEN_BODY: &str = include_str!("../fixtures/microsoft/graph_token.json");
    /// Two subscriptions, one of them disabled.
    const SUBSCRIPTIONS_BODY: &str = include_str!("../fixtures/microsoft/subscriptions.json");
    /// The created application.
    const APPLICATION_BODY: &str = include_str!("../fixtures/microsoft/application.json");
    /// The minted client secret.
    const PASSWORD_BODY: &str = include_str!("../fixtures/microsoft/password.json");
    /// The created service principal.
    const PRINCIPAL_BODY: &str = include_str!("../fixtures/microsoft/service_principal.json");
    /// The role assignment ARM accepted.
    const ROLE_ASSIGNMENT_BODY: &str = include_str!("../fixtures/microsoft/role_assignment.json");
    /// ARM refusing a principal it cannot see yet.
    const PRINCIPAL_NOT_FOUND_BODY: &str =
        include_str!("../fixtures/microsoft/principal_not_found.json");
    /// The token endpoint refusing a code.
    const INVALID_GRANT_BODY: &str = include_str!("../fixtures/microsoft/invalid_grant.json");

    const CLIENT: OauthClient<'static> = OauthClient {
        id: "the-client-id",
        secret: "the-client-secret",
    };

    const REDIRECT_URI: &str = "https://flyco.test/v1/providers/azure/oauth/callback";

    fn tokens() -> AzureTokens {
        AzureTokens {
            tenant_id: "the-tenant".to_owned(),
            arm_token: "the-arm-token".to_owned(),
            graph_token: "the-graph-token".to_owned(),
        }
    }

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

    #[test]
    fn the_authorize_url_asks_for_both_resources_at_once() {
        let url = authorize_url("the-client-id", REDIRECT_URI, "the-state");

        assert_eq!(url.host_str(), Some("login.microsoftonline.com"));
        assert_eq!(url.path(), "/organizations/oauth2/v2.0/authorize");
        assert_eq!(query(&url, "client_id"), "the-client-id");
        assert_eq!(query(&url, "response_type"), "code");
        assert_eq!(query(&url, "redirect_uri"), REDIRECT_URI);
        assert_eq!(query(&url, "response_mode"), "query");
        assert_eq!(query(&url, "state"), "the-state");
        assert_eq!(query(&url, "prompt"), "select_account");

        let scope = query(&url, "scope");
        assert_eq!(scope, SCOPE);
        assert!(scope.contains("management.azure.com/user_impersonation"));
        assert!(scope.contains("graph.microsoft.com/Application.ReadWrite.All"));
        assert!(scope.contains("offline_access"));
    }

    #[skyzen::test]
    async fn a_sign_in_redeems_the_code_then_trades_the_refresh_token_for_graph() {
        let transport = RecordedTransport::new(vec![
            HttpResponse::new(200, ARM_TOKEN_BODY),
            HttpResponse::new(200, GRAPH_TOKEN_BODY),
            HttpResponse::new(200, SUBSCRIPTIONS_BODY),
        ]);

        let signed_in = sign_in_over(&transport, CLIENT, "the-code", REDIRECT_URI)
            .await
            .expect("the sign-in completes");

        let exchange = transport.request(0);
        assert_eq!(exchange.method.as_str(), "POST");
        assert_eq!(
            exchange.url,
            "https://login.microsoftonline.com/organizations/oauth2/v2.0/token"
        );
        let body = exchange.body_text().expect("UTF-8");
        assert_eq!(field(body, "grant_type"), "authorization_code");
        assert_eq!(field(body, "code"), "the-code");
        assert_eq!(field(body, "client_id"), CLIENT.id);
        assert_eq!(field(body, "client_secret"), CLIENT.secret);
        assert_eq!(field(body, "redirect_uri"), REDIRECT_URI);
        assert!(field(body, "scope").contains("management.azure.com"));

        let refresh = transport.request(1);
        let body = refresh.body_text().expect("UTF-8");
        assert_eq!(field(body, "grant_type"), "refresh_token");
        assert_eq!(field(body, "refresh_token"), "the-refresh-token");
        assert_eq!(
            field(body, "scope"),
            "https://graph.microsoft.com/Application.ReadWrite.All"
        );

        // The id token is read rather than verified — see `crate::jwt`.
        assert_eq!(signed_in.account, "me@lexo.cool");
        assert_eq!(
            signed_in.tokens.tenant_id,
            "11111111-2222-4333-8444-555555555555"
        );
        assert_eq!(signed_in.tokens.arm_token, "the-arm-access-token");
        assert_eq!(signed_in.tokens.graph_token, "the-graph-access-token");

        // The disabled subscription is not a choice.
        assert_eq!(signed_in.choices.len(), 1);
        assert_eq!(
            signed_in.choices[0].id,
            "00000000-1111-4222-8333-444444444444"
        );
        assert_eq!(signed_in.choices[0].name, "Visual Studio Enterprise");
    }

    #[skyzen::test]
    async fn the_subscription_listing_names_the_api_version_it_was_written_against() {
        let transport = RecordedTransport::new(vec![HttpResponse::new(200, SUBSCRIPTIONS_BODY)]);
        subscriptions_over(&transport, "the-arm-token")
            .await
            .expect("the listing succeeds");

        let request = transport.request(0);
        assert_eq!(request.method.as_str(), "GET");
        assert_eq!(
            request.url,
            "https://management.azure.com/subscriptions?api-version=2022-12-01"
        );
        assert!(request.headers.contains(&(
            "authorization".to_owned(),
            "Bearer the-arm-token".to_owned()
        )));
    }

    #[skyzen::test]
    async fn creating_an_identity_makes_an_app_a_secret_a_principal_and_a_role() {
        let transport = RecordedTransport::new(vec![
            HttpResponse::new(201, APPLICATION_BODY),
            HttpResponse::new(200, PASSWORD_BODY),
            HttpResponse::new(201, PRINCIPAL_BODY),
            HttpResponse::new(201, ROLE_ASSIGNMENT_BODY),
        ]);
        let timer = RecordingTimer::new();

        let identity = create_identity_over(&transport, &timer, &tokens(), "the-subscription")
            .await
            .expect("the identity is created");

        assert_eq!(identity.tenant_id, "the-tenant");
        assert_eq!(identity.client_id, "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee");
        assert_eq!(identity.client_secret, "the-client-secret-graph-minted");

        let application = transport.request(0);
        assert_eq!(
            application.url,
            "https://graph.microsoft.com/v1.0/applications"
        );
        let body: serde_json::Value =
            serde_json::from_str(application.body_text().expect("UTF-8")).expect("JSON");
        assert_eq!(body["displayName"], "flyco");
        assert_eq!(body["signInAudience"], "AzureADMyOrg");

        let password = transport.request(1);
        assert_eq!(
            password.url,
            "https://graph.microsoft.com/v1.0/applications/the-application-object-id/addPassword"
        );

        let principal = transport.request(2);
        assert_eq!(
            principal.url,
            "https://graph.microsoft.com/v1.0/servicePrincipals"
        );
        let body: serde_json::Value =
            serde_json::from_str(principal.body_text().expect("UTF-8")).expect("JSON");
        assert_eq!(body["appId"], "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee");

        let role = transport.request(3);
        assert_eq!(role.method.as_str(), "PUT");
        assert!(role.url.starts_with(
            "https://management.azure.com/subscriptions/the-subscription/providers/\
             Microsoft.Authorization/roleAssignments/"
        ));
        assert!(role.url.ends_with("?api-version=2022-04-01"));
        let body: serde_json::Value =
            serde_json::from_str(role.body_text().expect("UTF-8")).expect("JSON");
        assert_eq!(
            body["properties"]["roleDefinitionId"],
            "/subscriptions/the-subscription/providers/Microsoft.Authorization/roleDefinitions/\
             b24988ac-6180-42a0-ab88-20f7382dd24c"
        );
        assert_eq!(body["properties"]["principalId"], "the-principal-object-id");
        assert_eq!(body["properties"]["principalType"], "ServicePrincipal");
        assert!(timer.delays().is_empty(), "nothing had to be waited for");
    }

    #[skyzen::test]
    async fn a_principal_arm_cannot_see_yet_is_waited_for_under_one_assignment_name() {
        let transport = RecordedTransport::new(vec![
            HttpResponse::new(201, APPLICATION_BODY),
            HttpResponse::new(200, PASSWORD_BODY),
            HttpResponse::new(201, PRINCIPAL_BODY),
            HttpResponse::new(400, PRINCIPAL_NOT_FOUND_BODY),
            HttpResponse::new(400, PRINCIPAL_NOT_FOUND_BODY),
            HttpResponse::new(201, ROLE_ASSIGNMENT_BODY),
        ]);
        let timer = RecordingTimer::new();

        create_identity_over(&transport, &timer, &tokens(), "the-subscription")
            .await
            .expect("the assignment succeeds once ARM catches up");

        assert_eq!(
            timer.delays(),
            vec![1, 2],
            "the wait grows between attempts"
        );
        assert_eq!(transport.request_count(), 6);
        assert_eq!(
            transport.request(3).url,
            transport.request(5).url,
            "a retry must not create a second assignment"
        );
    }

    #[skyzen::test]
    async fn a_principal_that_never_appears_fails_rather_than_waiting_for_ever() {
        let mut responses = vec![
            HttpResponse::new(201, APPLICATION_BODY),
            HttpResponse::new(200, PASSWORD_BODY),
            HttpResponse::new(201, PRINCIPAL_BODY),
        ];
        responses.resize_with(32, || HttpResponse::new(400, PRINCIPAL_NOT_FOUND_BODY));
        let transport = RecordedTransport::new(responses);
        let timer = RecordingTimer::new();

        let error = create_identity_over(&transport, &timer, &tokens(), "the-subscription")
            .await
            .expect_err("a principal that never replicates is a failure");

        assert!(matches!(
            error,
            MicrosoftError::Rejected { code, .. } if code == "PrincipalNotFound"
        ));
        assert!(
            timer.delays().iter().sum::<u32>() <= 60,
            "the wait is bounded: {:?}",
            timer.delays()
        );
    }

    #[skyzen::test]
    async fn a_refused_code_keeps_microsofts_own_reason() {
        let transport = RecordedTransport::new(vec![HttpResponse::new(400, INVALID_GRANT_BODY)]);
        let error = sign_in_over(&transport, CLIENT, "stale", REDIRECT_URI)
            .await
            .expect_err("a refused code");

        assert!(matches!(
            error,
            MicrosoftError::Rejected { code, description }
                if code == "invalid_grant" && description.contains("expired")
        ));
    }

    #[skyzen::test]
    async fn an_unreadable_failure_is_reported_as_its_status() {
        let transport = RecordedTransport::new(vec![HttpResponse::new(503, "<html>nope</html>")]);
        let error = subscriptions_over(&transport, "the-arm-token")
            .await
            .expect_err("an outage page");

        assert!(matches!(error, MicrosoftError::Status(503)));
    }
}
