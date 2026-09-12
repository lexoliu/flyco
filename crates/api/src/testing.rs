//! Fixtures shared by the crate's unit tests.

use core::future::{Future, ready};

use flyco_core::{
    CpuArchitecture, CurrentUser, HarnessAccountId, HarnessKind, HostFacts, HostId, MachineChoice,
    ProviderAccountId, ProviderCredentials, UserId,
};
use skyzen::routing::Router;
use skyzen::sql;
use skyzen_services::{Db, Queue};
use skyzen_test::mock::InMemoryQueue;

use crate::anthropic::{
    Account, AnthropicError, ClaudeClient, ClaudeOauth, TokenRequest, TokenSet,
};
use crate::app::router;
use crate::clouds::{CloudLink, Clouds};
use crate::config::{ApiConfig, ApiSettings};
use crate::error::ApiError;
use crate::github::{GithubClient, GithubError, GithubOauth, GithubToken, GithubUser};
use crate::google::{self, GoogleClient, GoogleError, GoogleOauth};
use crate::harness_accounts::StoredCredential;
use crate::microsoft::{
    self, AzureIdentity, AzureTokens, MicrosoftClient, MicrosoftError, MicrosoftOauth,
};
use crate::openai::{
    self, CodexClient, CodexOauth, DeviceAuth, DeviceCode, DevicePoll, OpenAiError,
};
use crate::rooms::{HostRooms, NativeHostRooms, NativeRooms, Rooms};
use crate::vendors::Vendors;

/// The schema every database-backed test starts from, in the order
/// `wrangler d1 migrations apply` would run it.
pub const MIGRATIONS: [&str; 24] = [
    include_str!("../../../migrations/0001_init.sql"),
    include_str!("../../../migrations/0002_sessions.sql"),
    include_str!("../../../migrations/0003_daemon.sql"),
    include_str!("../../../migrations/0004_registry.sql"),
    include_str!("../../../migrations/0005_session_env.sql"),
    include_str!("../../../migrations/0006_machines.sql"),
    include_str!("../../../migrations/0007_observations.sql"),
    include_str!("../../../migrations/0008_provisioning.sql"),
    include_str!("../../../migrations/0009_budget_metering.sql"),
    include_str!("../../../migrations/0010_harness_session.sql"),
    include_str!("../../../migrations/0012_harness_credentials.sql"),
    include_str!("../../../migrations/0013_session_title.sql"),
    include_str!("../../../migrations/0014_provider_workspace.sql"),
    include_str!("../../../migrations/0015_session_branch.sql"),
    include_str!("../../../migrations/0016_machine_facts.sql"),
    include_str!("../../../migrations/0017_spot_reclaim.sql"),
    include_str!("../../../migrations/0018_session_activity.sql"),
    include_str!("../../../migrations/0019_hosts.sql"),
    include_str!("../../../migrations/0020_provider_unlink.sql"),
    include_str!("../../../migrations/0021_session_model.sql"),
    include_str!("../../../migrations/0022_harness_usage.sql"),
    include_str!("../../../migrations/0023_machine_runtime.sql"),
    include_str!("../../../migrations/0024_usage_limit_pause.sql"),
    include_str!("../../../migrations/0025_session_permission_mode.sql"),
];

/// Client id the test configuration presents to GitHub.
pub const CLIENT_ID: &str = "Iv1.flyco-test-client";

/// Client secret the test configuration presents to GitHub.
pub const CLIENT_SECRET: &str = "flyco-test-client-secret";

/// Redirect URI the test configuration registers.
pub const REDIRECT_URI: &str = "https://flyco.test/v1/auth/github/callback";

/// A 32-byte AES key, hex-encoded.
pub const ENCRYPTION_KEY_HEX: &str =
    "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

/// Raw test-only P-256 VAPID private key.
pub const VAPID_PRIVATE_KEY: &str = "IQ9Ur0ykXoHS9gzfYX0aBjy9lvdrjx_PFUXmie9YRcY";

/// Contact URI carried by test VAPID signatures.
pub const VAPID_SUBJECT: &str = "mailto:me@lexo.cool";

/// Shared secret used to sign GitHub webhook fixtures.
pub const GITHUB_WEBHOOK_SECRET: &str = "a-shared-webhook-secret";

/// The GitHub access token [`TestGithub`] hands back.
pub const GITHUB_ACCESS_TOKEN: &str = "gho_test_access_token";

/// The GitHub account [`TestGithub`] resolves to.
pub const GITHUB_LOGIN: &str = "lexoliu";

/// GitHub's numeric id for [`GITHUB_LOGIN`].
pub const GITHUB_ID: i64 = 4_242;

/// A second GitHub account, for tests that check one user cannot reach
/// another's data.
pub const OTHER_LOGIN: &str = "octocat";

/// GitHub's numeric id for [`OTHER_LOGIN`].
pub const OTHER_GITHUB_ID: i64 = 8_484;

/// Client id the test configuration presents to Anthropic.
pub const CLAUDE_CLIENT_ID: &str = "flyco-test-claude-client";

/// Client id the test configuration presents to `OpenAI`.
pub const CODEX_CLIENT_ID: &str = "app_flyco-test-codex-client";

/// Client id the test configuration presents to Microsoft.
pub const AZURE_CLIENT_ID: &str = "flyco-test-microsoft-client";

/// Client secret the test configuration presents to Microsoft.
pub const AZURE_CLIENT_SECRET: &str = "flyco-test-microsoft-secret";

/// Client id the test configuration presents to Google.
pub const GOOGLE_CLIENT_ID: &str = "flyco-test.apps.googleusercontent.com";

