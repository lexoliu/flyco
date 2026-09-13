//! flyco's terminal client library.
//!
//! Two surfaces share this crate:
//!
//! * **The human path** (`flyco claude`, `flyco codex`, `flyco resume`)
//!   provisions a session and bridges the local terminal to the harness's
//!   native TUI running inside the session's machine PTY.
//! * **The agent path** (`flyco session …`, `flyco run`, the discovery
//!   commands) is a faithful projection of the control-plane REST API —
//!   JSON-first, non-interactive, and versioned by `/v1` rather than by
//!   anything this binary invents.
//!
//! Both ride REST for commands and one SSE stream (`GET /v1/events`) for
//! push. There is no WebSocket anywhere in the system.

pub mod bridge;
pub mod cli;
pub mod client;
pub mod creds;
pub mod follow;
pub mod out;
pub mod pick;

pub mod auth;
pub mod discover;
pub mod human;
pub mod run;
pub mod session;

/// The process exit code, the whole of the contract's second half.
///
/// Codes 7–10 are the ones a `wait`/`run` reports its ending with: they
/// are outcomes, not failures, and scripts switch on them. [`Exit::Remote`]
/// carries a remote command's own status through — `exec` is `ssh`-shaped,
/// so its answer is the code the command chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The command did what was asked, or a `wait`/`run` matched a
    /// condition that is not an ending.
    Ok,
    /// The command line itself was wrong. `clap` reports usage itself;
    /// this is for inputs it accepted that the semantics rejected.
    Usage,
    /// No credential, a refused credential, or a denied approval of one.
    Auth,
    /// The named thing is not there, or the request conflicts with what
    /// is there (404/409).
    NotFoundOrConflict,
    /// The control plane answered with a problem document.
    Problem,
    /// The request never produced an answer: transport, timeout, a stream
    /// that dropped.
    Transport,
    /// A `run`/`wait` ended because an approval is waiting.
    ApprovalPending,
    /// A `run`/`wait` ended because the session paused.
    Paused,
    /// A `run`/`wait` ended because the session failed or stopped.
    Failed,
    /// The caller's `--timeout` elapsed.
    Timeout,
    /// A remote command's own exit status, passed through verbatim —
    /// `session exec` is `ssh`-shaped, and its answer is the code the
    /// command chose, even one that collides with this list.
    Remote(u8),
}

impl Exit {
    /// The process exit code.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Usage => 2,
            Self::Auth => 3,
            Self::NotFoundOrConflict => 4,
            Self::Problem => 5,
            Self::Transport => 6,
            Self::ApprovalPending => 7,
            Self::Paused => 8,
            Self::Failed => 9,
            Self::Timeout => 10,
            Self::Remote(code) => code,
        }
    }
}

/// A command's refusal: text for stderr and the code to exit with.
///
/// The text is either a sentence this CLI composed, or the control
/// plane's `application/problem+json` document verbatim — agents read the
/// document, humans read whichever it is.
#[derive(Debug, thiserror::Error)]
#[error("{text}")]
pub struct Failure {
    /// The code the process exits with.
    pub code: Exit,
    /// What goes to stderr.
    pub text: String,
}

impl Failure {
    /// A usage error: the flag that is missing or malformed, named.
    pub fn usage(text: impl Into<String>) -> Self {
        Self {
            code: Exit::Usage,
            text: text.into(),
        }
    }

    /// A refusal by the control plane, carrying its problem document.
    pub fn problem(code: Exit, text: impl Into<String>) -> Self {
        Self {
            code,
            text: text.into(),
        }
    }

    /// A transport-level failure: no answer arrived at all.
    pub fn transport(text: impl Into<String>) -> Self {
        Self {
            code: Exit::Transport,
            text: text.into(),
        }
    }
}

/// The control plane's own root, when the environment does not override it.
pub const DEFAULT_API_URL: &str = "https://flyco.dev";

/// Environment variable that points the CLI at another control plane.
pub const API_URL_ENV: &str = "FLYCO_API_URL";

/// Environment variable carrying an `fk_` API key, overriding the
/// credentials file. The headless half of the auth contract.
pub const TOKEN_ENV: &str = "FLYCO_TOKEN";

/// A command's result: a value to render, or a failure to report.
pub type Outcome<T> = Result<T, Failure>;
