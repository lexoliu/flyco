//! Proving a cloud credential works, and preparing what flyco owns inside
//! the account it opens.
//!
//! Every path that links a provider account runs through here first, so a
//! credential that cannot provision is refused where the mistake was made
//! rather than at the first session. It is a value the router carries rather
//! than a free function for the same reason
//! [`GithubClient`](crate::github::GithubClient) and
//! [`ClaudeClient`](crate::anthropic::ClaudeClient) are: the whole linking
//! path is otherwise untestable, because it cannot be exercised without a
//! real cloud account to talk to.

use core::future::Future;

use flyco_core::ProviderCredentials;
use flyco_provider::LoginKey;
use flyco_provider::ProviderError;
use flyco_provider::azure::RESOURCE_GROUP;

use crate::error::ApiError;

/// What linking a credential does at the provider before it is stored.
pub trait CloudLink: Send + Sync + Clone + 'static {
    /// Proves `credentials` work, and prepares the workspace flyco owns.
    ///
    /// Answers with the name of the Azure resource group flyco created, and
    /// `None` for every other provider.
    ///
    /// The returned future is deliberately not `Send`: the provider drivers
    /// are generic over a transport that is `fetch` inside the Worker, and
    /// demanding `Send` of their futures would force a bound wasm32 cannot
    /// satisfy — the same reason `clippy::future_not_send` is allowed
    /// workspace-wide.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::ProviderRejectedCredentials`] if the provider
    /// refuses them, and [`ApiError::HostNotLinkable`] for an enrolled
    /// machine, which is linked by enrolling it rather than by presenting a
    /// credential.
    fn prepare(
        &self,
        credentials: &ProviderCredentials,
        login_key: &LoginKey,
    ) -> impl Future<Output = Result<Option<String>, ApiError>>;
}

/// The production implementation, which talks to the provider.
#[derive(Debug, Clone, Copy, Default)]
pub struct LiveClouds;