/// Client secret the test configuration presents to Google.
pub const GOOGLE_CLIENT_SECRET: &str = "flyco-test-google-secret";

/// The bindings the test configuration is read from.
pub fn test_settings() -> ApiSettings {
    ApiSettings {
        github_client_id: CLIENT_ID.to_owned(),
        github_client_secret: CLIENT_SECRET.to_owned(),
        claude_oauth_client_id: CLAUDE_CLIENT_ID.to_owned(),
        codex_oauth_client_id: CODEX_CLIENT_ID.to_owned(),
        azure_oauth_client_id: AZURE_CLIENT_ID.to_owned(),
        azure_oauth_client_secret: AZURE_CLIENT_SECRET.to_owned(),
        google_oauth_client_id: GOOGLE_CLIENT_ID.to_owned(),
        google_oauth_client_secret: GOOGLE_CLIENT_SECRET.to_owned(),
        redirect_uri: REDIRECT_URI.to_owned(),
        encryption_key_hex: ENCRYPTION_KEY_HEX.to_owned(),
        vapid_private_key: VAPID_PRIVATE_KEY.to_owned(),
        vapid_subject: VAPID_SUBJECT.to_owned(),
        github_webhook_secret: GITHUB_WEBHOOK_SECRET.to_owned(),
    }
}

/// A configuration built from the constants above.
pub fn test_config() -> ApiConfig {
    ApiConfig::new(test_settings()).expect("the test configuration is valid")
}

/// Session rooms backed by skyzen's in-process simulator.
///
/// A fresh namespace per call: nothing in the unit tests reads a room back,
/// they only need somewhere for a broadcast to land, and sharing one
/// namespace between tests would share its event streams too.
#[must_use]
pub fn test_rooms() -> Rooms {
    Rooms::from_native(NativeRooms::new())
}

/// Host rooms backed by skyzen's in-process simulator.
///
/// A fresh namespace per call, for the reason above — and one a test *does*
/// read back: what a provisioning dispatch did is a container job sitting in
/// a host's mailbox, and this is where it lands.
#[must_use]
pub fn test_host_rooms() -> HostRooms {
    HostRooms::from_native(NativeHostRooms::new())
}

/// The display name [`TestGithub`] reports, which is what a session's
/// commits are authored under.
pub const GITHUB_NAME: &str = "Lexo Liu";

/// The address a session's commits are authored under, derived from
/// [`GITHUB_ID`] and [`GITHUB_LOGIN`] the way GitHub itself derives it.
pub const GITHUB_COMMIT_EMAIL: &str = "4242+lexoliu@users.noreply.github.com";

/// The repository every test session works in.
pub const TEST_REPO: &str = "lexoliu/flyco";

/// Its default branch, which is what a session that names none records.
pub const TEST_DEFAULT_BRANCH: &str = "dev";

/// What [`TestGithub`] reports as the default branch of any *other*
/// repository, so a test that opens a session somewhere else still records
/// a branch — and a test that confused the two would see it.
pub const OTHER_DEFAULT_BRANCH: &str = "main";

/// Branches [`TestGithub`] reports for [`TEST_REPO`], in GitHub's own
/// alphabetical order — so a picker that failed to hoist the default branch
/// would visibly open on the wrong one.
pub const TEST_BRANCHES: [&str; 3] = ["add-tests", "dev", "release-1.0"];

/// A [`GithubOauth`] that answers without a network.
///
/// The scopes it reports are a field rather than a constant: whether a
/// stored token grants `repo` is what decides between a session that opens
/// and one that tells the user to sign in again, and both answers need
/// exercising.
#[derive(Debug, Clone, Copy)]
pub struct TestGithub {
    /// What GitHub says this token was granted, as `X-OAuth-Scopes` would
    /// report it. `None` stands in for a response with no such header.
    pub scopes: Option<&'static [&'static str]>,
}

impl Default for TestGithub {
    /// A token authorized the way flyco's own sign-in asks for.
    fn default() -> Self {
        Self {
            scopes: Some(&["repo"]),
        }
    }
}

impl TestGithub {
    /// A token from a sign-in that predates flyco asking for `repo`.
    #[must_use]
    pub const fn without_repo_scope() -> Self {
        Self {
            scopes: Some(&["read:user"]),
        }
    }

    /// The two repositories the picker sees, so a filter has something to
    /// exclude.
    fn repos() -> Vec<flyco_core::RepoSummary> {
        vec![
            flyco_core::RepoSummary {
                slug: TEST_REPO.parse().expect("a valid slug"),
                private: true,
                default_branch: TEST_DEFAULT_BRANCH.parse().expect("a valid branch"),
                description: Some("agentic coding on the web".to_owned()),
                pushed_at_unix: Some(1_787_000_000),
            },
            flyco_core::RepoSummary {
                slug: "zen-rs/skyzen".parse().expect("a valid slug"),
                private: false,
                default_branch: "main".parse().expect("a valid branch"),
                description: None,
                pushed_at_unix: None,
            },
        ]
    }
}

impl GithubOauth for TestGithub {
    fn exchange_code(
        &self,
        client_id: &str,
        client_secret: &str,
        _code: &str,
        redirect_uri: &str,
    ) -> impl Future<Output = Result<GithubToken, GithubError>> + Send {
        assert_eq!(client_id, CLIENT_ID);
        assert_eq!(client_secret, CLIENT_SECRET);
        assert_eq!(redirect_uri, REDIRECT_URI);
        ready(Ok(GithubToken {
            access_token: GITHUB_ACCESS_TOKEN.to_owned(),
        }))
    }

