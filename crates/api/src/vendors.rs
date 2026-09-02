//! The vendor OAuth clients the control plane holds.
//!
//! Both harnesses now sign in through their vendor's own OAuth flow, and
//! both grants expire, so every path that unseals a credential may have to
//! renew either one. Carrying the two clients as a single value is what
//! keeps that true: a signature that named only Anthropic's would compile
//! perfectly and silently stop refreshing `ChatGPT` grants.

use crate::anthropic::ClaudeClient;
use crate::openai::CodexClient;

/// The two vendors flyco redeems and renews grants at.
#[derive(Debug, Clone, Default)]
pub struct Vendors {
    /// `console.anthropic.com`, for Claude Code.
    pub claude: ClaudeClient,
    /// `auth.openai.com`, for Codex.
    pub codex: CodexClient,
}

impl Vendors {
    /// The pair a deployment talks to.
    #[must_use]
    pub const fn new(claude: ClaudeClient, codex: CodexClient) -> Self {
        Self { claude, codex }
    }
}
