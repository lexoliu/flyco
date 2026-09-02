//! Linked cloud-provider accounts, the bonus questionnaire, and the cloud
//! usage panel.
//!
//! Flyco provisions into the *user's own* cloud accounts, so linking one is
//! the step between signing in and running anything. Credentials are sealed
//! by [`crate::crypto::TokenCipher`] on the way in and never come back out:
//! no route here returns a credential, and
//! [`ProviderAccountView`](flyco_core::ProviderAccountView) has no field
//! that could hold one.

use askama::Template;
use flyco_core::{
    AwsIamPolicy, CloudProviderKind, CloudUsageView, CurrentUser, HostId, LinkProvider,
    ProviderAccountId, ProviderAccountView, ProviderBonusHint, ProviderCredentials,
    QuickstartAnswers, UserId,
};
use flyco_provider::aws::iam;
use flyco_provider::azure::RESOURCE_GROUP;
use serde::Deserialize;
use skyzen::extract::Query;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::sql;
use skyzen::utils::{Json, State};
use skyzen_services::Db;

use crate::bonuses;
use crate::clock::now_unix;
use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::extract::path_id;
use crate::problem::Outcome;
use crate::respond::{Created, NoContent};

/// The columns every read on this path projects.
///
/// `credentials_enc` is deliberately absent: a credential that is never
/// selected cannot be leaked by a later edit to a response type.
#[derive(Debug, skyzen::FromRow)]
struct AccountRow {
    id: ProviderAccountId,
    kind: CloudProviderKind,
    label: String,
    linked_at_unix: u64,
    host_id: Option<HostId>,
}

impl From<AccountRow> for ProviderAccountView {
    fn from(row: AccountRow) -> Self {
        Self {
            id: row.id,
            kind: row.kind,
            label: row.label,
            linked_at_unix: row.linked_at_unix,
            host_id: row.host_id,
        }
    }
}

/// Narrows the cloud usage panel to one provider.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
pub struct CloudUsageFilter {
    /// Only accounts with this provider.
    pub provider: Option<CloudProviderKind>,
}

/// Lists the caller's linked cloud-provider accounts.
#[skyzen::openapi]
async fn list_providers(
    State(user): State<CurrentUser>,
    db: Db,
) -> Outcome<Json<Vec<ProviderAccountView>>> {
    list(&db, user.id).await.map(Json).into()
}

async fn list(db: &Db, user: UserId) -> Result<Vec<ProviderAccountView>, ApiError> {
    // A drained host keeps its account row, because the machines that ran
    // there still point at it — but it is not an account anybody can
    // provision through any more, so it is not one to list.
    let removed = flyco_core::HostState::Removed;
    let rows: Vec<AccountRow> = sql!(
        db,
        "SELECT provider_accounts.id, kind, provider_accounts.label, linked_at_unix, host_id \
         FROM provider_accounts \
         LEFT JOIN hosts ON hosts.id = provider_accounts.host_id \
         WHERE provider_accounts.user_id = {user} \
         AND (host_id IS NULL OR hosts.state != {removed}) \
         ORDER BY linked_at_unix DESC, provider_accounts.id"
    )
    .fetch_all()
    .await?;

    Ok(rows.into_iter().map(Into::into).collect())
}

/// Links a cloud-provider account, sealing its credentials at rest.
#[skyzen::openapi]
async fn link_provider(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    Json(request): Json<LinkProvider>,
    db: Db,
) -> Outcome<Created<Json<ProviderAccountView>>> {
    link(&db, &config, user.id, request)
        .await
        .map(|view| Created(Json(view)))
        .into()
}

/// Proves the credentials work before storing them.
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
///   See [`provision_azure_workspace`].
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
/// A machine the user owns is not linkable here at all: it is enrolled, and
/// enrolling is what proves it exists.
async fn verify(credentials: &ProviderCredentials) -> Result<(), ApiError> {
    match credentials {
        // Azure is checked by [`provision_azure_workspace`] instead, which
        // has to run anyway and proves strictly more: a token proves the
        // secret is right, and creating the resource group proves the
        // principal is scoped widely enough to be useful.
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
        .map_err(|error| ApiError::ProviderRejectedCredentials {
            reason: error.to_string(),
        }),
        ProviderCredentials::Gcp {
            service_account_json,
        } => {
            let mut provider =
                crate::provisioning::gcp_driver(service_account_json).map_err(|error| {
                    ApiError::ProviderRejectedCredentials {
                        reason: error.to_string(),
                    }
                })?;
            provider.mint_token().await.map(|_| ()).map_err(|error| {
                ApiError::ProviderRejectedCredentials {
                    reason: error.to_string(),
                }
            })
        }
    }
}

