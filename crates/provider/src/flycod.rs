//! The `flycod` configuration a provisioned machine boots with.
//!
//! Every provider hands the daemon the same document — Azure writes it into
//! cloud-init, byo-ssh into the container's environment — so it is rendered
//! once, here, and both drivers embed the result.
//!
//! It is a serde structure serialized by [`toml`] rather than a template:
//! the daemon's own [`DaemonConfig`] is `deny_unknown_fields` and rejects the
//! whole file over one stray key, so the escaping and the layout have to be
//! the serializer's problem, not a template author's. What the serializer
//! cannot check is that the *shape* still matches what the daemon expects,
//! which is why `crates/daemon/tests/provisioned_config.rs` renders this and
//! parses it back with the daemon's own loader: a field renamed on either
//! side fails that test rather than a machine that boots and never phones
//! home.
//!
//! [`DaemonConfig`]: https://github.com/lexoliu/flyco/blob/main/crates/daemon/src/config.rs

use core::fmt;

use flyco_core::machine::SessionMachine;
use flyco_core::{BranchName, HarnessKind, MachineOrigin, PermissionMode, RepoSlug, SessionId};
use serde::Serialize;

use crate::{DaemonBootstrap, GitIdentity};

/// Where the agent's checkout lives inside a flyco machine.
pub const WORKDIR: &str = "/srv/flyco/work";

/// Where a daemon with no control plane would keep its transcript. Present
/// because the field is required, unused because a provisioned machine
/// always has a control plane and keeps its transcript in R2.
pub const TRANSCRIPT_DIR: &str = "/var/lib/flyco/transcripts";

/// Where the Bun sidecar is materialized.
pub const SIDECAR_DIR: &str = "/var/lib/flyco/sidecar";

/// The isolated `CLAUDE_CONFIG_DIR` an injected credential runs under.
pub const CLAUDE_CONFIG_DIR: &str = "/var/lib/flyco/claude";

/// `CLAUDE_CODE_PROJECT_DIR_NAME`, the Agent SDK's project key.
///
/// It is what a session's transcript is filed under, and therefore what
/// cross-host resume joins on. Varying it with the machine would lose the
/// history on every move, which is why it is a constant.
pub const CLAUDE_PROJECT_DIR_NAME: &str = "flyco-session";

/// The isolated `CODEX_HOME` an injected Codex credential runs under.
pub const CODEX_HOME: &str = "/var/lib/flyco/codex";

/// How the supervised `claude` CLI authenticates on a provisioned machine.
///
/// [`Inherit`](Self::Inherit) is the developer-machine mode and is what a
/// machine gets when the user has linked no Claude account yet: the session
/// comes up, and the harness says it is unauthenticated, which is a better
/// failure than a machine that never provisions.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ClaudeCredential {
    /// No credential injected.
    Inherit,
    /// A Claude subscription OAuth token.
    OauthToken {
        /// Value for `CLAUDE_CODE_OAUTH_TOKEN`.
        token: String,
    },
    /// An Anthropic API key.
    ApiKey {
        /// Value for `ANTHROPIC_API_KEY`.
        key: String,
    },
}

impl fmt::Debug for ClaudeCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode = match self {
            Self::Inherit => "inherit",
            Self::OauthToken { .. } => "oauth_token",
            Self::ApiKey { .. } => "api_key",
        };
        f.debug_struct("ClaudeCredential")
            .field("mode", &mode)
            .finish_non_exhaustive()
    }
}

impl ClaudeCredential {
    /// The isolated config tree this mode runs under, if any.
    ///
    /// Credentials and isolation are one decision in the daemon's model:
    /// injecting a token into a shared `~/.claude` would trample a real
    /// login, so every credential-bearing mode carries its own tree.
    const fn isolation(&self) -> Option<Isolation> {
        match self {
            Self::Inherit => None,
            Self::OauthToken { .. } | Self::ApiKey { .. } => Some(Isolation {
                config_dir: CLAUDE_CONFIG_DIR,
                project_dir_name: CLAUDE_PROJECT_DIR_NAME,
            }),
        }
    }
}

