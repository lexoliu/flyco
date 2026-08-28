//! Fixtures shared by the crate's unit tests.

use core::future::{Future, ready};

use flyco_core::CurrentUser;
use skyzen::routing::Router;
use skyzen_services::Db;

use crate::app::router;
use crate::config::ApiConfig;
use crate::github::{GithubError, GithubOauth, GithubToken, GithubUser};

/// The schema every database-backed test starts from, in the order
/// `wrangler d1 migrations apply` would run it.
const MIGRATIONS: [&str; 5] = [
    include_str!("../../../migrations/0001_init.sql"),
    include_str!("../../../migrations/0002_sessions.sql"),
    include_str!("../../../migrations/0003_daemon.sql"),
    include_str!("../../../migrations/0004_registry.sql"),
    include_str!("../../../migrations/0005_session_env.sql"),
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

/// The full control-plane router, wired to [`TestGithub`] and `db`.
pub fn test_router(db: Db) -> Router {
    router(test_config(), TestGithub, db)
}

/// A migrated database plus the router that talks to it.
pub async fn migrated_router(db: &Db) -> Router {
    migrate(db).await;
    test_router(db.clone())
}

/// Applies `migrations/0001_init.sql` to a fresh in-memory database.
///
/// Skyzen 0.1.2's `Db` executes one statement per call, so the file is split
/// on statement boundaries first.
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