    fn list_repos(
        &self,
        _token: &GithubToken,
    ) -> impl Future<Output = Result<Vec<flyco_core::RepoSummary>, GithubError>> + Send {
        ready(Ok(Self::repos()))
    }

    /// Answers for any repository, not only the two the picker lists.
    ///
    /// That is GitHub's own shape: `GET /user/repos` is the caller's own
    /// list, while `GET /repos/{slug}` answers for anything the token can
    /// see. A fake that 404'd outside its fixture list would refuse every
    /// test that opens a session on a repository of its own naming.
    fn get_repo(
        &self,
        _token: &GithubToken,
        slug: &flyco_core::RepoSlug,
    ) -> impl Future<Output = Result<flyco_core::RepoSummary, GithubError>> + Send {
        ready(Ok(Self::repos()
            .into_iter()
            .find(|repo| &repo.slug == slug)
            .unwrap_or_else(|| flyco_core::RepoSummary {
                slug: slug.clone(),
                private: false,
                default_branch: OTHER_DEFAULT_BRANCH.parse().expect("a valid branch"),
                description: None,
                pushed_at_unix: None,
            })))
    }

    fn list_branches(
        &self,
        _token: &GithubToken,
        _slug: &flyco_core::RepoSlug,
        page: u32,
    ) -> impl Future<Output = Result<crate::github::BranchListing, GithubError>> + Send {
        ready(Ok(crate::github::BranchListing {
            names: if page == 1 {
                TEST_BRANCHES
                    .iter()
                    .map(|name| name.parse().expect("a valid branch"))
                    .collect()
            } else {
                Vec::new()
            },
            has_more: false,
        }))
    }

    fn current_user(
        &self,
        token: &GithubToken,
    ) -> impl Future<Output = Result<crate::github::GithubIdentity, GithubError>> + Send {
        assert_eq!(token.access_token, GITHUB_ACCESS_TOKEN);
        ready(Ok(crate::github::GithubIdentity {
            user: GithubUser {
                id: GITHUB_ID,
                login: GITHUB_LOGIN.to_owned(),
                name: Some(GITHUB_NAME.to_owned()),
            },
            scopes: self
                .scopes
                .map(|scopes| scopes.iter().map(|scope| (*scope).to_owned()).collect()),
        }))
    }
}

/// The only authorization code [`TestClaude`] will redeem.
pub const CLAUDE_CODE: &str = "ac_a-pasted-authorization-code";

/// The access token a redeemed code yields.
pub const CLAUDE_ACCESS_TOKEN: &str = "sk-ant-oat01-exchanged";

/// The refresh token a redeemed code yields.
pub const CLAUDE_REFRESH_TOKEN: &str = "sk-ant-ort01-exchanged";

/// The access token a refresh yields, so a rotation is visible.
pub const CLAUDE_RENEWED_ACCESS_TOKEN: &str = "sk-ant-oat01-renewed";

/// The refresh token a refresh yields; grants rotate both halves.
pub const CLAUDE_RENEWED_REFRESH_TOKEN: &str = "sk-ant-ort01-renewed";

/// How long Anthropic says an issued access token lasts, in seconds.
pub const CLAUDE_TOKEN_LIFETIME: u64 = 8 * 60 * 60;

/// The address [`TestClaude`] reports, which becomes the account's label.
pub const CLAUDE_ACCOUNT_EMAIL: &str = "me@lexo.cool";

/// A [`ClaudeOauth`] that answers without a network.
///
/// It asserts what it was given rather than recording it: the client id and
/// the redirect URI are fixed by the deployment, and a PKCE flow that
/// exchanged a code without its verifier would be the bug worth catching.
/// What actually goes on the wire is pinned separately, against recorded
/// exchanges, in [`crate::anthropic`].
#[derive(Debug, Clone, Copy, Default)]
pub struct TestClaude;

impl TestClaude {
    /// The grant a fresh exchange hands back.
    fn issued() -> TokenSet {
        TokenSet {
            access_token: CLAUDE_ACCESS_TOKEN.to_owned(),
            refresh_token: CLAUDE_REFRESH_TOKEN.to_owned(),
            expires_in: CLAUDE_TOKEN_LIFETIME,
            account: Some(Account {
                email_address: Some(CLAUDE_ACCOUNT_EMAIL.to_owned()),
            }),
        }
    }

    /// The grant a refresh hands back.
    fn renewed() -> TokenSet {
        TokenSet {
            access_token: CLAUDE_RENEWED_ACCESS_TOKEN.to_owned(),
            refresh_token: CLAUDE_RENEWED_REFRESH_TOKEN.to_owned(),
            expires_in: CLAUDE_TOKEN_LIFETIME,
            account: Some(Account {
                email_address: Some(CLAUDE_ACCOUNT_EMAIL.to_owned()),
            }),
        }
    }

    /// Anthropic's own answer to a code or refresh token it will not take.
    fn refused() -> AnthropicError {
        AnthropicError::Rejected {
            code: "invalid_grant".to_owned(),
            description: "The authorization code is invalid or has expired.".to_owned(),
        }
    }
}

impl ClaudeOauth for TestClaude {
    fn exchange(
        &self,
        request: TokenRequest<'_>,
    ) -> impl Future<Output = Result<TokenSet, AnthropicError>> + Send {
        ready(match request {
            TokenRequest::AuthorizationCode {
                code,
                state,
                client_id,
                redirect_uri,
                code_verifier,
            } => {
                assert_eq!(client_id, CLAUDE_CLIENT_ID);
                assert_eq!(redirect_uri, crate::anthropic::REDIRECT_URI);
                assert!(!state.is_empty(), "the exchange carries the minted state");
                assert!(
                    !code_verifier.is_empty(),
                    "the exchange carries the PKCE verifier the challenge was made from"
                );
                if code == CLAUDE_CODE {
                    Ok(Self::issued())
                } else {
                    Err(Self::refused())
                }
            }
            TokenRequest::RefreshToken {
                refresh_token,
                client_id,
            } => {
                assert_eq!(client_id, CLAUDE_CLIENT_ID);
                if refresh_token == CLAUDE_REFRESH_TOKEN {
                    Ok(Self::renewed())
                } else {
                    Err(Self::refused())
                }
            }
        })
    }
}