/// Creates the resource group flyco owns inside a freshly linked Azure
/// subscription.
///
/// The user never names one. The service principal the wizard mints is
/// `Contributor` on the whole subscription, which is exactly the scope a
/// resource-group creation needs, so flyco makes the group itself — and the
/// `PUT` doubles as the credential check, because it exercises the token
/// *and* the role assignment rather than only the secret.
///
/// Answers with the group's name, which is stored on the account so a
/// subscription linked today keeps the group it owns if flyco's default name
/// ever changes.
async fn provision_azure_workspace(
    credentials: &ProviderCredentials,
) -> Result<Option<String>, ApiError> {
    let ProviderCredentials::Azure {
        tenant_id,
        client_id,
        client_secret,
        subscription_id,
        admin_ssh_public_key,
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
        admin_ssh_public_key,
    )
    .ensure_resource_group()
    .await
    .map_err(|error| ApiError::ProviderRejectedCredentials {
        reason: error.to_string(),
    })?;

    tracing::info!(group = RESOURCE_GROUP, %region, "prepared an Azure subscription");
    Ok(Some(RESOURCE_GROUP.to_owned()))
}

async fn link(
    db: &Db,
    config: &ApiConfig,
    user: UserId,
    request: LinkProvider,
) -> Result<ProviderAccountView, ApiError> {
    verify(&request.credentials).await?;
    let resource_group = provision_azure_workspace(&request.credentials).await?;
    create_with(
        db,
        config,
        user,
        request.label,
        &request.credentials,
        resource_group,
        None,
    )
    .await
}

/// Writes the account row a set of credentials opens.
///
/// Shared with host enrollment, which links an account of its own without
/// going anywhere near `POST /v1/providers`: a machine the user owns has to
/// be a provider account, or the compute chip, the curated catalog, session
/// creation and the usage panel would each need a special case for it.
///
/// # Errors
///
/// Returns [`ApiError`] if the credentials cannot be sealed or the write
/// fails.
pub(crate) async fn create(
    db: &Db,
    config: &ApiConfig,
    user: UserId,
    label: String,
    credentials: &ProviderCredentials,
    host: Option<HostId>,
) -> Result<ProviderAccountView, ApiError> {
    create_with(db, config, user, label, credentials, None, host).await
}

async fn create_with(
    db: &Db,
    config: &ApiConfig,
    user: UserId,
    label: String,
    credentials: &ProviderCredentials,
    resource_group: Option<String>,
    host: Option<HostId>,
) -> Result<ProviderAccountView, ApiError> {
    let kind = credentials.kind();
    let sealed = config.token_cipher().seal(
        &serde_json::to_string(credentials)
            .map_err(|_| ApiError::CorruptRecord("credentials could not be encoded"))?,
    )?;

    let id = ProviderAccountId::generate();
    let linked_at = now_unix();
    sql!(
        db,
        "INSERT INTO provider_accounts \
         (id, user_id, kind, label, credentials_enc, resource_group, host_id, linked_at_unix) \
         VALUES ({id}, {user}, {kind}, {label.clone()}, {sealed}, {resource_group}, {host}, \
                 {linked_at})"
    )
    .execute()
    .await?;

    tracing::info!(?kind, "linked a provider account");

    Ok(ProviderAccountView {
        id,
        kind,
        label,
        linked_at_unix: linked_at,
        host_id: host,
    })
}

