//! Turning a stored provider account into a live driver.
//!
//! Credentials are sealed at rest, so every provisioning path starts here:
//! unseal exactly one account's credentials, build the concrete driver they
//! open, and hand it back. The [`CloudProvider`](flyco_provider::CloudProvider)
//! trait is deliberately not object-safe — it keeps every future free of
//! boxing on wasm32 — so the dispatch is a `match` on the credential variant
//! rather than a trait object, and each operation names the shapes it
//! supports.

use flyco_core::{
    CloudProviderKind, CloudSpend, MachineCatalogEntry, ProviderAccountId, ProviderCredentials,
    UserId,
};
use flyco_provider::aws::sigv4::AccessKey;
use flyco_provider::aws::{AwsProvider, AwsWorkspace};
use flyco_provider::azure::auth::ServicePrincipal;
use flyco_provider::azure::{AzureProvider, Workspace};
use flyco_provider::byo_ssh::ByoSsh;
use flyco_provider::gcp::auth::ServiceAccountKey;
use flyco_provider::gcp::{GcpProvider, GcpWorkspace};
use flyco_provider::{CloudProvider, ProviderError};
use skyzen::sql;
use skyzen_services::Db;

use crate::config::ApiConfig;
use crate::error::ApiError;

/// One linked account, with its credentials unsealed for immediate use.
///
/// Held only for the length of an operation. Nothing here is serialized,
/// logged, or returned: `Debug` is written by hand for the same reason the
/// credentials themselves are.
pub struct LinkedAccount {
    /// Which account this is, for correlating a failure with a row.
    pub id: ProviderAccountId,
    /// The unsealed credentials.
    credentials: ProviderCredentials,
}

impl core::fmt::Debug for LinkedAccount {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LinkedAccount")
            .field("id", &self.id)
            .field("kind", &self.credentials.kind())
            .finish_non_exhaustive()
    }
}

impl LinkedAccount {
    /// Which provider this account opens.
    #[must_use]
    pub const fn kind(&self) -> CloudProviderKind {
        self.credentials.kind()
    }
}

/// The Azure driver one set of Azure credentials opens.
///
/// Every dispatch below needs the same construction, so it is written once:
/// a credential field added or renamed is one edit rather than three that
/// can drift apart.
pub(crate) fn azure_driver(
    tenant_id: &str,
    client_id: &str,
    client_secret: &str,
    subscription_id: &str,
    resource_group: &str,
    admin_ssh_public_key: &str,
) -> AzureProvider {
    AzureProvider::new(
        ServicePrincipal {
            tenant_id: tenant_id.to_owned(),
            client_id: client_id.to_owned(),
            client_secret: client_secret.to_owned(),
            subscription_id: subscription_id.to_owned(),
        },
        Workspace::new(resource_group, admin_ssh_public_key),
    )
}

/// The AWS driver one access key opens.
pub(crate) fn aws_driver(
    access_key_id: &str,
    secret_access_key: &str,
    session_token: Option<&str>,
    key_name: Option<&str>,
) -> AwsProvider {
    let mut key = AccessKey::new(access_key_id, secret_access_key);
    if let Some(token) = session_token {
        key = key.with_session_token(token);
    }

    let mut workspace = AwsWorkspace::new();
    if let Some(key_name) = key_name {
        workspace = workspace.with_key_pair(key_name);
    }
    AwsProvider::new(key, workspace)
}

/// The GCP driver one service-account key opens.
///
/// Fallible where the other two are not: a Google credential is a *document*
/// rather than a set of fields, and one that is not a service-account key is
/// a credential to fix rather than a failure to retry.
///
/// # Errors
///
/// Returns [`ProviderError::Malformed`] if the document is not a
/// service-account key.
pub(crate) fn gcp_driver(service_account_json: &str) -> Result<GcpProvider, ProviderError> {
    Ok(GcpProvider::new(
        ServiceAccountKey::parse(service_account_json)?,
        GcpWorkspace::new(),
    ))
}

#[derive(Debug, skyzen::FromRow)]
struct SealedRow {
    id: ProviderAccountId,
    credentials_enc: String,
}

