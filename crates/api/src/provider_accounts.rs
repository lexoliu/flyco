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
use flyco_provider::LoginKey;
use flyco_provider::aws::iam;
use serde::{Deserialize, Serialize};
use skyzen::extract::Query;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::sql;
use skyzen::utils::{Json, State};
use skyzen_services::{Db, Kv, Queue};

use crate::bonuses;
use crate::catalog;
use crate::clock::now_unix;
use crate::clouds::{CloudLink as _, Clouds};
use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::extract::path_id;
use crate::github::{GithubClient, GithubOauth};
use crate::problem::Outcome;
use crate::respond::{Created, NoContent};

/// What `credentials_enc` holds once an account has been unlinked.
///
/// The column is `NOT NULL` and cannot be relaxed without rebuilding a
/// table every machine row references, so the scrub writes the one string
/// that is not a sealed anything. Nothing ever unseals it — every read that
/// resolves an account to provision through skips unlinked rows — and if
/// one ever did, [`crate::crypto::TokenCipher::open`] rejects it as
/// truncated. A missed filter is therefore a loud failure rather than a
/// silent provision against a credential the user withdrew.
const SCRUBBED_CREDENTIAL: &str = "";

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
    // Two ways an account row outlives the thing it provisions through, and
    // neither is one to list. A drained host keeps its account because the
    // machines that ran there still point at it; an unlinked account keeps
    // its row for exactly the same reason, and is told apart by the stamp
    // rather than by the credential it no longer holds.
    let removed = flyco_core::HostState::Removed;
    let rows: Vec<AccountRow> = sql!(
        db,
        "SELECT provider_accounts.id, kind, provider_accounts.label, linked_at_unix, host_id \
         FROM provider_accounts \
         LEFT JOIN hosts ON hosts.id = provider_accounts.host_id \
         WHERE provider_accounts.user_id = {user} \
         AND provider_accounts.unlinked_at_unix IS NULL \
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
    State(clouds): State<Clouds>,
    Json(request): Json<LinkProvider>,
    db: Db,
    kv: Kv,
    queue: Queue,
) -> Outcome<Created<Json<ProviderAccountView>>> {
    link(&db, &config, &clouds, &kv, &queue, user.id, request)
        .await
        .map(|view| Created(Json(view)))
        .into()
}

/// Links one account: prove the credentials, prepare what flyco owns inside
/// the account, seal, store.
///
/// The one store path, shared by `POST /v1/providers` and by both
/// "Sign in with…" flows, so validation and storage stay one rule rather
/// than three that drift.
///
/// # Errors
///
/// Returns [`ApiError`] if the provider refuses the credentials, the
/// credentials cannot be sealed, or the write fails.
pub(crate) async fn link(
    db: &Db,
    config: &ApiConfig,
    clouds: &Clouds,
    kv: &Kv,
    queue: &Queue,
    user: UserId,
    request: LinkProvider,
) -> Result<ProviderAccountView, ApiError> {
    // Flyco's, minted here: the browser never sees a key, and the user has
    // nowhere to keep one.
    let login_key = LoginKey::generate();
    let resource_group = clouds.prepare(&request.credentials, &login_key).await?;
    let view = create_with(
        db,
        config,
        user,
        request.label,
        SealedSecrets {
            credentials: &request.credentials,
            machine_login_key: &login_key,
        },
        resource_group,
        None,
    )
    .await?;

    // The account is linked and its catalog has never been read, so the
    // read starts now rather than when somebody first looks: the next screen
    // the user sees is the compute card, and it is asking what this account
    // can deploy.
    catalog::ask_for_refresh(kv, queue, user, view.id).await?;
    Ok(view)
}

/// What `credentials_enc` seals: the credentials the user presented, and
/// the machine login key flyco minted for the account.
///
/// Borrowed for sealing, so a link does not clone secrets it is about to
/// encrypt; [`StoredSecrets`] is the same document read back.
#[derive(Serialize)]
struct SealedSecrets<'a> {
    credentials: &'a ProviderCredentials,
    machine_login_key: &'a LoginKey,
}

