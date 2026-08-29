//! GitHub OAuth: the two endpoints that turn a browser into a flyco session.
//!
//! Flyco has no password of its own. `start` hands the browser an authorize
//! URL and remembers the single-use `state` it minted; `callback` refuses
//! anything it does not recognise, exchanges the code, and hands the session
//! token to the SPA.
//!
//! The token rides the redirect's URL *fragment*, never its query string: a
//! fragment is not sent to the server, so it stays out of access logs, out
//! of `Referer` headers, and out of anything a proxy records.

use flyco_core::AuthorizeUrl;
use serde::Deserialize;
use skyzen::extract::Query;
use skyzen::utils::{Json, State};
use skyzen::{Body, Response, StatusCode, header};
use skyzen_services::{Db, Kv};
use url::Url;

use crate::config::ApiConfig;
use crate::crypto::random_token;
use crate::error::ApiError;
use crate::github::{GithubOauth, SCOPE};
use crate::problem::Outcome;
use crate::{expiring, session, users};

/// Where the browser is sent to approve the OAuth app.
const AUTHORIZE_URL: &str = "https://github.com/login/oauth/authorize";

/// How long a browser has to complete the round trip.
const STATE_TTL_SECONDS: u64 = 10 * 60;

/// Where the browser lands once it holds a session token. Resolved against
/// the configured redirect URI, so it always shares the callback's origin.
const POST_LOGIN_PATH: &str = "/auth/complete";

/// Fragment parameter the SPA reads the session token out of.
const TOKEN_PARAM: &str = "token";

/// Query string GitHub appends when it redirects back.
#[derive(Debug, Deserialize, skyzen::ToSchema)]
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

/// Begins a GitHub sign-in, returning the URL to send the browser to.
#[skyzen::openapi]
pub async fn start(State(config): State<ApiConfig>, kv: Kv) -> Outcome<Json<AuthorizeUrl>> {
    begin(&config, &kv).await.into()
}

async fn begin(config: &ApiConfig, kv: &Kv) -> Result<Json<AuthorizeUrl>, ApiError> {
    let state = random_token()?;
    expiring::put(kv, &state_key(&state), &(), STATE_TTL_SECONDS).await?;

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
/// Consumes the `state`, exchanges the code, upserts the account, and sends
/// the browser to the SPA with the session token in the URL fragment.
///
/// Deliberately not annotated with `#[skyzen::openapi]`: the macro emits
/// module-level items that mention every argument type, and this handler is
/// generic over [`GithubOauth`], whose parameter does not exist at module
/// scope. The route still appears in the exported document, without its
/// parameter schemas.
pub async fn callback<G: GithubOauth>(
    Query(callback): Query<Callback>,
    State(config): State<ApiConfig>,
    State(github): State<G>,
    kv: Kv,
    db: Db,
) -> Outcome<Response> {
    complete(callback, &config, &github, &kv, &db).await.into()
}

async fn complete<G: GithubOauth>(
    callback: Callback,
    config: &ApiConfig,
    github: &G,
    kv: &Kv,
    db: &Db,
) -> Result<Response, ApiError> {
    if expiring::take::<()>(kv, &state_key(&callback.state))
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
    let user = users::upsert_from_github(db, &account, &sealed).await?;
    let session_token = session::issue(kv, user.id).await?;

    tracing::info!(login = %user.login, "completed a GitHub sign-in");
    Ok(see_other(&completion_url(config, &session_token)))
}

/// Where the browser is sent once it has a token: the SPA's completion route
/// on the callback's own origin, with the token in the fragment.
fn completion_url(config: &ApiConfig, session_token: &str) -> Url {
    let mut url = config
        .redirect_uri()
        .join(POST_LOGIN_PATH)
        .expect("a rooted path always resolves against an absolute redirect URI");
    url.set_fragment(Some(
        &url::form_urlencoded::Serializer::new(String::new())
            .append_pair(TOKEN_PARAM, session_token)
            .finish(),
    ));
    url
}

/// A 303 redirect, which is what turns GitHub's GET into a plain navigation.
fn see_other(location: &Url) -> Response {
    // A parsed `Url` percent-encodes everything a header value forbids, so
    // this conversion cannot fail.
    let value = header::HeaderValue::from_str(location.as_str())
        .expect("a parsed URL is always a valid header value");

    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SEE_OTHER;
    response.headers_mut().insert(header::LOCATION, value);
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
        response.assert_header("content-type", "application/problem+json");
        assert_eq!(
            response.json::<flyco_core::Problem>().kind,
            "https://flyco.dev/problems/unknown-oauth-state"
        );
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
        let location = response
            .headers()
            .get("location")
            .expect("the response redirects")
            .to_str()
            .expect("the location is ASCII");
        let location = Url::parse(location).expect("the location is an absolute URL");

        // Same origin as the callback, so the SPA that reads the fragment is
        // the one flyco serves.
        let callback_origin = Url::parse(REDIRECT_URI)
            .expect("the test redirect URI is absolute")
            .origin();
        assert_eq!(location.origin(), callback_origin);
        assert_eq!(location.path(), "/auth/complete");
        assert!(
            location.query().is_none(),
            "the token must never reach a query string"
        );

        let fragment = location.fragment().expect("the token rides the fragment");
        let token = url::form_urlencoded::parse(fragment.as_bytes())
            .find_map(|(key, value)| (key == "token").then(|| value.into_owned()))
            .expect("the fragment carries `token`");
        assert!(token.starts_with(session::TOKEN_PREFIX));

        let user_id = session::resolve(&kv, &token)
            .await
            .expect("resolve the session")
            .expect("the issued token is live");

        let user = users::find(&db, user_id)
            .await
            .expect("read the user")
            .expect("the callback created the user row");
        assert_eq!(user.login, GITHUB_LOGIN);
    }

    #[skyzen::test]
    async fn the_callback_sets_no_cookie(ctx: TestContext, _kv: Kv, db: Db) {
        let client = ctx.client(migrated_router(&db).await);
        let state = begin(&client).await;

        let response = client
            .get(&format!("/v1/auth/github/callback?code=abc&state={state}"))
            .send()
            .await;

        assert!(
            response.headers().get("set-cookie").is_none(),
            "flyco authenticates with the Authorization header only"
        );
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
            .fetch_scalars()
            .await
            .expect("read the stored token");

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

            let id: String = db
                .query("SELECT id FROM users WHERE github_id = ?")
                .bind(GITHUB_ID)
                .fetch_scalar()
                .await
                .expect("exactly one row per GitHub account");
            ids.push(id);
        }

        assert_eq!(ids[0], ids[1]);
    }
}