/// The device authorization [`TestCodex`] creates.
pub const CODEX_DEVICE_AUTH_ID: &str = "devauth_flyco-test";

/// The one-time code [`TestCodex`] shows the user.
pub const CODEX_USER_CODE: &str = "FLYC-8QK2";

/// How often [`TestCodex`] says to poll.
pub const CODEX_POLL_INTERVAL_SECONDS: u64 = 5;

/// The authorization code an approved device authorization yields.
pub const CODEX_AUTHORIZATION_CODE: &str = "ac_flyco-test-device";

/// The PKCE verifier `OpenAI` hands back with it.
pub const CODEX_CODE_VERIFIER: &str = "cGtjZS12ZXJpZmllci1mcm9tLW9wZW5haQ";

/// The id token a redeemed code yields. Names the account and workspace.
pub const CODEX_ID_TOKEN: &str = include_str!("../fixtures/openai/id_token.jwt");

/// The access token a redeemed code yields; its `exp` is
/// [`CODEX_TOKEN_EXPIRY`].
pub const CODEX_ACCESS_TOKEN: &str = include_str!("../fixtures/openai/access_token.jwt");

/// The access token a refresh yields, so a rotation is visible.
pub const CODEX_RENEWED_ACCESS_TOKEN: &str =
    include_str!("../fixtures/openai/renewed_access_token.jwt");

/// The refresh token a redeemed code yields.
pub const CODEX_REFRESH_TOKEN: &str = "rt_flyco-test-issued";

/// The refresh token a refresh yields; grants rotate both halves.
pub const CODEX_RENEWED_REFRESH_TOKEN: &str = "rt_flyco-test-renewed";

/// `exp` of [`CODEX_ACCESS_TOKEN`].
pub const CODEX_TOKEN_EXPIRY: u64 = 1_787_003_600;

/// `exp` of [`CODEX_RENEWED_ACCESS_TOKEN`].
pub const CODEX_RENEWED_TOKEN_EXPIRY: u64 = 1_787_007_200;

/// `chatgpt_account_id` of [`CODEX_ID_TOKEN`].
pub const CODEX_ACCOUNT_ID: &str = "acc_01JD5XKQZ8";

/// The address [`CODEX_ID_TOKEN`] names, which becomes the account's label.
pub const CODEX_ACCOUNT_EMAIL: &str = "me@lexo.cool";

/// What [`TestCodex`] does when a device authorization is polled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CodexBehaviour {
    /// The user has already approved the code.
    #[default]
    Approved,
    /// Nobody has approved the code yet.
    Pending,
    /// `OpenAI` will not start a device sign-in for this account.
    DeviceAuthDisabled,
    /// `OpenAI` refuses whatever it is handed.
    Refused,
}

/// A [`CodexOauth`] that answers without a network.
///
/// It asserts the client id rather than recording it, for the reason
/// [`TestClaude`] does: what actually goes on the wire is pinned separately,
/// against recorded exchanges, in [`crate::openai`]. What varies here is
/// only *which* of `OpenAI`'s four answers a test wants.
#[derive(Debug, Clone, Copy, Default)]
pub struct TestCodex {
    /// Which answer this client gives.
    pub behaviour: CodexBehaviour,
}

impl TestCodex {
    /// A client whose device authorization is already approved.
    #[must_use]
    pub const fn approved() -> Self {
        Self {
            behaviour: CodexBehaviour::Approved,
        }
    }

    /// A client whose device authorization is still outstanding.
    #[must_use]
    pub const fn pending() -> Self {
        Self {
            behaviour: CodexBehaviour::Pending,
        }
    }

    /// A client whose account has device-code login switched off.
    #[must_use]
    pub const fn device_auth_disabled() -> Self {
        Self {
            behaviour: CodexBehaviour::DeviceAuthDisabled,
        }
    }

    /// A client that refuses every grant.
    #[must_use]
    pub const fn refused() -> Self {
        Self {
            behaviour: CodexBehaviour::Refused,
        }
    }

    /// `OpenAI`'s own answer to a grant it will not take.
    fn refusal() -> OpenAiError {
        OpenAiError::Rejected {
            code: "invalid_grant".to_owned(),
            description: "The authorization code is invalid or has expired.".to_owned(),
        }
    }
}

impl CodexOauth for TestCodex {
    fn request_user_code(
        &self,
        client_id: &str,
    ) -> impl Future<Output = Result<DeviceAuth, OpenAiError>> + Send {
        assert_eq!(client_id, CODEX_CLIENT_ID);
        ready(match self.behaviour {
            CodexBehaviour::DeviceAuthDisabled => Err(OpenAiError::DeviceAuthDisabled),
            _ => Ok(serde_json::from_value(serde_json::json!({
                "device_auth_id": CODEX_DEVICE_AUTH_ID,
                "user_code": CODEX_USER_CODE,
                "interval": CODEX_POLL_INTERVAL_SECONDS.to_string(),
            }))
            .expect("the fixture is a device authorization")),
        })
    }