/// Loads the caller's linked accounts, optionally narrowed to one provider.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails, or if a stored credential
/// cannot be unsealed or decoded — which means the row was written by a
/// different key and is not something to paper over.
pub async fn accounts_for(
    db: &Db,
    config: &ApiConfig,
    user: UserId,
    kind: Option<CloudProviderKind>,
) -> Result<Vec<LinkedAccount>, ApiError> {
    // A NULL filter matches every provider, which keeps one statement for
    // both callers rather than assembling SQL per request.
    let rows: Vec<SealedRow> = sql!(
        db,
        "SELECT id, credentials_enc FROM provider_accounts \
         WHERE user_id = {user} AND ({kind} IS NULL OR kind = {kind}) \
         ORDER BY linked_at_unix"
    )
    .fetch_all()
    .await?;

    let cipher = config.token_cipher();
    rows.into_iter()
        .map(|row| {
            let credentials: ProviderCredentials =
                serde_json::from_str(&cipher.open(&row.credentials_enc)?).map_err(|_| {
                    ApiError::CorruptRecord("provider_accounts.credentials_enc is not credentials")
                })?;
            Ok(LinkedAccount {
                id: row.id,
                credentials,
            })
        })
        .collect()
}

/// Loads exactly one of the caller's accounts.
///
/// # Errors
///
/// Returns [`ApiError::ProviderAccountNotFound`] when the account does not
/// exist or belongs to somebody else — the two are indistinguishable.
pub async fn account(
    db: &Db,
    config: &ApiConfig,
    user: UserId,
    id: ProviderAccountId,
) -> Result<LinkedAccount, ApiError> {
    let row: SealedRow = sql!(
        db,
        "SELECT id, credentials_enc FROM provider_accounts \
         WHERE id = {id} AND user_id = {user}"
    )
    .fetch_optional()
    .await?
    .ok_or(ApiError::ProviderAccountNotFound)?;

    let credentials: ProviderCredentials =
        serde_json::from_str(&config.token_cipher().open(&row.credentials_enc)?).map_err(|_| {
            ApiError::CorruptRecord("provider_accounts.credentials_enc is not credentials")
        })?;

    Ok(LinkedAccount { id, credentials })
}

/// Reads what an account can actually deploy today.
///
/// For every cloud provider that is the intersection of the independent
/// gates its API exposes — availability, quota, and whatever the account
/// itself forbids — so an entry that comes back is one a provision request
/// will be allowed to create. A registered SSH host reports the single
/// machine it is, unpriced: the user already owns it.
///
/// # Errors
///
/// Returns [`ProviderError`] if the provider rejects the credentials or
/// cannot be reached, and for providers flyco has no driver for yet.
pub async fn catalog(account: &LinkedAccount) -> Result<Vec<MachineCatalogEntry>, ProviderError> {
    match &account.credentials {
        ProviderCredentials::Azure {
            tenant_id,
            client_id,
            client_secret,
            subscription_id,
            resource_group,
            admin_ssh_public_key,
        } => {
            azure_driver(
                tenant_id,
                client_id,
                client_secret,
                subscription_id,
                resource_group,
                admin_ssh_public_key,
            )
            .catalog()
            .await
        }
        ProviderCredentials::ByoSsh { host, .. } => Ok(ByoSsh::new(host.clone()).catalog()),
        ProviderCredentials::Aws {
            access_key_id,
            secret_access_key,
            session_token,
            key_name,
        } => {
            aws_driver(
                access_key_id,
                secret_access_key,
                session_token.as_deref(),
                key_name.as_deref(),
            )
            .catalog()
            .await
        }
        ProviderCredentials::Gcp {
            service_account_json,
        } => gcp_driver(service_account_json)?.catalog().await,
    }
}

/// Reads what the provider's own meter says this account has been billed
/// over its current billing period.
///
/// `None` is not a failure and not a zero: it means flyco has no metered
/// number to report, for one of two reasons. A registered SSH host is
/// hardware the user already owns and already pays for, so there is nothing
/// to meter; a GCP project has plenty to meter and no API that will say so,
/// because Google delivers billing data through a `BigQuery` export the user
/// configures rather than through a running total a service account can
/// read. Either way the answer is silence rather than a `$0.00` that would
/// read as "this costs nothing".
///
/// Exposed here for the same reason [`catalog`] is: the driver trait is not
/// object-safe, so the dispatch on the credential variant lives in the
/// control plane and each arm names what it supports.
///
/// # Errors
///
/// Returns [`ProviderError`] if the provider refuses the read or cannot be
/// reached, and for providers flyco has no driver for yet.
pub async fn cloud_usage(
    account: &LinkedAccount,
    now_unix: u64,
) -> Result<Option<CloudSpend>, ProviderError> {
    match &account.credentials {
        ProviderCredentials::Azure {
            tenant_id,
            client_id,
            client_secret,
            subscription_id,
            resource_group,
            admin_ssh_public_key,
        } => azure_driver(
            tenant_id,
            client_id,
            client_secret,
            subscription_id,
            resource_group,
            admin_ssh_public_key,
        )
        .billing_period_cost(now_unix)
        .await
        .map(Some),
        ProviderCredentials::Aws {
            access_key_id,
            secret_access_key,
            session_token,
            key_name,
        } => aws_driver(
            access_key_id,
            secret_access_key,
            session_token.as_deref(),
            key_name.as_deref(),
        )
        .billing_period_cost(now_unix)
        .await
        .map(Some),
        // Two providers contribute no row, for the two different reasons
        // this function's documentation gives: a registered SSH host has
        // nothing flyco meters, and a GCP project has plenty and no API that
        // will say so. The answer is the same because the honest answer to
        // "how much has this cost" is silence in both cases.
        ProviderCredentials::ByoSsh { .. } | ProviderCredentials::Gcp { .. } => Ok(None),
    }
}

