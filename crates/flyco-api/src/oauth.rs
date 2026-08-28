//! GitHub OAuth: the two endpoints that turn a browser into a flyco session.
//!
//! Flyco has no password of its own. `start` hands the browser an authorize
//! URL and remembers the single-use `state` it minted; `callback` refuses
//! anything it does not recognise, exchanges the code, and issues the
//! session cookie.

use flyco_core::AuthorizeUrl;
use serde::Deserialize;
use skyzen::extract::Query;
use skyzen::utils::cookie::CookieJar;
use skyzen::utils::{Json, State};
use skyzen::{Body, Response, StatusCode, header};
use skyzen_services::{Db, Kv};
use url::Url;

use crate::config::ApiConfig;
use crate::crypto::random_token;
use crate::error::ApiError;
use crate::github::{GithubOauth, SCOPE};
use crate::{expiring, session, users};

/// Where the browser is sent to approve the OAuth app.
const AUTHORIZE_URL: &str = "https://github.com/login/oauth/authorize";

/// How long a browser has to complete the round trip.
const STATE_TTL_SECONDS: u64 = 10 * 60;

/// Where the browser lands once the session cookie is set.
const POST_LOGIN_PATH: &str = "/";

/// Query string GitHub appends when it redirects back.
#[derive(Debug, Deserialize)]
pub struct Callback {
    /// The single-use authorization code.
    code: String,
    /// The `state` this control plane minted in [`start`].
    state: String,
}

/// GitHub's authorize endpoint as a parsed URL.
fn authorize_endpoint() -> Url {
    Url::parse(AUTHORIZE_URL).expect("the GitHub authorize URL is a valid absolute URL")
}

fn state_key(state: &str) -> String {
    let mut key = String::with_capacity(17 + state.len());
    key.push_str("auth:oauth-state:");
    key.push_str(state);
    key
}

/// `POST /v1/auth/github/start` — begins a GitHub sign-in.
///
/// # Errors
///
/// Returns [`ApiError`] if entropy is unavailable or KV rejects the write.
pub async fn start(
    State(config): State<ApiConfig>,
    kv: Kv,
) -> Result<Json<AuthorizeUrl>, ApiError> {
    let state = random_token()?;
    expiring::put(&kv, &state_key(&state), &(), STATE_TTL_SECONDS).await?;

    let mut authorize_url = authorize_endpoint();
    authorize_url
        .query_pairs_mut()
        .append_pair("client_id", config.github_client_id())
        .append_pair("redirect_uri", config.redirect_uri().as_str())
        .append_pair("scope", SCOPE)
        .append_pair("state", &state);

    tracing::debug!("issued a GitHub authorize URL");
    Ok(Json(AuthorizeUrl {
        authorize_url: authorize_url.into(),
    }))
}

/// `GET /v1/auth/github/callback` — completes a GitHub sign-in.
///
/// Consumes the `state`, exchanges the code, upserts the account, and
/// redirects the browser home carrying the session cookie.
///
/// # Errors
///
/// Returns [`ApiError::UnknownOauthState`] when the `state` is not one this
/// control plane issued and has not already spent, or the underlying
/// GitHub, KV, database, or cryptography failure otherwise.
pub async fn callback<G: GithubOauth>(
    Query(callback): Query<Callback>,
    State(config): State<ApiConfig>,
    State(github): State<G>,
    kv: Kv,
    db: Db,
) -> Result<(Response, CookieJar), ApiError> {
    if expiring::take::<()>(&kv, &state_key(&callback.state))
        .await?
        .is_none()
    {
        return Err(ApiError::UnknownOauthState);
    }

    let token = github
        .exchange_code(
            config.github_client_id(),
            config.github_client_secret(),
            &callback.code,
            config.redirect_uri().as_str(),
        )
        .await?;
    let account = github.current_user(&token).await?;

    let sealed = config.token_cipher().seal(&token.access_token)?;
    let user = users::upsert_from_github(&db, &account, &sealed).await?;
    let session_token = session::issue(&kv, user.id).await?;

    tracing::info!(login = %user.login, "completed a GitHub sign-in");

    let mut jar = empty_jar();
    jar.add(session::cookie(session_token));
    Ok((see_other(POST_LOGIN_PATH), jar))
}

/// Skyzen 0.1.2's `CookieJar` is only constructible by parsing a `Cookie`
/// header; the empty header yields the empty jar a response starts from.
fn empty_jar() -> CookieJar {
    "".parse()
        .expect("an empty cookie header parses into an empty jar")
}

fn see_other(location: &'static str) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SEE_OTHER;
    response
        .headers_mut()
        .insert(header::LOCATION, header::HeaderValue::from_static(location));
    response
}

#[cfg(test)]
mod tests {
    use flyco_core::AuthorizeUrl;
    use skyzen_services::{Db, Kv};
    use skyzen_test::TestContext;
    use url::Url;

