//! The vendor OAuth clients the control plane holds.
//!
//! Both harnesses sign in through their vendor's own OAuth flow, and both
//! grants expire, so every path that unseals a credential may have to renew
//! either one. Carrying the clients as a single value is what keeps that
//! true: a signature that named only Anthropic's would compile perfectly and
//! silently stop refreshing `ChatGPT` grants.
//!
//! The two cloud vendors are here for the same reason rather than for that
//! one: "Sign in with Microsoft" and "Sign in with Google" each need a
//! client the router can hand to a handler and a test can stand in for, and
//! one bag of vendor clients is one thing to wire rather than four.

use crate::anthropic::ClaudeClient;
use crate::google::GoogleClient;
use crate::microsoft::MicrosoftClient;
use crate::openai::CodexClient;

/// The four vendors flyco redeems grants at.
#[derive(Debug, Clone, Default)]
pub struct Vendors {
    /// `console.anthropic.com`, for Claude Code.
    pub claude: ClaudeClient,
    /// `auth.openai.com`, for Codex.
    pub codex: CodexClient,
    /// `login.microsoftonline.com`, for an Azure subscription.
    pub microsoft: MicrosoftClient,
    /// `accounts.google.com`, for a GCP project.
    pub google: GoogleClient,
}

impl Vendors {
    /// The four a deployment talks to.
    #[must_use]
    pub const fn new(
        claude: ClaudeClient,
        codex: CodexClient,
        microsoft: MicrosoftClient,
        google: GoogleClient,
    ) -> Self {
        Self {
            claude,
            codex,
            microsoft,
            google,
        }
    }
}