    fn poll_device_code(
        &self,
        device_auth_id: &str,
        user_code: &str,
    ) -> impl Future<Output = Result<DevicePoll, OpenAiError>> + Send {
        assert_eq!(device_auth_id, CODEX_DEVICE_AUTH_ID);
        assert_eq!(user_code, CODEX_USER_CODE);
        ready(match self.behaviour {
            CodexBehaviour::Pending => Ok(DevicePoll::Pending),
            CodexBehaviour::Refused => Err(Self::refusal()),
            CodexBehaviour::DeviceAuthDisabled => Err(OpenAiError::DeviceAuthDisabled),
            CodexBehaviour::Approved => Ok(DevicePoll::Approved(
                serde_json::from_value::<DeviceCode>(serde_json::json!({
                    "authorization_code": CODEX_AUTHORIZATION_CODE,
                    "code_verifier": CODEX_CODE_VERIFIER,
                }))
                .expect("the fixture is a device code"),
            )),
        })
    }

    fn exchange(
        &self,
        request: openai::TokenRequest<'_>,
    ) -> impl Future<Output = Result<openai::TokenSet, OpenAiError>> + Send {
        if self.behaviour == CodexBehaviour::Refused {
            return ready(Err(Self::refusal()));
        }
        ready(match request {
            openai::TokenRequest::AuthorizationCode {
                code,
                redirect_uri,
                client_id,
                code_verifier,
            } => {
                assert_eq!(client_id, CODEX_CLIENT_ID);
                assert_eq!(redirect_uri, openai::REDIRECT_URI);
                assert_eq!(code_verifier, CODEX_CODE_VERIFIER);
                if code == CODEX_AUTHORIZATION_CODE {
                    Ok(openai::TokenSet {
                        id_token: Some(CODEX_ID_TOKEN.to_owned()),
                        access_token: Some(CODEX_ACCESS_TOKEN.to_owned()),
                        refresh_token: Some(CODEX_REFRESH_TOKEN.to_owned()),
                    })
                } else {
                    Err(Self::refusal())
                }
            }
            openai::TokenRequest::RefreshToken {
                refresh_token,
                client_id,
            } => {
                assert_eq!(client_id, CODEX_CLIENT_ID);
                if refresh_token == CODEX_REFRESH_TOKEN {
                    Ok(openai::TokenSet {
                        id_token: Some(CODEX_ID_TOKEN.to_owned()),
                        access_token: Some(CODEX_RENEWED_ACCESS_TOKEN.to_owned()),
                        refresh_token: Some(CODEX_RENEWED_REFRESH_TOKEN.to_owned()),
                    })
                } else {
                    Err(Self::refusal())
                }
            }
        })
    }
}

// ── The two cloud sign-ins ──

/// The authorization code [`TestMicrosoft`] and [`TestGoogle`] will redeem.
///
/// One constant for both: what a fake is asked to distinguish is the code it
/// was given from anything else, and two spellings of "the right code" would
/// only be two things to keep in step.
pub const CLOUD_CODE: &str = "the-authorization-code";

/// The Microsoft account [`TestMicrosoft`] reports.
pub const AZURE_ACCOUNT: &str = "me@lexo.cool";

/// The subscription [`TestMicrosoft`] offers.
pub const AZURE_SUBSCRIPTION_ID: &str = "00000000-1111-4222-8333-444444444444";

/// What it is called.
pub const AZURE_SUBSCRIPTION_NAME: &str = "Visual Studio Enterprise";

/// The directory the signed-in Microsoft account belongs to.
pub const AZURE_TENANT_ID: &str = "11111111-2222-4333-8444-555555555555";

/// The application id of the service principal [`TestMicrosoft`] creates.
pub const AZURE_APP_CLIENT_ID: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

/// The client secret it mints for it.
pub const AZURE_APP_CLIENT_SECRET: &str = "the-minted-client-secret";

/// The Google account [`TestGoogle`] reports.
pub const GCP_ACCOUNT: &str = "me@lexo.cool";

/// The project [`TestGoogle`] offers.
pub const GCP_PROJECT_ID: &str = "flyco-dev-4821";

/// What it is called.
pub const GCP_PROJECT_NAME: &str = "flyco dev";

/// The service-account key document [`TestGoogle`] mints.
pub const GCP_SERVICE_ACCOUNT_JSON: &str = include_str!("../fixtures/google/key_document.json");

/// What a cloud vendor does when a sign-in reaches it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CloudBehaviour {
    /// It signs the user in and creates what it is asked for.
    #[default]
    Succeeds,
    /// It refuses everything, the way it does when consent was declined.
    Refused,
}

/// A [`MicrosoftOauth`] that answers without a network.
///
/// It asserts the deployment's own client credentials rather than recording
/// them, for the reason [`TestClaude`] does: what actually goes on the wire
/// is pinned separately, against recorded exchanges, in
/// [`crate::microsoft`]. What varies here is only which of Microsoft's two
/// answers a test wants.
#[derive(Debug, Clone, Copy, Default)]
pub struct TestMicrosoft {
    /// Which answer this client gives.
    pub behaviour: CloudBehaviour,
}

impl TestMicrosoft {
    /// A Microsoft that signs the user in.
    #[must_use]
    pub const fn succeeding() -> Self {
        Self {
            behaviour: CloudBehaviour::Succeeds,
        }
    }

    /// A Microsoft that refuses.
    #[must_use]
    pub const fn refusing() -> Self {
        Self {
            behaviour: CloudBehaviour::Refused,
        }
    }

    /// Microsoft's own answer to something it will not take.
    fn refusal() -> MicrosoftError {
        MicrosoftError::Rejected {
            code: "invalid_grant".to_owned(),
            description: "The authorization code is invalid or has expired.".to_owned(),
        }
    }
}

