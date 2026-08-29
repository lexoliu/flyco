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
use flyco_provider::azure::auth::ServicePrincipal;
use flyco_provider::azure::{AzureProvider, Workspace};
use flyco_provider::byo_ssh::ByoSsh;
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
/// For Azure that is the intersection of three independent gates — the
/// SKU's own restrictions, the subscription's quota, and the region policy
/// assigned to the subscription — so an entry that comes back is one a
/// provision request will be allowed to create. A registered SSH host
/// reports the single machine it is, unpriced: the user already owns it.
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
            let mut provider = AzureProvider::new(
                ServicePrincipal {
                    tenant_id: tenant_id.clone(),
                    client_id: client_id.clone(),
                    client_secret: client_secret.clone(),
                    subscription_id: subscription_id.clone(),
                },
                Workspace::new(resource_group.clone(), admin_ssh_public_key.clone()),
            );
            provider.catalog().await
        }
        ProviderCredentials::ByoSsh { host, .. } => Ok(ByoSsh::new(host.clone()).catalog()),
        ProviderCredentials::Aws { .. } => Err(ProviderError::Unsupported {
            provider: "AWS",
            operation: "catalog",
            reason: "flyco has no AWS driver yet",
        }),
        ProviderCredentials::Gcp { .. } => Err(ProviderError::Unsupported {
            provider: "GCP",
            operation: "catalog",
            reason: "flyco has no GCP driver yet",
        }),
    }
}

/// Reads what the provider's own meter says this account has been billed
/// over its current billing period.
///
/// `None` is not a failure and not a zero: it means this provider meters
/// nothing on flyco's behalf. A registered SSH host is hardware the user
/// already owns and already pays for, so it contributes no row at all —
/// reporting `$0.00` against it would be flyco asserting the machine is
/// free.
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
        } => {
            let mut provider = AzureProvider::new(
                ServicePrincipal {
                    tenant_id: tenant_id.clone(),
                    client_id: client_id.clone(),
                    client_secret: client_secret.clone(),
                    subscription_id: subscription_id.clone(),
                },
                Workspace::new(resource_group.clone(), admin_ssh_public_key.clone()),
            );
            provider.billing_period_cost(now_unix).await.map(Some)
        }
        ProviderCredentials::ByoSsh { .. } => Ok(None),
        ProviderCredentials::Aws { .. } => Err(ProviderError::Unsupported {
            provider: "AWS",
            operation: "usage",
            reason: "flyco has no AWS driver yet",
        }),
        ProviderCredentials::Gcp { .. } => Err(ProviderError::Unsupported {
            provider: "GCP",
            operation: "usage",
            reason: "flyco has no GCP driver yet",
        }),
    }
}

/// One lifecycle operation against an already-provisioned machine.
///
/// Each arm dispatches on the credential variant for the same reason
/// [`catalog`] does: the driver trait is not object-safe, deliberately.
/// Only Azure implements these today — a registered SSH host cannot be
/// resized, and its container lifecycle is executed natively rather than
/// from the Worker.
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
            let mut provider = AzureProvider::new(
                ServicePrincipal {
                    tenant_id: tenant_id.clone(),
                    client_id: client_id.clone(),
                    client_secret: client_secret.clone(),
                    subscription_id: subscription_id.clone(),
                },
                Workspace::new(resource_group.clone(), admin_ssh_public_key.clone()),
            );
            match operation {
                Operation::Resize { machine_type } => provider.resize(machine, machine_type).await,
                Operation::Stop => provider.deallocate(machine).await.map(|()| {
                    let mut stopped = machine.clone();
                    stopped.state = flyco_core::MachineState::Deallocated;
                    stopped
                }),
                Operation::Start => provider.start(machine).await,
            }
        }
        ProviderCredentials::ByoSsh { .. } => Err(ProviderError::Unsupported {
            provider: "byo-ssh",
            operation: operation.name(),
            reason: "a host you own is started and stopped by you, not by flyco",
        }),
        ProviderCredentials::Aws { .. } => Err(ProviderError::Unsupported {
            provider: "AWS",
            operation: operation.name(),
            reason: "flyco has no AWS driver yet",
        }),
        ProviderCredentials::Gcp { .. } => Err(ProviderError::Unsupported {
            provider: "GCP",
            operation: operation.name(),
            reason: "flyco has no GCP driver yet",
        }),
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
}
