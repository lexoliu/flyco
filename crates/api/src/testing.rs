//! Fixtures shared by the crate's unit tests.

use core::future::{Future, ready};

use flyco_core::{
    CurrentUser, HarnessAccountId, HarnessKind, MachineChoice, ProviderAccountId,
    ProviderCredentials, UserId,
};
use skyzen::routing::Router;
use skyzen::sql;
use skyzen_services::{Db, Queue};
use skyzen_test::mock::InMemoryQueue;

use crate::anthropic::{
    Account, AnthropicError, ClaudeClient, ClaudeOauth, TokenRequest, TokenSet,
};
use crate::app::router;
use crate::config::{ApiConfig, ApiSettings};
use crate::github::{GithubClient, GithubError, GithubOauth, GithubToken, GithubUser};
use crate::harness_accounts::StoredCredential;
use crate::rooms::{NativeRooms, Rooms};

/// The schema every database-backed test starts from, in the order
/// `wrangler d1 migrations apply` would run it.
pub const MIGRATIONS: [&str; 12] = [
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

/// The bindings the test configuration is read from.
pub fn test_settings() -> ApiSettings {
    ApiSettings {
        github_client_id: CLIENT_ID.to_owned(),
        github_client_secret: CLIENT_SECRET.to_owned(),
        claude_oauth_client_id: CLAUDE_CLIENT_ID.to_owned(),
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

/// A [`GithubOauth`] that answers without a network.
#[derive(Debug, Clone, Copy, Default)]
pub struct TestGithub;

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

    /// Two repositories, so a filter has something to exclude.
    fn list_repos(
        &self,
        _token: &GithubToken,
    ) -> impl Future<Output = Result<Vec<flyco_core::RepoSummary>, GithubError>> + Send {
        ready(Ok(vec![
            flyco_core::RepoSummary {
                slug: "lexoliu/flyco".parse().expect("a valid slug"),
                private: true,
                default_branch: "dev".to_owned(),
                description: Some("agentic coding on the web".to_owned()),
                pushed_at_unix: Some(1_787_000_000),
            },
            flyco_core::RepoSummary {
                slug: "zen-rs/skyzen".parse().expect("a valid slug"),
                private: false,
                default_branch: "main".to_owned(),
                description: None,
                pushed_at_unix: None,
            },
        ]))
    }

    fn current_user(
        &self,
        token: &GithubToken,
    ) -> impl Future<Output = Result<GithubUser, GithubError>> + Send {
        assert_eq!(token.access_token, GITHUB_ACCESS_TOKEN);
        ready(Ok(GithubUser {
            id: GITHUB_ID,
            login: GITHUB_LOGIN.to_owned(),
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

/// The full control-plane router, wired to [`TestGithub`], [`TestClaude`],
/// `db`, and the provisioning queue its session routes produce to.
pub fn test_router(db: Db, queue: Queue) -> Router {
    router(
        test_config(),
        GithubClient::Fake(TestGithub),
        ClaudeClient::Fake(TestClaude),
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
            repo: &"lexoliu/flyco".parse().expect("a valid repo slug"),
            machine_origin: flyco_core::MachineOrigin::Auto,
            budget: flyco_core::BudgetConfig::new(flyco_core::Usd::from_dollars(10))
                .expect("a valid budget"),
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
        },
        &sealed,
    )
    .await
    .expect("seed the user row")
}

/// The registered host every test provisions onto.
///
/// A real address rather than a placeholder, because it is also the machine
/// type and the region a byo-ssh catalog reports: the tests assert against
/// the same string the driver derives from the credentials.
pub const SSH_HOST: &str = "build.lexo.cool";

/// The SHA-256 host key fingerprint the fixture registers.
pub const SSH_FINGERPRINT: &str = "SHA256:qWyVLPxNBRr7Nnkm1xTQKMDcXwHFsSFRnLW6iNfPmcQ";

/// Links a byo-ssh provider account, sealed exactly as the link route seals
/// one.
///
/// byo-ssh is the provider whose catalog needs no network: it reports the
/// one machine it is. That is what lets creation-time validation and the
/// whole provisioning queue be exercised without a cloud account.
pub async fn seed_provider_account(db: &Db, user: UserId) -> ProviderAccountId {
    let credentials = ProviderCredentials::ByoSsh {
        host: SSH_HOST.to_owned(),
        port: 22,
        user: "flyco".to_owned(),
        private_key: "-----BEGIN OPENSSH PRIVATE KEY-----\nnot-a-real-key\n".to_owned(),
        host_fingerprint: SSH_FINGERPRINT.to_owned(),
    };
    let sealed = test_config()
        .token_cipher()
        .seal(&serde_json::to_string(&credentials).expect("encode credentials"))
        .expect("seal credentials");

    let id = ProviderAccountId::generate();
    let kind = credentials.kind();
    let label = "the laptop".to_owned();
    let linked_at = 1_787_000_000_u64;
    sql!(
        db,
        "INSERT INTO provider_accounts \
         (id, user_id, kind, label, credentials_enc, linked_at_unix) \
         VALUES ({id}, {user}, {kind}, {label}, {sealed}, {linked_at})"
    )
    .execute()
    .await
    .expect("link a provider account");
    id
}

/// The machine a test session asks for: the registered host itself.
#[must_use]
pub fn machine_choice(account: ProviderAccountId) -> MachineChoice {
    MachineChoice {
        provider_account: account,
        machine_type: SSH_HOST.to_owned(),
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
