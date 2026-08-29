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

use crate::app::router;
use crate::config::ApiConfig;
use crate::github::{GithubClient, GithubError, GithubOauth, GithubToken, GithubUser};

/// The schema every database-backed test starts from, in the order
/// `wrangler d1 migrations apply` would run it.
pub const MIGRATIONS: [&str; 10] = [
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

/// A configuration built from the constants above.
pub fn test_config() -> ApiConfig {
    ApiConfig::new(
        CLIENT_ID.to_owned(),
        CLIENT_SECRET.to_owned(),
        REDIRECT_URI,
        ENCRYPTION_KEY_HEX,
    )
    .expect("the test configuration is valid")
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

/// The full control-plane router, wired to [`TestGithub`], `db`, and the
/// provisioning queue its session routes produce to.
pub fn test_router(db: Db, queue: Queue) -> Router {
    router(test_config(), GithubClient::Fake(TestGithub), db, queue)
}

/// A migrated database plus the router that talks to it.
///
/// The provisioning queue is created here and kept by the router alone,
/// because most tests only need session creation to *accept* a job. A test
/// that has to read one back hands in its own with [`migrated_router_on`].
pub async fn migrated_router(db: &Db) -> Router {
    migrated_router_on(db, Queue::new(InMemoryQueue::new())).await
}

/// A migrated database plus a router built around a caller's configuration.
///
/// For the routes whose behaviour *is* their configuration — harness
/// linking, web push — where the difference between configured and not is
/// the thing under test.
pub async fn migrated_router_with_config(db: &Db, config: ApiConfig) -> Router {
    migrate(db).await;
    router(
        config,
        GithubClient::Fake(TestGithub),
        db.clone(),
        Queue::new(InMemoryQueue::new()),
    )
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
        user.id,
        flyco_core::SESSION_CAP_MAX,
        flyco_core::HarnessKind::ClaudeCode,
        &"lexoliu/flyco".parse().expect("a valid repo slug"),
        flyco_core::BudgetConfig::new(flyco_core::Usd::from_dollars(10)).expect("a valid budget"),
    )
    .await
    .expect("seed a session")
    .summary
    .id
}

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

/// The Claude OAuth token the linked harness account seals.
///
/// Provisioned onto every machine a Claude Code session runs on, which is
/// why the provisioning tests assert on it: a credential that does not reach
/// the daemon is a session whose agent cannot sign in.
pub const HARNESS_TOKEN: &str = "sk-ant-oat01-a-linked-account";

/// Links a harness account, sealed the way the link callback will seal one.
pub async fn seed_harness_account(db: &Db, user: UserId, harness: HarnessKind) -> HarnessAccountId {
    let id = HarnessAccountId::generate();
    let sealed = test_config()
        .token_cipher()
        .seal(HARNESS_TOKEN)
        .expect("seal a harness token");
    let label = "lexo@lexo.cool".to_owned();
    let linked_at = 1_787_000_000_u64;

    sql!(
        db,
        "INSERT INTO harness_accounts \
         (id, user_id, harness, label, token_enc, linked_at_unix) \
         VALUES ({id}, {user}, {harness}, {label}, {sealed}, {linked_at})"
    )
    .execute()
    .await
    .expect("link a harness account");
    id
}