/// One lifecycle operation against an already-provisioned machine.
///
/// Each arm dispatches on the credential variant for the same reason
/// [`catalog`] does: the driver trait is not object-safe, deliberately, and
/// what an operation *means* is written once in [`Operation::run`] rather
/// than once per provider. Every cloud driver implements these; a registered
/// SSH host does not, because it cannot be resized and its container
/// lifecycle is executed natively rather than from the Worker.
///
/// # Errors
///
/// Returns [`ProviderError`] if the provider refuses the operation or
/// cannot be reached.
pub async fn operate(
    account: &LinkedAccount,
    machine: &flyco_provider::Machine,
    operation: Operation<'_>,
) -> Result<flyco_provider::Machine, ProviderError> {
    match &account.credentials {
        ProviderCredentials::Azure {
            tenant_id,
            client_id,
            client_secret,
            subscription_id,
            resource_group,
            admin_ssh_public_key,
        } => {
            operation
                .run(
                    &mut azure_driver(
                        tenant_id,
                        client_id,
                        client_secret,
                        subscription_id,
                        resource_group,
                        admin_ssh_public_key,
                    ),
                    machine,
                )
                .await
        }
        ProviderCredentials::ByoSsh { .. } => Err(ProviderError::Unsupported {
            provider: "byo-ssh",
            operation: operation.name(),
            reason: "a host you own is started and stopped by you, not by flyco",
        }),
        ProviderCredentials::Aws {
            access_key_id,
            secret_access_key,
            session_token,
            key_name,
        } => {
            operation
                .run(
                    &mut aws_driver(
                        access_key_id,
                        secret_access_key,
                        session_token.as_deref(),
                        key_name.as_deref(),
                    ),
                    machine,
                )
                .await
        }
        ProviderCredentials::Gcp {
            service_account_json,
        } => {
            operation
                .run(&mut gcp_driver(service_account_json)?, machine)
                .await
        }
    }
}

/// What [`operate`] should do to a machine.
#[derive(Debug, Clone, Copy)]
pub enum Operation<'a> {
    /// Move to another machine type, keeping the disk.
    Resize {
        /// Provider-native machine type to move to.
        machine_type: &'a str,
    },
    /// Release compute, keep the disk.
    Stop,
    /// Put a deallocated machine back on compute.
    Start,
}

impl Operation<'_> {
    /// The operation's name, for an error a user has to act on.
    const fn name(self) -> &'static str {
        match self {
            Self::Resize { .. } => "resize",
            Self::Stop => "stop",
            Self::Start => "start",
        }
    }

    /// Performs this operation against one driver.
    ///
    /// Generic over the driver rather than repeated per credential variant:
    /// the trait is not object-safe on purpose, and a generic function is
    /// what keeps every future unboxed while still writing "what a stop
    /// means" exactly once. A stop is the one that has something to say —
    /// the driver answers with nothing, and the machine a caller gets back
    /// has to say it is deallocated.
    async fn run<P: CloudProvider>(
        self,
        provider: &mut P,
        machine: &flyco_provider::Machine,
    ) -> Result<flyco_provider::Machine, ProviderError> {
        match self {
            Self::Resize { machine_type } => provider.resize(machine, machine_type).await,
            Self::Stop => provider.deallocate(machine).await.map(|()| {
                let mut stopped = machine.clone();
                stopped.state = flyco_core::MachineState::Deallocated;
                stopped
            }),
            Self::Start => provider.start(machine).await,
        }
    }
}