impl MicrosoftOauth for TestMicrosoft {
    fn sign_in(
        &self,
        client: microsoft::OauthClient<'_>,
        code: &str,
        redirect_uri: &str,
    ) -> impl Future<Output = Result<microsoft::SignIn, MicrosoftError>> + Send {
        assert_eq!(client.id, AZURE_CLIENT_ID);
        assert_eq!(client.secret, AZURE_CLIENT_SECRET);
        assert!(
            redirect_uri.ends_with("/v1/providers/azure/oauth/callback"),
            "the exchange is bound to flyco's own callback: {redirect_uri}"
        );
        ready(
            if self.behaviour == CloudBehaviour::Succeeds && code == CLOUD_CODE {
                Ok(microsoft::SignIn {
                    account: AZURE_ACCOUNT.to_owned(),
                    choices: vec![flyco_core::ProviderOauthChoice {
                        id: AZURE_SUBSCRIPTION_ID.to_owned(),
                        name: AZURE_SUBSCRIPTION_NAME.to_owned(),
                    }],
                    tokens: AzureTokens {
                        tenant_id: AZURE_TENANT_ID.to_owned(),
                        arm_token: "the-arm-token".to_owned(),
                        graph_token: "the-graph-token".to_owned(),
                    },
                })
            } else {
                Err(Self::refusal())
            },
        )
    }

    fn create_identity(
        &self,
        tokens: &AzureTokens,
        subscription_id: &str,
    ) -> impl Future<Output = Result<AzureIdentity, MicrosoftError>> + Send {
        assert_eq!(tokens.tenant_id, AZURE_TENANT_ID);
        let tenant_id = tokens.tenant_id.clone();
        let chosen = subscription_id == AZURE_SUBSCRIPTION_ID;
        ready(if self.behaviour == CloudBehaviour::Succeeds && chosen {
            Ok(AzureIdentity {
                tenant_id,
                client_id: AZURE_APP_CLIENT_ID.to_owned(),
                client_secret: AZURE_APP_CLIENT_SECRET.to_owned(),
            })
        } else {
            Err(Self::refusal())
        })
    }
}

/// A [`GoogleOauth`] that answers without a network.
#[derive(Debug, Clone, Copy, Default)]
pub struct TestGoogle {
    /// Which answer this client gives.
    pub behaviour: CloudBehaviour,
}

impl TestGoogle {
    /// A Google that signs the user in.
    #[must_use]
    pub const fn succeeding() -> Self {
        Self {
            behaviour: CloudBehaviour::Succeeds,
        }
    }

    /// A Google that refuses.
    #[must_use]
    pub const fn refusing() -> Self {
        Self {
            behaviour: CloudBehaviour::Refused,
        }
    }

    /// Google's own answer to something it will not take.
    fn refusal() -> GoogleError {
        GoogleError::Rejected {
            status: "PERMISSION_DENIED".to_owned(),
            message: "The caller does not have permission.".to_owned(),
        }
    }
}

impl GoogleOauth for TestGoogle {
    fn sign_in(
        &self,
        client: google::OauthClient<'_>,
        code: &str,
        redirect_uri: &str,
    ) -> impl Future<Output = Result<google::SignIn, GoogleError>> + Send {
        assert_eq!(client.id, GOOGLE_CLIENT_ID);
        assert_eq!(client.secret, GOOGLE_CLIENT_SECRET);
        assert!(
            redirect_uri.ends_with("/v1/providers/gcp/oauth/callback"),
            "the exchange is bound to flyco's own callback: {redirect_uri}"
        );
        ready(
            if self.behaviour == CloudBehaviour::Succeeds && code == CLOUD_CODE {
                Ok(google::SignIn {
                    account: GCP_ACCOUNT.to_owned(),
                    choices: vec![flyco_core::ProviderOauthChoice {
                        id: GCP_PROJECT_ID.to_owned(),
                        name: GCP_PROJECT_NAME.to_owned(),
                    }],
                    access_token: "the-google-token".to_owned(),
                })
            } else {
                Err(Self::refusal())
            },
        )
    }

    fn create_identity(
        &self,
        token: &str,
        project_id: &str,
    ) -> impl Future<Output = Result<String, GoogleError>> + Send {
        assert_eq!(token, "the-google-token");
        ready(
            if self.behaviour == CloudBehaviour::Succeeds && project_id == GCP_PROJECT_ID {
                Ok(GCP_SERVICE_ACCOUNT_JSON.to_owned())
            } else {
                Err(Self::refusal())
            },
        )
    }
}

/// A [`CloudLink`] that checks a credential without a cloud account.
///
/// It reproduces the two answers the live one gives that anything depends
/// on: an Azure link owns the resource group flyco creates in the
/// subscription, and an enrolled machine is not linkable by presenting a
/// credential at all. Whether a *real* credential works is pinned where it
/// can be — against the recorded exchanges in `flyco_provider`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TestClouds;

impl CloudLink for TestClouds {
    fn prepare(
        &self,
        credentials: &ProviderCredentials,
        _login_key: &flyco_provider::LoginKey,
    ) -> impl Future<Output = Result<Option<String>, ApiError>> {
        ready(match credentials {
            ProviderCredentials::Azure { .. } => {
                Ok(Some(flyco_provider::azure::RESOURCE_GROUP.to_owned()))
            }
            ProviderCredentials::Host { .. } => Err(ApiError::HostNotLinkable),
            ProviderCredentials::Aws { .. } | ProviderCredentials::Gcp { .. } => Ok(None),
        })
    }
}

/// The cloud side of linking, as every test router carries it.
#[must_use]
pub fn test_clouds() -> Clouds {
    Clouds::Fake(TestClouds)
}

