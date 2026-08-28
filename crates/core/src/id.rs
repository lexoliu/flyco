//! Phantom-typed identifiers.
//!
//! Every entity gets its own id type so a `SessionId` can never be passed
//! where a `MachineId` is expected — the mixup is a compile error, not a
//! runtime lookup miss.

use core::fmt;
use core::marker::PhantomData;
use core::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A UUID-backed identifier tagged with the entity type it identifies.
///
/// Serialized as the plain hyphenated UUID string.
#[derive(Serialize, Deserialize)]
#[serde(transparent)]
pub struct Id<T> {
    value: Uuid,
    #[serde(skip)]
    _marker: PhantomData<fn() -> T>,
}

impl<T> Id<T> {
    /// Generates a fresh random identifier.
    #[must_use]
    pub fn generate() -> Self {
        Self::from_uuid(Uuid::new_v4())
    }

    /// Wraps an existing UUID.
    #[must_use]
    pub const fn from_uuid(value: Uuid) -> Self {
        Self {
            value,
            _marker: PhantomData,
        }
    }

    /// The underlying UUID.
    #[must_use]
    pub const fn as_uuid(&self) -> &Uuid {
        &self.value
    }
}

impl<T> fmt::Display for Id<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.value.fmt(f)
    }
}

impl<T> fmt::Debug for Id<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", core::any::type_name::<T>(), self.value)
    }
}

impl<T> Clone for Id<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Id<T> {}

impl<T> PartialEq for Id<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<T> Eq for Id<T> {}

impl<T> core::hash::Hash for Id<T> {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}

impl<T> PartialOrd for Id<T> {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for Id<T> {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.value.cmp(&other.value)
    }
}

impl<T> utoipa::PartialSchema for Id<T> {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::ObjectBuilder::new()
            .schema_type(utoipa::openapi::schema::Type::String)
            .format(Some(utoipa::openapi::SchemaFormat::Custom(
                "uuid".to_owned(),
            )))
            .into()
    }
}

impl<T> utoipa::ToSchema for Id<T> {
    fn name() -> alloc::borrow::Cow<'static, str> {
        // Every id shares one string/uuid component; the phantom tag is a
        // compile-time distinction only.
        alloc::borrow::Cow::Borrowed("Uuid")
    }
}

extern crate alloc;

impl<T> FromStr for Id<T> {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::from_str(s).map(Self::from_uuid)
    }
}

/// Marker for user identifiers.
#[derive(Debug)]
pub struct UserEntity(());
/// Marker for session identifiers.
#[derive(Debug)]
pub struct SessionEntity(());
/// Marker for machine identifiers.
#[derive(Debug)]
pub struct MachineEntity(());
/// Marker for budget identifiers.
#[derive(Debug)]
pub struct BudgetEntity(());
/// Marker for approval identifiers.
#[derive(Debug)]
pub struct ApprovalEntity(());
/// Marker for skill identifiers.
#[derive(Debug)]
pub struct SkillEntity(());
/// Marker for memory-tree node identifiers.
#[derive(Debug)]
pub struct MemoryNodeEntity(());
/// Marker for REST API key identifiers.
#[derive(Debug)]
pub struct ApiKeyEntity(());
/// Marker for spend-ledger entry identifiers.
#[derive(Debug)]
pub struct SpendEventEntity(());
/// Marker for linked cloud-provider account identifiers.
#[derive(Debug)]
pub struct ProviderAccountEntity(());
/// Marker for linked harness account identifiers.
#[derive(Debug)]
pub struct HarnessAccountEntity(());
/// Marker for registered MCP server identifiers.
#[derive(Debug)]
pub struct McpServerEntity(());
/// Marker for web push subscription identifiers.
#[derive(Debug)]
pub struct PushSubscriptionEntity(());

/// Identifies a flyco user.
pub type UserId = Id<UserEntity>;
/// Identifies a session.
pub type SessionId = Id<SessionEntity>;
/// Identifies a provisioned machine.
pub type MachineId = Id<MachineEntity>;
/// Identifies a session budget.
pub type BudgetId = Id<BudgetEntity>;
/// Identifies a pending or resolved approval.
pub type ApprovalId = Id<ApprovalEntity>;
/// Identifies an uploaded skill.
pub type SkillId = Id<SkillEntity>;
/// Identifies a node in the tree memory.
pub type MemoryNodeId = Id<MemoryNodeEntity>;
/// Identifies a REST API key.
pub type ApiKeyId = Id<ApiKeyEntity>;
/// Identifies one entry in a budget's append-only spend ledger.
pub type SpendEventId = Id<SpendEventEntity>;
/// Identifies a linked cloud-provider account.
pub type ProviderAccountId = Id<ProviderAccountEntity>;
/// Identifies a linked Claude or Codex account.
pub type HarnessAccountId = Id<HarnessAccountEntity>;
/// Identifies a registered MCP server.
pub type McpServerId = Id<McpServerEntity>;
/// Identifies one browser's web push subscription.
pub type PushSubscriptionId = Id<PushSubscriptionEntity>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_through_serde() {
        let id = SessionId::generate();
        let json = serde_json::to_string(&id).expect("serialize");
        let back: SessionId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
    }

    #[test]
    fn ids_parse_from_display() {
        let id = MachineId::generate();
        let parsed: MachineId = id.to_string().parse().expect("parse");
        assert_eq!(id, parsed);
    }
}