/// [`SealedSecrets`], unsealed.
#[derive(Deserialize)]
pub(crate) struct StoredSecrets {
    pub(crate) credentials: ProviderCredentials,
    pub(crate) machine_login_key: LoginKey,
}

impl StoredSecrets {
    /// The sealed document this value writes back as.
    ///
    /// The renewal path's half of `create`: a grant refreshed at load time
    /// is resealed whole, so the row keeps carrying every field it was
    /// written with rather than only the one that changed.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError`] if the credentials cannot be encoded or sealed.
    pub(crate) fn seal(&self, config: &ApiConfig) -> Result<String, ApiError> {
        Ok(config.token_cipher().seal(
            &serde_json::to_string(&SealedSecrets {
                credentials: &self.credentials,
                machine_login_key: &self.machine_login_key,
            })
            .map_err(|_| ApiError::CorruptRecord("credentials could not be encoded"))?,
        )?)
    }
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
    let login_key = LoginKey::generate();
    create_with(
        db,
        config,
        user,
        label,
        SealedSecrets {
            credentials,
            machine_login_key: &login_key,
        },
        None,
        host,
    )
    .await
}

async fn create_with(
    db: &Db,
    config: &ApiConfig,
    user: UserId,
    label: String,
    secrets: SealedSecrets<'_>,
    resource_group: Option<String>,
    host: Option<HostId>,
) -> Result<ProviderAccountView, ApiError> {
    let kind = secrets.credentials.kind();
    let sealed = config.token_cipher().seal(
        &serde_json::to_string(&secrets)
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
///
/// The row survives the unlink and the credential does not. Every machine
/// flyco ever built there still names this account — that is the spend
/// history the budget ledger explains — so deleting the row would either
/// fail the foreign key holding the history together or, if it cascaded,
/// erase it. What is deleted is the only part that matters: the sealed
/// credential is scrubbed, the account is stamped unlinked, and it stops
/// appearing anywhere flyco offers something to provision through.
///
/// Linking the same cloud account again writes a new row. This one is
/// history from here on, and reading it back by id is a 404 like any
/// account the caller does not have.
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
    // one that does not exist — and by the stamp, so an account already
    // unlinked is too. Unlinking twice is not idempotent success: the
    // second call names a row nothing can be done with any more, and
    // answering `204` would report that a credential had just been
    // withdrawn when none was there to withdraw.
    let owned: Option<AccountRow> = sql!(
        db,
        "SELECT id, kind, label, linked_at_unix, host_id FROM provider_accounts \
         WHERE id = {id} AND user_id = {user} AND unlinked_at_unix IS NULL"
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
    let live: u32 = sql!(
        db,
        "SELECT COUNT(*) AS live FROM machines \
         WHERE provider_account_id = {id} AND state != {destroyed}"
    )
    .fetch_scalar()
    .await?;
    if live > 0 {
        return Err(ApiError::ProviderInUse { sessions: live });
    }

    // Scrubbed and stamped in one statement, because a row that had lost
    // its credential without being marked unlinked would be an account the
    // API still offers and nothing can provision through.
    sql!(
        db,
        "UPDATE provider_accounts \
         SET credentials_enc = {SCRUBBED_CREDENTIAL}, unlinked_at_unix = {now_unix()} \
         WHERE id = {id} AND user_id = {user}"
    )
    .execute()
    .await?;

    tracing::info!(account = %id, "unlinked a provider account");
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
    State(github): State<GithubClient>,
    Query(filter): Query<CloudUsageFilter>,
    db: Db,
) -> Outcome<Json<Vec<CloudUsageView>>> {
    usage(&db, &config, &github, user.id, filter.provider)
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
    github: &impl GithubOauth,
    user: UserId,
    provider: Option<CloudProviderKind>,
) -> Result<Vec<CloudUsageView>, ApiError> {
    let accounts = crate::provisioning::accounts_for(db, config, github, user, provider).await?;
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