/// The vendor clients every test router carries.
#[must_use]
pub fn test_vendors() -> Vendors {
    Vendors::new(
        ClaudeClient::Fake(TestClaude),
        CodexClient::Fake(TestCodex::approved()),
        MicrosoftClient::Fake(TestMicrosoft::succeeding()),
        GoogleClient::Fake(TestGoogle::succeeding()),
    )
}

/// The full control-plane router, wired to [`TestGithub`], [`TestClaude`],
/// [`TestCodex`], `db`, and the provisioning queue its session routes
/// produce to.
pub fn test_router(db: Db, queue: Queue) -> Router {
    test_router_with(db, queue, TestGithub::default(), test_vendors())
}

/// The same router, against a GitHub whose token says something else.
pub fn test_router_with_github(db: Db, queue: Queue, github: TestGithub) -> Router {
    test_router_with(db, queue, github, test_vendors())
}

/// The same router, with the GitHub and vendor clients the caller chose.
pub fn test_router_with(db: Db, queue: Queue, github: TestGithub, vendors: Vendors) -> Router {
    router(
        test_config(),
        GithubClient::Fake(github),
        vendors,
        test_clouds(),
        db,
        queue,
    )
}

/// A migrated database plus the router that talks to it.
///
/// The provisioning queue is created here and kept by the router alone,
/// because most tests only need session creation to *accept* a job. A test
/// that has to read one back hands in its own with [`migrated_router_on`].
pub async fn migrated_router(db: &Db) -> Router {
    migrated_router_on(db, Queue::new(InMemoryQueue::new())).await
}

/// A migrated database plus a router producing to a queue the caller holds.
pub async fn migrated_router_on(db: &Db, queue: Queue) -> Router {
    migrate(db).await;
    test_router(db.clone(), queue)
}

/// Applies every migration to a fresh in-memory database.
///
/// `Db` executes one statement per call, so each file is split on statement
/// boundaries first. Deliberately not skyzen's migration runner: the
/// deployed schema is applied by `wrangler d1 migrations apply`, and a
/// second runner keeping its own bookkeeping table would be a second
/// opinion about which migrations a database has.
pub async fn migrate(db: &Db) {
    for migration in MIGRATIONS {
        for statement in statements(migration) {
            db.query(&statement)
                .execute()
                .await
                .unwrap_or_else(|error| panic!("failed to apply `{statement}`: {error}"));
        }
    }
}

