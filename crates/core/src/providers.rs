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

use crate::id::{HostId, ProviderAccountId};
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
    /// An Azure service principal, `Contributor` on a whole subscription.
    ///
    /// Subscription scope rather than resource-group scope, because that is
    /// what `az ad sp create-for-rbac --role Contributor --scopes
    /// /subscriptions/…` produces and what lets flyco create the resource
    /// group it owns instead of asking the user to make one and name it.
    Azure {
        /// Directory (tenant) the service principal belongs to.
        tenant_id: String,
        /// Application (client) id of the service principal.
        client_id: String,
        /// Client secret issued for that application.
        client_secret: String,
        /// Subscription machines are provisioned into.
        subscription_id: String,
        /// The `OpenSSH` public key a machine's break-glass login is created
        /// with.
        ///
        /// Azure refuses to create a Linux machine with neither a password
        /// nor a key and flyco sets no passwords, so one is required. It is
        /// the *user's* key: the wizard generates the pair in their browser,
        /// offers them the private half once, and sends only this. Flyco
        /// never holds a private key for a machine it provisions.
        admin_ssh_public_key: String,
    },
    /// An AWS IAM access key.
    Aws {
        /// Access key id.
        access_key_id: String,
        /// Secret access key.
        secret_access_key: String,
        /// Session token, for a temporary credential.
        ///
        /// Absent for the long-lived IAM key most users will paste in.
        /// Present, and required, for anything minted by `sts:AssumeRole` —
        /// a signature made without it is refused however correct it is.
        #[serde(default)]
        session_token: Option<String>,
        /// Name of an EC2 key pair in the user's own account, for a
        /// break-glass login.
        ///
        /// Optional, unlike Azure's public key: EC2 creates an instance
        /// perfectly well without one. It is the *user's* key pair either
        /// way — flyco never holds a private key for a machine it
        /// provisions, and a key pair lives in their account, not in
        /// flyco's.
        #[serde(default)]
        key_name: Option<String>,
    },
    /// A GCP service account key, as the JSON document Google issues.
    Gcp {
        /// The whole service-account key document.
        service_account_json: String,
    },
    /// A Linux machine the user owns, enrolled with the control plane.
    ///
    /// The one variant that holds no secret, because there is none to hold:
    /// a host authenticates *itself* with the token it was issued at
    /// enrollment, and the control plane never dials it. What this names is
    /// which machine the account provisions onto — see [`crate::host`].
    ///
    /// An account of this kind is created by enrolling a machine, never by
    /// `POST /v1/providers`: a host id nobody enrolled would name a machine
    /// that cannot answer.
    Host {
        /// The enrolled machine this account provisions onto.
        host: HostId,
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
            Self::Host { .. } => CloudProviderKind::Host,
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
    ///
    /// For a machine the user owns this is the host's own label, kept in
    /// step by `PATCH /v1/hosts/{id}`: the two rows name the same thing, so
    /// a rename that moved only one of them would leave the compute chip
    /// calling a machine something its card no longer does.
    pub label: String,
    /// When it was linked, seconds since the Unix epoch.
    pub linked_at_unix: u64,
    /// The enrolled machine this account *is*, when it is one.
    ///
    /// `None` for every cloud account. A host is a provider account
    /// (docs/host-enrollment.md), which is what keeps the catalog, session
    /// creation and the usage panel free of a special case for it — but a
    /// client holding both `GET /v1/hosts` and `GET /v1/providers` still has
    /// to know which account is which machine, and an id is the only honest
    /// way to say so. Inferring it from [`CloudProviderKind::Host`] would
    /// name the kind and not the machine.
    pub host_id: Option<HostId>,
}

/// Answer of `GET /v1/providers/aws/iam-policy`.
///
/// The wizard shows the user the policy they are about to attach to an IAM
/// user, and a policy that is not the truth is worse than none: too narrow
/// and the first provision fails with an `UnauthorizedOperation` nobody can
/// act on, too wide and flyco asked for rights it never uses. So the
/// document is rendered from the driver's own call sites rather than written
/// out by hand somewhere — see `flyco_provider::aws::iam`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AwsIamPolicy {
    /// The policy document, exactly as it is to be pasted into IAM.
    pub document: String,
    /// Every action the document grants, `service:Action`, sorted.
    ///
    /// Beside the document rather than only inside it, so a caller can list
    /// or count the permissions without parsing JSON back out of a string.
    pub actions: Vec<String>,
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
    use crate::id::HostId;
    use crate::machine::CloudProviderKind;

    #[test]
    fn credentials_name_their_own_provider() {
        for (credentials, kind) in [
            (
                ProviderCredentials::Aws {
                    access_key_id: "AKIA".to_owned(),
                    secret_access_key: "secret".to_owned(),
                    session_token: None,
                    key_name: None,
                },
                CloudProviderKind::Aws,
            ),
            (
                ProviderCredentials::Host {
                    host: HostId::from_uuid(uuid::Uuid::from_u128(9)),
                },
                CloudProviderKind::Host,
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
