//! The tree memory flyco serves to agents over MCP, replacing the
//! harnesses' file-based memory systems.

use serde::{Deserialize, Serialize};

use crate::id::MemoryNodeId;

/// One node in the memory tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MemoryNode {
    /// This node's identifier.
    pub id: MemoryNodeId,
    /// Parent node; `None` for a root.
    pub parent: Option<MemoryNodeId>,
    /// Short title shown when listing children.
    pub title: String,
    /// The remembered content.
    pub content: String,
    /// Last update as a unix timestamp in seconds.
    pub updated_at_unix: u64,
}