/// Splits a migration file into executable statements, dropping `--` comments.
fn statements(sql: &str) -> Vec<String> {
    let without_comments = sql
        .lines()
        .map(|line| line.split_once("--").map_or(line, |(code, _)| code))
        .collect::<Vec<_>>()
        .join("\n");

    without_comments
        .split(';')
        .map(str::trim)
        .filter(|statement| !statement.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// Creates a user row directly, for tests that need an authenticated caller
/// without going through the OAuth flow.
pub async fn seed_user(db: &Db) -> CurrentUser {
    seed_account(db, GITHUB_ID, GITHUB_LOGIN).await
}

/// Creates a second, unrelated account.
pub async fn seed_other_user(db: &Db) -> CurrentUser {
    seed_account(db, OTHER_GITHUB_ID, OTHER_LOGIN).await
}

/// Opens a provisioning session owned by `user`, for tests that need one to
/// exist without going through `POST /v1/sessions`.
pub async fn seed_session(db: &Db, user: &CurrentUser) -> flyco_core::SessionId {
    crate::sessions::create(
        db,
        flyco_core::SESSION_CAP_MAX,
        crate::sessions::Opening {
            user: user.id,
            title: SEEDED_TITLE,
            harness: flyco_core::HarnessKind::ClaudeCode,
            repo: &TEST_REPO.parse().expect("a valid repo slug"),
            branch: &TEST_DEFAULT_BRANCH.parse().expect("a valid branch"),
            machine_origin: flyco_core::MachineOrigin::Auto,
            budget: flyco_core::BudgetConfig::new(flyco_core::Usd::from_dollars(10))
                .expect("a valid budget"),
            model: &flyco_core::ModelChoice::default_of(&flyco_core::builtin_models(
                flyco_core::HarnessKind::ClaudeCode,
            )),
        },
    )
    .await
    .expect("seed a session")
    .summary
    .id
}

/// Title a seeded session carries, standing in for the excerpt a real
/// session takes from its opening prompt.
pub const SEEDED_TITLE: &str = "wire up the relay";

async fn seed_account(db: &Db, github_id: i64, login: &str) -> CurrentUser {
    let sealed = test_config()
        .token_cipher()
        .seal(GITHUB_ACCESS_TOKEN)
        .expect("seal");
    crate::users::upsert_from_github(
        db,
        &GithubUser {
            id: github_id,
            login: login.to_owned(),
            name: Some(GITHUB_NAME.to_owned()),
        },
        &sealed,
    )
    .await
    .expect("seed the user row")
}

/// The hostname the enrolled machine every test provisions onto reports.
///
/// A real address rather than a placeholder, because it is also the machine
/// type and the region a host's catalog offers: the tests assert against the
/// same string the planner reads out of the machine's own facts.
pub const SSH_HOST: &str = "build.lexo.cool";

/// What the fixture machine says about itself.
///
/// Big enough for flyco to pick on its own, so a test that leaves the
/// machine to flyco gets this one rather than a refusal.
#[must_use]
pub fn host_facts() -> HostFacts {
    HostFacts {
        architecture: CpuArchitecture::Arm64,
        vcpus: 10,
        memory_mib: 32 * 1024,
        disk_free_gib: 400,
        podman_version: "5.4.0".to_owned(),
        kernel: "6.11.0-19-generic".to_owned(),
        hostname: SSH_HOST.to_owned(),
    }
}

/// Enrols a machine and its provider account, exactly as
/// `POST /v1/hosts/enroll` writes them.
///
/// A host is the provider whose catalog needs no network: it reports the one
/// machine it is, out of the facts it enrolled with. That is what lets
/// creation-time validation and the whole provisioning queue be exercised
/// without a cloud account.
///
/// Seeded `online`, because what a test is usually about starts after the
/// machine has arrived — the enrollment tests drive the real routes instead.
pub async fn seed_host(db: &Db, user: UserId) -> HostId {
    let id = HostId::generate();
    let facts = serde_json::to_string(&host_facts()).expect("encode host facts");
    let online = flyco_core::HostState::Online;
    let created_at = 1_787_000_000_u64;
    sql!(
        db,
        "INSERT INTO hosts (id, user_id, label, facts, token_hash, state, created_at_unix) \
         VALUES ({id}, {user}, {SSH_HOST}, {facts}, {crate::crypto::token_hash(HOST_TOKEN)}, \
                 {online}, {created_at})"
    )
    .execute()
    .await
    .expect("enrol a host");
    id
}

/// The token the seeded machine authenticates with.
pub const HOST_TOKEN: &str = "fh_a-live-host-token";

/// Links the provider account a seeded host provisions through.
pub async fn seed_provider_account(db: &Db, user: UserId) -> ProviderAccountId {
    seed_host_account(db, user).await.1
}

/// The same, answering with the machine as well as its account.
pub async fn seed_host_account(db: &Db, user: UserId) -> (HostId, ProviderAccountId) {
    let host = seed_host(db, user).await;
    let account = crate::provider_accounts::create(
        db,
        &test_config(),
        user,
        SSH_HOST.to_owned(),
        &ProviderCredentials::Host { host },
        Some(host),
    )
    .await
    .expect("link a host account")
    .id;
    (host, account)
}

/// The machine a test session asks for: the enrolled machine itself.
#[must_use]
pub fn machine_choice(account: ProviderAccountId) -> MachineChoice {
    MachineChoice {
        provider_account: account,
        machine_type: SSH_HOST.to_owned(),
        // An enrolled host offers exactly one entry, and it is a container:
        // a session there has always been a Podman container.
        runtime: flyco_core::Runtime::Container,
        region: SSH_HOST.to_owned(),
        spot: true,
        disk_gib: flyco_core::DEFAULT_DISK_GIB,
    }
}

/// Test secret a linked harness account seals.
///
/// Provisioned onto every machine a Claude Code session runs on, which is
/// why the provisioning tests assert on it: a credential that does not reach
/// the daemon is a session whose agent cannot sign in.
pub const HARNESS_TOKEN: &str = "sk-ant-oat01-a-linked-account";

/// Links a harness account, sealed the way the authenticated link route does.
pub async fn seed_harness_account(db: &Db, user: UserId, harness: HarnessKind) -> HarnessAccountId {
    let credential = match harness {
        HarnessKind::ClaudeCode => StoredCredential::OauthToken {
            token: HARNESS_TOKEN.to_owned(),
        },
        HarnessKind::Codex => StoredCredential::ApiKey {
            key: HARNESS_TOKEN.to_owned(),
        },
    };
    seed_credential(db, user, harness, &credential).await
}

/// Links a Claude account holding an OAuth grant that expires at
/// `expires_at_unix`, for the paths that refresh one before using it.
pub async fn seed_claude_oauth_account(
    db: &Db,
    user: UserId,
    expires_at_unix: u64,
) -> HarnessAccountId {
    seed_credential(
        db,
        user,
        HarnessKind::ClaudeCode,
        &StoredCredential::ClaudeOauth {
            access_token: CLAUDE_ACCESS_TOKEN.to_owned(),
            refresh_token: CLAUDE_REFRESH_TOKEN.to_owned(),
            expires_at_unix,
        },
    )
    .await
}

/// Links a Codex account holding a `ChatGPT` grant that expires at
/// `expires_at_unix`, for the paths that refresh one before using it.
pub async fn seed_codex_oauth_account(
    db: &Db,
    user: UserId,
    expires_at_unix: u64,
) -> HarnessAccountId {
    seed_credential(
        db,
        user,
        HarnessKind::Codex,
        &StoredCredential::CodexOauth {
            id_token: CODEX_ID_TOKEN.to_owned(),
            access_token: CODEX_ACCESS_TOKEN.to_owned(),
            refresh_token: CODEX_REFRESH_TOKEN.to_owned(),
            account_id: CODEX_ACCOUNT_ID.to_owned(),
            expires_at_unix,
        },
    )
    .await
}

/// Writes one sealed credential as a linked account.
async fn seed_credential(
    db: &Db,
    user: UserId,
    harness: HarnessKind,
    credential: &StoredCredential,
) -> HarnessAccountId {
    let id = HarnessAccountId::generate();
    let encoded = serde_json::to_string(credential).expect("encode a harness credential");
    let sealed = test_config()
        .token_cipher()
        .seal(&encoded)
        .expect("seal a harness credential");
    let label = "lexo@lexo.cool".to_owned();
    let linked_at = 1_787_000_000_u64;
    let expires_at = credential.expires_at_unix();

    sql!(
        db,
        "INSERT INTO harness_accounts \
         (id, user_id, harness, label, credential_enc, linked_at_unix, expires_at_unix) \
         VALUES ({id}, {user}, {harness}, {label}, {sealed}, {linked_at}, {expires_at})"
    )
    .execute()
    .await
    .expect("link a harness account");
    id
}