/// Unlinks a cloud-provider account.
#[skyzen::openapi]
async fn unlink_provider(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Outcome<NoContent> {
    unlink(&db, user.id, &params).await.into()
}

async fn unlink(db: &Db, user: UserId, params: &Params) -> Result<NoContent, ApiError> {
    let id: ProviderAccountId = path_id(params, "id")?;

    // Scoped by user so somebody else's account is indistinguishable from
    // one that does not exist.
    let owned: Option<AccountRow> = sql!(
        db,
        "SELECT id, kind, label, linked_at_unix, host_id FROM provider_accounts \
         WHERE id = {id} AND user_id = {user}"
    )
    .fetch_optional()
    .await?;
    if owned.is_none() {
        return Err(ApiError::ProviderAccountNotFound);
    }

    // Unlinking discards the only credentials that can destroy what is
    // running there, so a live machine makes this a 409 rather than a leak
    // nobody can clean up afterwards.
    let destroyed = flyco_core::MachineState::Destroyed;
    let live: u64 = sql!(
        db,
        "SELECT COUNT(*) AS live FROM machines \
         WHERE provider_account_id = {id} AND state != {destroyed}"
    )
    .fetch_scalar()
    .await?;
    if live > 0 {
        return Err(ApiError::ProviderInUse { sessions: live });
    }

    sql!(
        db,
        "DELETE FROM provider_accounts WHERE id = {id} AND user_id = {user}"
    )
    .execute()
    .await?;

    Ok(NoContent)
}

/// The minimal IAM policy, as a document ready to paste into IAM.
///
/// A compiled template over `flyco_provider::aws::iam::actions()`, which is
/// itself derived from the driver's own call sites, so the policy the wizard
/// shows cannot drift from the permissions the driver needs — see that
/// module for the test that holds it to them.
#[derive(Debug, Template)]
#[template(path = "aws/iam_policy.json", escape = "none")]
struct IamPolicyDocument {
    /// Every action to grant, `service:Action`, sorted.
    actions: Vec<String>,
}

/// Shows the least privilege an AWS access key needs to run flyco sessions.
///
/// Served rather than checked into the frontend because it is a fact about
/// this build of the driver: a copy in a template somewhere else would be
/// right on the day it was written and quietly wrong afterwards.
#[skyzen::openapi]
async fn aws_iam_policy(State(_user): State<CurrentUser>) -> Outcome<Json<AwsIamPolicy>> {
    iam_policy().map(Json).into()
}

fn iam_policy() -> Result<AwsIamPolicy, ApiError> {
    let actions = iam::actions();
    let document = IamPolicyDocument {
        actions: actions.clone(),
    }
    .render()
    .map_err(|_| ApiError::CorruptRecord("the AWS IAM policy template did not render"))?;

    Ok(AwsIamPolicy { document, actions })
}

/// Suggests free-credit programmes the caller qualifies for.
///
/// Two questions decide it — new customer, student — because those are the
/// two facts every provider's credit programme keys on.
#[skyzen::openapi]
async fn provider_quickstart(
    State(_user): State<CurrentUser>,
    Json(answers): Json<QuickstartAnswers>,
) -> Outcome<Json<Vec<ProviderBonusHint>>> {
    Ok(Json(bonuses::matching(&answers))).into()
}

/// Reports metered cloud spend per linked account.
///
/// The provider's own meter is the authority — a total flyco assembled from
/// its machine records would miss the storage, egress and support charges
/// on the same invoice — so every row here was read from the account it
/// describes, over the window it names.
///
/// Not every linked account produces a row. A machine the user owns is
/// hardware they already pay for: flyco meters nothing there and says
/// nothing, rather than reporting a `$0.00` that would read as "this costs
/// nothing".
#[skyzen::openapi]
async fn cloud_usage(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    Query(filter): Query<CloudUsageFilter>,
    db: Db,
) -> Outcome<Json<Vec<CloudUsageView>>> {
    usage(&db, &config, user.id, filter.provider)
        .await
        .map(Json)
        .into()
}

/// Reads every linked account's meter into one document.
///
/// An account whose usage cannot be read is skipped with a warning rather
/// than failing the request, exactly as the machine catalog does: one
/// expired credential must not hide every other account's spend.
async fn usage(
    db: &Db,
    config: &ApiConfig,
    user: UserId,
    provider: Option<CloudProviderKind>,
) -> Result<Vec<CloudUsageView>, ApiError> {
    let accounts = crate::provisioning::accounts_for(db, config, user, provider).await?;
    let now = now_unix();

    let mut rows = Vec::new();
    for account in accounts {
        match crate::provisioning::cloud_usage(&account, now).await {
            Ok(Some(spend)) => rows.push(CloudUsageView::of(account.id, account.kind(), spend)),
            Ok(None) => {
                tracing::debug!(
                    account = %account.id,
                    kind = ?account.kind(),
                    "this provider meters nothing on flyco's behalf"
                );
            }
            Err(error) => {
                tracing::warn!(
                    account = %account.id,
                    %error,
                    "skipping a provider account whose usage could not be read"
                );
            }
        }
    }
    Ok(rows)
}

/// The user-scoped provider-account routes.
pub fn routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/providers".at(list_providers).post(link_provider),
        "/v1/providers/aws/iam-policy".at(aws_iam_policy),
        "/v1/providers/quickstart".post(provider_quickstart),
        "/v1/providers/{id}".delete(unlink_provider),
        "/v1/usage/cloud".at(cloud_usage),
    ))
    .into_route_nodes()
}