/// An isolated Claude configuration tree, as the daemon's config spells it.
#[derive(Debug, Clone, Copy, Serialize)]
struct Isolation {
    config_dir: &'static str,
    project_dir_name: &'static str,
}

/// The `[claude.auth]` table.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum Auth<'a> {
    Inherit,
    OauthToken {
        token: &'a str,
        isolation: Isolation,
    },
    ApiKey {
        key: &'a str,
        isolation: Isolation,
    },
}

/// The `[claude]` table.
#[derive(Debug, Clone, Serialize)]
struct Claude<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    permission_mode: PermissionMode,
    auth: Auth<'a>,
}

/// The `[control_plane]` table.
#[derive(Debug, Clone, Serialize)]
struct ControlPlane<'a> {
    url: &'a str,
    daemon_token: &'a str,
}

/// The `[repo]` table, and `[repo.identity]` under it.
///
/// The token is a field of the same table as the slug because the two are
/// one decision: a checkout flyco cannot authenticate is not a checkout, and
/// a token with no repository to spend it on has no reason to be on the
/// machine at all.
#[derive(Debug, Clone, Serialize)]
struct Repo<'a> {
    slug: &'a RepoSlug,
    branch: &'a BranchName,
    token: &'a str,
    identity: &'a GitIdentity,
}

/// The `[sidecar]` table.
#[derive(Debug, Clone, Copy, Serialize)]
struct Sidecar {
    dir: &'static str,
    bun: &'static str,
}

/// An isolated Codex home, as the daemon's config spells it.
#[derive(Debug, Clone, Copy, Serialize)]
struct CodexIsolation {
    home: &'static str,
}

/// The `[codex.auth]` table.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum CodexAuth<'a> {
    Inherit,
    OauthToken {
        token: &'a str,
        isolation: CodexIsolation,
    },
    ApiKey {
        key: &'a str,
        isolation: CodexIsolation,
    },
}

/// The `[codex]` table.
#[derive(Debug, Clone, Serialize)]
struct Codex<'a> {
    bin: &'static str,
    approval_policy: &'static str,
    sandbox: &'static str,
    auth: CodexAuth<'a>,
}

/// The whole document.
///
/// Field order is the serialization order and TOML puts every scalar before
/// the first table, so the scalars come first here. Getting that wrong makes
/// `toml` refuse to serialize rather than emit an invalid document.
#[derive(Debug, Clone, Serialize)]
struct Document<'a> {
    session: SessionId,
    harness: HarnessKind,
    workdir: &'static str,
    transcript_dir: &'static str,
    machine_origin: MachineOrigin,
    #[serde(skip_serializing_if = "Option::is_none")]
    resume_session_id: Option<&'a str>,
    control_plane: ControlPlane<'a>,
    repo: Repo<'a>,
    machine: &'a SessionMachine,
    #[serde(skip_serializing_if = "Option::is_none")]
    claude: Option<Claude<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sidecar: Option<Sidecar>,
    #[serde(skip_serializing_if = "Option::is_none")]
    codex: Option<Codex<'a>>,
}