impl LiveClouds {
    /// Creates it.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

/// Proves the credentials work before they are stored.
///
/// A credential that only fails at the first provision strands a session
/// half-created, so the check happens where the mistake was made. Each
/// provider is asked for the cheapest call that exercises the whole
/// credential and creates nothing:
///
/// * **Azure** — nothing here. Linking an Azure subscription creates the
///   resource group flyco owns inside it, and that write is a stronger check
///   than any read: a token proves the client secret is right, while the
///   `PUT` proves the principal is also scoped widely enough to be useful.
///   See [`azure_workspace`].
/// * **AWS** — `sts:GetCallerIdentity`, which no IAM policy can deny, costs
///   nothing, and answers with the account the key opens.
/// * **GCP** — a token mint, which is the whole credential: a service
///   account proves itself by signing an assertion with its private key and
///   having Google check it.
///
/// None of them proves the credential may *provision*: what a policy grants
/// is only knowable by trying, and a link-time simulation would be a second,
/// weaker opinion about a question the first provision answers exactly.
///
/// A machine the user owns is not linkable at all: it is enrolled, and
/// enrolling is what proves it exists.
async fn verify(credentials: &ProviderCredentials) -> Result<(), ApiError> {
    match credentials {
        // Azure is checked by [`azure_workspace`] instead, which has to run
        // anyway and proves strictly more: a token proves the secret is
        // right, and creating the resource group proves the principal is
        // scoped widely enough to be useful.
        ProviderCredentials::Azure { .. } => Ok(()),
        // A machine somebody owns is linked by *enrolling* it, which is
        // what mints its token and proves it answers. There is nothing to
        // check here because there is nothing a caller could present: the
        // host id in these credentials is written by
        // `POST /v1/hosts/enroll`, never by a request to this route.
        ProviderCredentials::Host { .. } => Err(ApiError::HostNotLinkable),
        ProviderCredentials::Aws {
            access_key_id,
            secret_access_key,
            session_token,
            key_name,
        } => crate::provisioning::aws_driver(
            access_key_id,
            secret_access_key,
            session_token.as_deref(),
            key_name.as_deref(),
        )
        .caller_identity()
        .await
        .map(|identity| {
            tracing::debug!(account = %identity.account, "an AWS access key checked out");
        })
        .map_err(rejected),
        ProviderCredentials::Gcp {
            service_account_json,
        } => {
            let mut provider =
                crate::provisioning::gcp_driver(service_account_json).map_err(rejected)?;
            provider.mint_token().await.map(|_| ()).map_err(rejected)
        }
        ProviderCredentials::Codespaces {
            token,
            env_repo,
            env_repo_id,
            owner_id,
            included_core_hours,
        } => {
            let verified = crate::provisioning::codespaces_driver(
                token,
                env_repo,
                *env_repo_id,
                *included_core_hours,
            )
            .verify()
            .await
            .map_err(rejected)?;
            if verified.user.id != *owner_id {
                return Err(ApiError::ProviderRejectedCredentials {
                    reason: format!(
                        "the token belongs to {}, not the GitHub account this credential was \
                         linked for — link the account again",
                        verified.user.login
                    ),
                });
            }
            Ok(())
        }
    }
}

/// Creates the resource group flyco owns inside a freshly linked Azure
/// subscription.
///
/// The user never names one. The service principal the sign-in mints is
/// `Contributor` on the whole subscription, which is exactly the scope a
/// resource-group creation needs, so flyco makes the group itself — and the
/// `PUT` doubles as the credential check, because it exercises the token
/// *and* the role assignment rather than only the secret.
///
/// Answers with the group's name, which is stored on the account so a
/// subscription linked today keeps the group it owns if flyco's default name
/// ever changes.
async fn azure_workspace(
    credentials: &ProviderCredentials,
    login_key: &LoginKey,
) -> Result<Option<String>, ApiError> {
    let ProviderCredentials::Azure {
        tenant_id,
        client_id,
        client_secret,
        subscription_id,
    } = credentials
    else {
        return Ok(None);
    };

    let region = crate::provisioning::azure_driver(
        tenant_id,
        client_id,
        client_secret,
        subscription_id,
        RESOURCE_GROUP,
        login_key,
    )
    .ensure_resource_group()
    .await
    .map_err(rejected)?;

    tracing::info!(group = RESOURCE_GROUP, %region, "prepared an Azure subscription");
    Ok(Some(RESOURCE_GROUP.to_owned()))
}

impl CloudLink for LiveClouds {
    async fn prepare(
        &self,
        credentials: &ProviderCredentials,
        login_key: &LoginKey,
    ) -> Result<Option<String>, ApiError> {
        verify(credentials).await?;
        azure_workspace(credentials, login_key).await
    }
}

/// The cloud side of linking, as the router carries it.
///
/// An enum rather than a type parameter, for the reason every other vendor
/// client here is one: `#[skyzen::openapi]` cannot annotate a generic
/// handler.
#[derive(Debug, Clone)]
pub enum Clouds {
    /// Talks to AWS, Azure and Google.
    Live(LiveClouds),
    /// Answers without a network, for tests.
    #[cfg(test)]
    Fake(crate::testing::TestClouds),
}

impl Default for Clouds {
    fn default() -> Self {
        Self::Live(LiveClouds::new())
    }
}

impl CloudLink for Clouds {
    async fn prepare(
        &self,
        credentials: &ProviderCredentials,
        login_key: &LoginKey,
    ) -> Result<Option<String>, ApiError> {
        match self {
            Self::Live(live) => live.prepare(credentials, login_key).await,
            #[cfg(test)]
            Self::Fake(fake) => fake.prepare(credentials, login_key).await,
        }
    }
}

/// A driver's refusal, as the problem the user reads.
///
/// A provider that *rejected* the credential said something worth
/// repeating ("Client application has no configured keys"); that sentence
/// is the reason, without the driver's own "provider rejected the request"
/// wrapper in front of it. Any other failure keeps its full description.
pub(crate) fn rejected(error: ProviderError) -> ApiError {
    ApiError::ProviderRejectedCredentials {
        reason: match error {
            ProviderError::Rejected(reason) => reason,
            other => other.to_string(),
        },
    }
}
