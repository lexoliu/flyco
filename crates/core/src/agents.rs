//! The shared `AGENTS.md` every session runs under.
//!
//! One document, owned by the user, installed as managed policy on every
//! machine — `AGENTS.md` for Codex, the managed `CLAUDE.md` for Claude Code.
//! The agent cannot edit it: the file is root-owned and a hook refuses the
//! write, so an agent that wants a change asks for one through the
//! `agentsmd_change_request` MCP tool and the request arrives as an ordinary
//! approval. These types are the *user's* own edit path, which is the only
//! one that writes.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Response of `GET`/`PUT /v1/agents-md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AgentsDocument {
    /// The whole document, as Markdown.
    pub content: String,
    /// Last change, seconds since the Unix epoch.
    pub updated_at_unix: u64,
}

/// Request body of `PUT /v1/agents-md`.
///
/// Separate from [`AgentsDocument`] because
/// [`updated_at_unix`](AgentsDocument::updated_at_unix) is the control
/// plane's to stamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UpdateAgentsDocument {
    /// The complete new document; this replaces what is stored.
    pub content: String,
}