/// Why a configuration could not be rendered.
#[derive(Debug, thiserror::Error)]
#[error("the flycod configuration could not be rendered as TOML: {0}")]
pub struct RenderError(#[from] toml::ser::Error);

/// Renders the configuration a machine's `flycod` boots with.
///
/// # Errors
///
/// Returns [`RenderError`] if the document does not serialize, which would
/// mean this module's own structure is malformed rather than anything the
/// caller did.
pub fn render(bootstrap: &DaemonBootstrap) -> Result<String, RenderError> {
    let isolation = bootstrap.claude_auth.isolation();
    let claude_auth = match (&bootstrap.claude_auth, isolation) {
        (ClaudeCredential::OauthToken { token }, Some(isolation)) => Auth::OauthToken {
            token: token.as_str(),
            isolation,
        },
        (ClaudeCredential::ApiKey { key }, Some(isolation)) => Auth::ApiKey {
            key: key.as_str(),
            isolation,
        },
        _ => Auth::Inherit,
    };
    let codex_isolation = CodexIsolation { home: CODEX_HOME };
    let codex_auth = match &bootstrap.claude_auth {
        ClaudeCredential::OauthToken { token } => CodexAuth::OauthToken {
            token: token.as_str(),
            isolation: codex_isolation,
        },
        ClaudeCredential::ApiKey { key } => CodexAuth::ApiKey {
            key: key.as_str(),
            isolation: codex_isolation,
        },
        ClaudeCredential::Inherit => CodexAuth::Inherit,
    };

    let (claude, sidecar, codex) = match bootstrap.harness {
        HarnessKind::ClaudeCode => (
            Some(Claude {
                model: None,
                permission_mode: bootstrap.permission_mode,
                auth: claude_auth,
            }),
            Some(Sidecar {
                dir: SIDECAR_DIR,
                bun: "bun",
            }),
            None,
        ),
        HarnessKind::Codex => (
            None,
            None,
            Some(Codex {
                bin: "codex",
                approval_policy: "on-request",
                sandbox: "workspace-write",
                auth: codex_auth,
            }),
        ),
    };

    let document = Document {
        session: bootstrap.session,
        harness: bootstrap.harness,
        workdir: WORKDIR,
        transcript_dir: TRANSCRIPT_DIR,
        machine_origin: bootstrap.machine_origin,
        resume_session_id: bootstrap.resume_session_id.as_deref(),
        control_plane: ControlPlane {
            url: &bootstrap.control_plane_url,
            daemon_token: &bootstrap.daemon_token,
        },
        repo: Repo {
            slug: &bootstrap.repo.slug,
            branch: &bootstrap.repo.branch,
            token: &bootstrap.repo.token,
            identity: &bootstrap.repo.identity,
        },
        machine: &bootstrap.machine,
        claude,
        sidecar,
        codex,
    };

    Ok(toml::to_string_pretty(&document)?)
}

#[cfg(test)]
mod tests {
    use flyco_core::{HarnessKind, MachineOrigin, PermissionMode, SessionId};

    use super::{CLAUDE_CONFIG_DIR, CODEX_HOME, ClaudeCredential, render};
    use crate::DaemonBootstrap;
    use crate::testing::{GITHUB_TOKEN, checkout};

    fn bootstrap(claude_auth: ClaudeCredential) -> DaemonBootstrap {
        DaemonBootstrap {
            session: SessionId::generate(),
            control_plane_url: "https://flyco.dev/".to_owned(),
            daemon_token: "fd_token".to_owned(),
            harness: HarnessKind::ClaudeCode,
            permission_mode: PermissionMode::Default,
            claude_auth,
            repo: checkout(),
            machine_origin: MachineOrigin::Auto,
            machine: crate::testing::session_machine(),
            resume_session_id: None,
        }
    }

    #[test]
    fn a_provisioned_config_names_the_control_plane_and_the_token() {
        let rendered = render(&bootstrap(ClaudeCredential::Inherit)).expect("render");
        assert!(rendered.contains("[control_plane]"));
        assert!(rendered.contains("url = \"https://flyco.dev/\""));
        assert!(rendered.contains("daemon_token = \"fd_token\""));
        assert!(rendered.contains("permission_mode = \"default\""));
        assert!(rendered.contains("mode = \"inherit\""));
    }

    #[test]
    fn an_injected_credential_always_carries_its_own_config_tree() {
        let rendered = render(&bootstrap(ClaudeCredential::OauthToken {
            token: "sk-ant-oat01-x".to_owned(),
        }))
        .expect("render");

        assert!(rendered.contains("mode = \"oauth_token\""));
        assert!(rendered.contains(CLAUDE_CONFIG_DIR));
    }

    #[test]
    fn a_credential_never_shows_up_in_a_debug_rendering() {
        let credential = ClaudeCredential::ApiKey {
            key: "sk-ant-secret".to_owned(),
        };
        assert!(!format!("{credential:?}").contains("sk-ant-secret"));
    }

    #[test]
    fn the_machine_is_told_which_repository_and_branch_to_check_out() {
        let rendered = render(&bootstrap(ClaudeCredential::Inherit)).expect("render");

        assert!(rendered.contains("[repo]"));
        assert!(rendered.contains("slug = \"lexoliu/flyco\""));
        assert!(rendered.contains("branch = \"dev\""));
        assert!(rendered.contains("[repo.identity]"));
        assert!(rendered.contains("email = \"4242+lexoliu@users.noreply.github.com\""));
    }

    #[test]
    fn the_github_token_never_shows_up_in_a_debug_rendering() {
        // The bootstrap is what a driver traces while it is being debugged,
        // and it now carries a live GitHub token as well as two other
        // credentials. None of the three may survive a `{:?}`.
        let bootstrap = bootstrap(ClaudeCredential::OauthToken {
            token: "sk-ant-oat01-x".to_owned(),
        });
        let debugged = format!("{bootstrap:?}");

        assert!(!debugged.contains(GITHUB_TOKEN));
        assert!(!debugged.contains("sk-ant-oat01-x"));
        assert!(!debugged.contains("fd_token"));
        // What is left is still enough to tell two bootstraps apart.
        assert!(debugged.contains("lexoliu/flyco"));
    }

    #[test]
    fn a_codex_session_writes_the_codex_table_and_not_claude() {
        let mut bootstrap = bootstrap(ClaudeCredential::OauthToken {
            token: "chatgpt-access".to_owned(),
        });
        bootstrap.harness = HarnessKind::Codex;
        let rendered = render(&bootstrap).expect("render");

        assert!(rendered.contains("[codex]"));
        assert!(rendered.contains("approval_policy = \"on-request\""));
        assert!(rendered.contains(CODEX_HOME));
        assert!(!rendered.contains("[claude]"));
        assert!(!rendered.contains("[sidecar]"));
    }

    #[test]
    fn the_machine_the_agent_is_told_about_is_written_with_who_chose_it() {
        let mut chosen = bootstrap(ClaudeCredential::Inherit);
        chosen.machine_origin = MachineOrigin::User;
        chosen.machine.minimum = Some(flyco_core::BillingMinimum::new(
            24,
            flyco_core::Usd::from_cents(65),
        ));
        let rendered = render(&chosen).expect("render");

        assert!(rendered.contains("machine_origin = \"user\""));
        assert!(rendered.contains("[machine]"));
        assert!(rendered.contains("machine_type = \"Standard_D4s_v6\""));
        assert!(rendered.contains("[machine.minimum]"));
        assert!(rendered.contains("hours = 24"));
    }

    #[test]
    fn a_machine_with_nothing_to_omit_writes_no_empty_facts() {
        // TOML has no null: an absent capacity or minimum has to be an
        // absent key, and a `None` reaching the serializer is a render
        // failure rather than a document the daemon would reject.
        let mut unknown = bootstrap(ClaudeCredential::Inherit);
        unknown.machine.capacity = None;
        unknown.machine.hourly = None;
        let rendered = render(&unknown).expect("render");

        assert!(!rendered.contains("capacity"));
        assert!(!rendered.contains("hourly"));
    }

    #[test]
    fn a_resume_id_is_written_only_when_there_is_one() {
        let mut with_resume = bootstrap(ClaudeCredential::Inherit);
        with_resume.resume_session_id = Some("1f6d2c50-8a4b-4a2b-9f6d-2c508a4b4a2b".to_owned());

        assert!(
            !render(&bootstrap(ClaudeCredential::Inherit))
                .expect("render")
                .contains("resume_session_id")
        );
        assert!(
            render(&with_resume)
                .expect("render")
                .contains("resume_session_id")
        );
    }
}
