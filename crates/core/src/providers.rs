//! Linked cloud-provider accounts, and the bonus questionnaire that helps a
//! new user find free credit before they spend their own money.
//!
//! Credentials are write-only: they arrive in [`LinkProvider`], are sealed at
//! rest by the control plane, and never appear in a response.
//! [`ProviderAccountView`] therefore has no credential field at all — not an
//! empty one, not a redacted one — so there is no shape in which a leak
//! could be rendered.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::id::ProviderAccountId;
use crate::machine::CloudProviderKind;
use crate::money::Usd;

/// Provider-native credentials, tagged by the provider they open.
///
/// The tag *is* the provider: an account's kind is read off the variant
/// rather than carried beside it, so a request cannot name Azure and enclose
/// an AWS key pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderCredentials {
    /// An Azure service principal with rights over one resource group.
    Azure {
        /// Directory (tenant) the service principal belongs to.
        tenant_id: String,
        /// Application (client) id of the service principal.
        client_id: String,
        /// Client secret issued for that application.
        client_secret: String,
        /// Subscription machines are provisioned into.
        subscription_id: String,
        /// The resource group flyco creates everything inside, which must
        /// already exist.
        ///
        /// Creating a resource group is a subscription-scope write and no
        /// resource-group-scoped role can create the group it is scoped to,
        /// so the group is made out of band and named here. Scope the
        /// principal `Contributor` on it — `Virtual Machine Contributor`
        /// alone cannot create a virtual network, a public IP or a security
        /// group.
        resource_group: String,
        /// The `OpenSSH` public key a machine's break-glass login is created
        /// with.
        ///
        /// Azure refuses to create a Linux machine with neither a password
        /// nor a key and flyco sets no passwords, so one is required. It is
        /// the *user's* key: flyco never holds a private key for a machine
        /// it provisions.
        admin_ssh_public_key: String,
    },
    /// An AWS IAM access key.
    Aws {
        /// Access key id.
        access_key_id: String,
        /// Secret access key.
        secret_access_key: String,
    },
    /// A GCP service account key, as the JSON document Google issues.
    Gcp {
        /// The whole service-account key document.
        service_account_json: String,
    },
    /// A Linux host the user already owns, reached over SSH and sandboxed
    /// with Podman.
    ByoSsh {
        /// Hostname or address to dial.
        host: String,
        /// SSH port.
        port: u16,
        /// Login user, which must be able to run Podman.
        user: String,
        /// PEM-encoded private key flyco authenticates with.
        private_key: String,
        /// The host key flyco must see, as `ssh-keygen -lf` prints it:
        /// `SHA256:` followed by unpadded base64.
        ///
        /// Required rather than optional, and there is no trust-on-first-use
        /// path: linking this account is the moment flyco starts handing the
        /// host live session credentials, and an unverified host key means
        /// handing them to whoever answers on that address.
        host_fingerprint: String,
    },
}

impl ProviderCredentials {
    /// Which provider these credentials open.
    #[must_use]
    pub const fn kind(&self) -> CloudProviderKind {
        match self {
            Self::Azure { .. } => CloudProviderKind::Azure,
            Self::Aws { .. } => CloudProviderKind::Aws,
            Self::Gcp { .. } => CloudProviderKind::Gcp,
            Self::ByoSsh { .. } => CloudProviderKind::ByoSsh,
        }
    }
}

/// Request body of `POST /v1/providers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LinkProvider {
    /// Human-readable name, so an account can be recognised in a list before
    /// it is unlinked.
    pub label: String,
    /// The credentials to seal. Their variant names the provider.
    pub credentials: ProviderCredentials,
}

/// One row of `GET /v1/providers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProviderAccountView {
    /// Identifier used to unlink the account.
    pub id: ProviderAccountId,
    /// Which provider it is.
    pub kind: CloudProviderKind,
    /// Label supplied when it was linked.
    pub label: String,
    /// When it was linked, seconds since the Unix epoch.
    pub linked_at_unix: u64,
}

/// Request body of `POST /v1/providers/quickstart`.
///
/// Two questions, because two questions are what separate the free-credit
/// programmes worth telling somebody about from the ones that would waste
/// their time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct QuickstartAnswers {
    /// Whether the user has never held an account with these providers, and
    /// so still qualifies for new-customer credit.
    pub new_to_provider: bool,
    /// Whether the user is a student, which unlocks the education tiers.
    pub is_student: bool,
}

/// One suggestion returned by the quickstart questionnaire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProviderBonusHint {
    /// Provider offering the credit.
    pub provider: CloudProviderKind,
    /// Name of the programme, as the provider calls it.
    pub title: String,
    /// What the user has to do to claim it.
    pub detail: String,
    /// Headline credit, when the programme states a fixed amount.
    pub credit: Option<Usd>,
    /// Where to sign up.
    pub url: String,
}

#[cfg(test)]
mod tests {
    use super::{LinkProvider, ProviderCredentials};
    use crate::machine::CloudProviderKind;

    #[test]
    fn credentials_name_their_own_provider() {
        for (credentials, kind) in [
            (
                ProviderCredentials::Aws {
                    access_key_id: "AKIA".to_owned(),
                    secret_access_key: "secret".to_owned(),
                },
                CloudProviderKind::Aws,
            ),
            (
                ProviderCredentials::ByoSsh {
                    host: "build.lexo.cool".to_owned(),
                    port: 22,
                    user: "flyco".to_owned(),
                    private_key: "-----BEGIN OPENSSH PRIVATE KEY-----".to_owned(),
                    host_fingerprint: "SHA256:qWyVLPxNBRr7Nnkm1xTQKMDcXwHFsSFRnLW6iNfPmcQ"
                        .to_owned(),
                },
                CloudProviderKind::ByoSsh,
            ),
        ] {
            assert_eq!(credentials.kind(), kind);
        }
    }

    #[test]
    fn a_link_request_round_trips_with_its_tag() {
        let request = LinkProvider {
            label: "personal azure".to_owned(),
            credentials: ProviderCredentials::Azure {
                tenant_id: "tenant".to_owned(),
                client_id: "client".to_owned(),
                client_secret: "secret".to_owned(),
                subscription_id: "subscription".to_owned(),
                resource_group: "flyco-rg".to_owned(),
                admin_ssh_public_key: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA lexo@flyco".to_owned(),
            },
        };

        let json = serde_json::to_value(&request).expect("serialize");
        assert_eq!(json["credentials"]["kind"], "azure");

        let back: LinkProvider = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, request);
        assert_eq!(back.credentials.kind(), CloudProviderKind::Azure);
    }
}
