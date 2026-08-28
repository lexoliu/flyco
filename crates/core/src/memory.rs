//! The tree memory flyco serves to agents over MCP, replacing the
//! harnesses' file-based memory systems.
//!
//! A node is scoped to a repository or shared across all of them, and every
//! node but a root has a parent, so recall is a walk down the tree rather
//! than a grep over a directory of Markdown.

use serde::{Deserialize, Serialize};

use crate::id::MemoryNodeId;
use crate::repo::RepoSlug;

/// One node in the memory tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MemoryNode {
    /// This node's identifier.
    pub id: MemoryNodeId,
    /// Parent node; `None` for a root.
    pub parent: Option<MemoryNodeId>,
    /// Repository this subtree is about; `None` for memory that applies
    /// wherever the user's agents run.
    pub repo: Option<RepoSlug>,
    /// Short title shown when listing children.
    pub title: String,
    /// The remembered content.
    pub content: String,
    /// Last update as a unix timestamp in seconds.
    pub updated_at_unix: u64,
}

/// Request body of `POST /v1/memory`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CreateMemoryNode {
    /// Parent to hang the node under; omitted creates a root.
    pub parent: Option<MemoryNodeId>,
    /// Repository the node is about; omitted makes it shared.
    pub repo: Option<RepoSlug>,
    /// Short title shown when listing children.
    pub title: String,
    /// The content to remember.
    pub content: String,
}

/// Request body of `PATCH /v1/memory/{id}`.
///
/// An omitted field is left as it is. Reparenting is deliberately not
/// expressible: moving a subtree changes what every descendant is scoped to,
/// which is a different operation from editing a note.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct UpdateMemoryNode {
    /// New title.
    pub title: Option<String>,
    /// New content.
    pub content: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{MemoryNode, UpdateMemoryNode};
    use crate::id::MemoryNodeId;

    #[test]
    fn a_shared_root_node_round_trips() {
        let node = MemoryNode {
            id: MemoryNodeId::generate(),
            parent: None,
            repo: None,
            title: "How Lexo likes commits".to_owned(),
            content: "Conventional commits, no co-author trailers.".to_owned(),
            updated_at_unix: 1_800_000_000,
        };

        let json = serde_json::to_value(&node).expect("serialize");
        assert!(json["repo"].is_null());
        assert_eq!(
            serde_json::from_value::<MemoryNode>(json).expect("deserialize"),
            node
        );
    }

    #[test]
    fn an_empty_patch_changes_nothing() {
        let update: UpdateMemoryNode = serde_json::from_str("{}").expect("deserialize");
        assert_eq!(update, UpdateMemoryNode::default());
    }
}