    use super::state_key;
    use crate::testing::{
        CLIENT_ID, GITHUB_ACCESS_TOKEN, GITHUB_ID, GITHUB_LOGIN, REDIRECT_URI, migrated_router,
        test_config,
    };
    use crate::{expiring, session, users};

    async fn begin(client: &skyzen_test::TestClient<skyzen::routing::Router>) -> String {
        let response = client.post("/v1/auth/github/start").send().await;
        response.assert_status(200);
        let body: AuthorizeUrl = response.json();

        let url = Url::parse(&body.authorize_url).expect("the authorize URL is absolute");
        assert_eq!(url.host_str(), Some("github.com"));

        let query: Vec<(String, String)> = url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        assert!(query.contains(&("client_id".to_owned(), CLIENT_ID.to_owned())));
        assert!(query.contains(&("scope".to_owned(), "repo".to_owned())));
        assert!(query.contains(&("redirect_uri".to_owned(), REDIRECT_URI.to_owned())));

        query
            .into_iter()
            .find_map(|(key, value)| (key == "state").then_some(value))
            .expect("the authorize URL carries a state")
    }

    #[skyzen::test]
    async fn start_mints_a_state_and_records_it(ctx: TestContext, kv: Kv, db: Db) {
        let client = ctx.client(migrated_router(&db).await);
        let state = begin(&client).await;

        assert!(
            expiring::get::<()>(&kv, &state_key(&state))
                .await
                .expect("read the state")
                .is_some()
        );
    }

    #[skyzen::test]
    async fn a_callback_with_an_unknown_state_is_rejected(ctx: TestContext, _kv: Kv, db: Db) {
        let client = ctx.client(migrated_router(&db).await);
        let response = client
            .get("/v1/auth/github/callback?code=abc&state=never-issued")
            .send()
            .await;

        response.assert_status(400);
    }

    #[skyzen::test]
    async fn a_state_is_only_good_once(ctx: TestContext, _kv: Kv, db: Db) {
        let client = ctx.client(migrated_router(&db).await);
        let state = begin(&client).await;
        let path = format!("/v1/auth/github/callback?code=abc&state={state}");

        client.get(&path).send().await.assert_status(303);
        client.get(&path).send().await.assert_status(400);
    }

    #[skyzen::test]
    async fn a_callback_signs_the_user_in(ctx: TestContext, kv: Kv, db: Db) {
        let client = ctx.client(migrated_router(&db).await);
        let state = begin(&client).await;

        let response = client
            .get(&format!("/v1/auth/github/callback?code=abc&state={state}"))
            .send()
            .await;

        response.assert_status(303);
        response.assert_header("location", "/");

        let set_cookie = response
            .headers()
            .get("set-cookie")
            .expect("the response sets a cookie")
            .to_str()
            .expect("the cookie is ASCII");
        assert!(set_cookie.starts_with("flyco_session="));
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("Secure"));
        assert!(set_cookie.contains("SameSite=Lax"));

        let token = set_cookie
            .trim_start_matches("flyco_session=")
            .split(';')
            .next()
            .expect("the cookie has a value");
        let user_id = session::resolve(&kv, token)
            .await
            .expect("resolve the session")
            .expect("the session cookie is live");

        let user = users::find(&db, user_id)
            .await
            .expect("read the user")
            .expect("the callback created the user row");
        assert_eq!(user.login, GITHUB_LOGIN);
    }

    #[skyzen::test]
    async fn the_github_token_is_stored_sealed(ctx: TestContext, _kv: Kv, db: Db) {
        let client = ctx.client(migrated_router(&db).await);
        let state = begin(&client).await;
        client
            .get(&format!("/v1/auth/github/callback?code=abc&state={state}"))
            .send()
            .await
            .assert_status(303);

        let stored: Vec<String> = db
            .query("SELECT github_token_enc FROM users")
            .fetch_all::<std::collections::BTreeMap<String, String>>()
            .await
            .expect("read the stored token")
            .into_iter()
            .filter_map(|row| row.get("github_token_enc").cloned())
            .collect();

        let sealed = stored.first().expect("one user row exists");
        assert!(!sealed.contains(GITHUB_ACCESS_TOKEN));
        assert_eq!(
            test_config().token_cipher().open(sealed).expect("unseal"),
            GITHUB_ACCESS_TOKEN
        );
    }

    #[skyzen::test]
    async fn a_second_sign_in_reuses_the_same_flyco_user(ctx: TestContext, _kv: Kv, db: Db) {
        let client = ctx.client(migrated_router(&db).await);

        let mut ids = Vec::new();
        for _ in 0..2 {
            let state = begin(&client).await;
            client
                .get(&format!("/v1/auth/github/callback?code=abc&state={state}"))
                .send()
                .await
                .assert_status(303);

            let row: std::collections::BTreeMap<String, String> = db
                .query("SELECT id FROM users WHERE github_id = ?")
                .bind(GITHUB_ID)
                .fetch_one()
                .await
                .expect("exactly one row per GitHub account");
            ids.push(row.get("id").cloned().expect("id column"));
        }

        assert_eq!(ids[0], ids[1]);
    }
}
