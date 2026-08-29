//! Linked cloud-provider accounts, the bonus questionnaire, and the cloud
//! usage panel.
//!
//! Flyco provisions into the *user's own* cloud accounts, so linking one is
//! the step between signing in and running anything. Credentials are sealed
//! by [`crate::crypto::TokenCipher`] on the way in and never come back out:
//! no route here returns a credential, and
//! [`ProviderAccountView`](flyco_core::ProviderAccountView) has no field
//! that could hold one.

use flyco_core::{
    CloudProviderKind, CloudUsageView, CurrentUser, LinkProvider, ProviderAccountId,
    ProviderAccountView, ProviderBonusHint, ProviderCredentials, QuickstartAnswers, UserId,
};
use flyco_provider::azure::auth::{ServicePrincipal, TokenCache};
use flyco_provider::{SystemClock, ZenwaveTransport};
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
}

impl From<AccountRow> for ProviderAccountView {
    fn from(row: AccountRow) -> Self {
        Self {
            id: row.id,
            kind: row.kind,
            label: row.label,
            linked_at_unix: row.linked_at_unix,
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
    let rows: Vec<AccountRow> = sql!(
        db,
        "SELECT id, kind, label, linked_at_unix FROM provider_accounts \
         WHERE user_id = {user} ORDER BY linked_at_unix DESC, id"
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
/// * **Azure** — an access token, which is what the whole service-principal
///   triple is for.
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
/// A registered SSH host cannot be reached from the Worker at all (it has no
/// sockets, and the executor is native-only), so its credentials are checked
/// structurally here and verified in full, host key included, by the first
/// job that dials it.
async fn verify(credentials: &ProviderCredentials) -> Result<(), ApiError> {
    match credentials {
        ProviderCredentials::Azure {
            tenant_id,
            client_id,
            client_secret,
            subscription_id,
            ..
        } => {
            let mut tokens = TokenCache::new(ServicePrincipal {
                tenant_id: tenant_id.clone(),
                client_id: client_id.clone(),
                client_secret: client_secret.clone(),
                subscription_id: subscription_id.clone(),
            });
            tokens
                .access_token(&ZenwaveTransport::new(), &SystemClock::default())
                .await
                .map(|_| ())
                .map_err(|error| ApiError::ProviderRejectedCredentials {
                    reason: error.to_string(),
                })
        }
        ProviderCredentials::ByoSsh {
            host,
            user,
            private_key,
            host_fingerprint,
            ..
        } => {
            if host.trim().is_empty()
                || user.trim().is_empty()
                || private_key.trim().is_empty()
                || !host_fingerprint.starts_with("SHA256:")
            {
                return Err(ApiError::ProviderRejectedCredentials {
                    reason: "host, user, private key and a SHA256: host fingerprint are all \
                             required"
                        .to_owned(),
                });
            }
            Ok(())
        }
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

async fn link(
    db: &Db,
    config: &ApiConfig,
    user: UserId,
    request: LinkProvider,
) -> Result<ProviderAccountView, ApiError> {
    verify(&request.credentials).await?;

    let kind = request.credentials.kind();
    let sealed = config.token_cipher().seal(
        &serde_json::to_string(&request.credentials)
            .map_err(|_| ApiError::CorruptRecord("credentials could not be encoded"))?,
    )?;

    let id = ProviderAccountId::generate();
    let linked_at = now_unix();
    sql!(
        db,
        "INSERT INTO provider_accounts \
         (id, user_id, kind, label, credentials_enc, linked_at_unix) \
         VALUES ({id}, {user}, {kind}, {request.label.clone()}, {sealed}, {linked_at})"
    )
    .execute()
    .await?;

    tracing::info!(?kind, "linked a cloud provider account");

    Ok(ProviderAccountView {
        id,
        kind,
        label: request.label,
        linked_at_unix: linked_at,
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
        "SELECT id, kind, label, linked_at_unix FROM provider_accounts \
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
/// Not every linked account produces a row. A registered SSH host is
/// hardware the user already owns and already pays for: flyco meters
/// nothing there and says nothing, rather than reporting a `$0.00` that
/// would read as "this costs nothing".
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
        "/v1/providers/quickstart".post(provider_quickstart),
        "/v1/providers/{id}".delete(unlink_provider),
        "/v1/usage/cloud".at(cloud_usage),
    ))
    .into_route_nodes()
}
