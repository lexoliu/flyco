//! Linking Claude Code by signing in, rather than by pasting a token.
//!
//! Two routes, and between them the user's browser does the only thing it
//! can do here: open Anthropic's authorize page and bring back the code it
//! shows. [`start`] mints the PKCE verifier and the `state`, keeps both in
//! KV for ten minutes under an opaque attempt id, and hands back the URL.
//! [`complete`] redeems what the user pasted and links the account.
//!
//! The verifier never reaches the browser. That is the whole point of PKCE
//! here: the flow is a public client, so the code alone must not be enough
//! to obtain a token, and the half that completes it stays in the control
//! plane — bound, additionally, to the user who started the attempt, so one
//! signed-in user cannot finish another's sign-in.
//!
//! Nothing here is a flyco redirect: `redirect_uri` is Anthropic's own
//! console page (see [`anthropic::REDIRECT_URI`]), which is what lets a
//! Worker with no callback route run the flow the Claude CLI runs.

use flyco_core::{
    ClaudeOauthAttemptId, ClaudeOauthStart, CompleteClaudeOauth, CurrentUser, HarnessAccountView,
    HarnessKind, UserId,
};
use serde::{Deserialize, Serialize};
use skyzen::routing::{CreateRouteNode, Route, RouteNode, Routes as _};
use skyzen::utils::{Json, State};
use skyzen_services::{Db, Kv};

use crate::anthropic::{self, ClaudeOauth as _, TokenRequest};
use crate::clock::now_unix;
use crate::config::ApiConfig;
use crate::crypto::{pkce, random_token};
use crate::error::ApiError;
use crate::expiring;
use crate::harness_accounts::{self, StoredCredential};
use crate::problem::Outcome;
use crate::respond::Created;
use crate::vendors::Vendors;

/// How long the user has to approve the grant and paste the code back.
const ATTEMPT_TTL_SECONDS: u64 = 10 * 60;

/// What a linked account is called when Anthropic names no address.
///
/// The label exists to tell two accounts apart, and the vendor's own answer
/// is the only honest one — so when there is none, the card says what the
/// account is rather than inventing an identity for it.
const UNNAMED_ACCOUNT: &str = "Claude subscription";

/// Prefix every attempt is stored under.
const ATTEMPT_KEY_PREFIX: &str = "auth:claude-oauth:";

/// One sign-in in flight, as KV holds it.
///
/// Secrets by construction: the verifier completes the grant and the state
/// authenticates the paste, so this value is never returned to anybody.
#[derive(Serialize, Deserialize)]
struct Attempt {
    /// Who started it. A code redeemed by anybody else is not this attempt.
    user: UserId,
    /// The `state` published in the authorize URL.
    state: String,
    /// The PKCE verifier whose challenge was published.
    verifier: String,
}

impl core::fmt::Debug for Attempt {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Attempt")
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

/// Where one attempt lives in the key-value store.
fn attempt_key(id: ClaudeOauthAttemptId) -> String {
    let rendered = id.to_string();
    let mut key = String::with_capacity(ATTEMPT_KEY_PREFIX.len() + rendered.len());
    key.push_str(ATTEMPT_KEY_PREFIX);
    key.push_str(&rendered);
    key
}

/// `POST /v1/harness-accounts/claude/oauth/start` — begins a Claude sign-in.
#[skyzen::openapi]
pub async fn start(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    kv: Kv,
) -> Outcome<Json<ClaudeOauthStart>> {
    begin(&config, &kv, user.id).await.map(Json).into()
}

async fn begin(config: &ApiConfig, kv: &Kv, user: UserId) -> Result<ClaudeOauthStart, ApiError> {
    let pkce = pkce()?;
    let state = random_token()?;
    let attempt_id = ClaudeOauthAttemptId::generate();

    expiring::put(
        kv,
        &attempt_key(attempt_id),
        &Attempt {
            user,
            state: state.clone(),
            verifier: pkce.verifier,
        },
        ATTEMPT_TTL_SECONDS,
    )
    .await?;

    let authorize_url =
        anthropic::authorize_url(config.claude_oauth_client_id(), &pkce.challenge, &state);

    tracing::debug!("issued a Claude authorize URL");
    Ok(ClaudeOauthStart {
        attempt_id,
        authorize_url: authorize_url.into(),
    })
}

/// `POST /v1/harness-accounts/claude/oauth/complete` — links the account.
#[skyzen::openapi]
pub async fn complete(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(vendors): State<Vendors>,
    Json(request): Json<CompleteClaudeOauth>,
    kv: Kv,
    db: Db,
) -> Outcome<Created<Json<HarnessAccountView>>> {
    redeem(&config, &vendors, &kv, &db, user.id, request)
        .await
        .map(|view| Created(Json(view)))
        .into()
}

async fn redeem(
    config: &ApiConfig,
    vendors: &Vendors,
    kv: &Kv,
    db: &Db,
    user: UserId,
    request: CompleteClaudeOauth,
) -> Result<HarnessAccountView, ApiError> {
    // Read, not yet spent: a paste Anthropic refuses — a typo, a code from
    // the wrong tab — has to be retryable against the same sign-in, and a
    // control plane that fails after the exchange must not leave the user
    // with a page that can only fail again. The attempt is spent below,
    // once Anthropic has accepted the code; the code itself is single-use
    // at Anthropic, so nothing here can be replayed.
    let key = attempt_key(request.attempt_id);
    let attempt = expiring::get::<Attempt>(kv, &key)
        .await?
        .ok_or(ApiError::ClaudeOauthAttemptExpired)?;
    if attempt.user != user {
        return Err(ApiError::ClaudeOauthAttemptExpired);
    }

    let (code, pasted_state) = anthropic::split_pasted_code(&request.code);
    if code.is_empty() {
        return Err(ApiError::InvalidHarnessCredential(
            "the pasted code must not be empty",
        ));
    }
    if let Some(pasted_state) = pasted_state
        && pasted_state != attempt.state
    {
        return Err(ApiError::ClaudeOauthStateMismatch);
    }

    let tokens = vendors
        .claude
        .exchange(TokenRequest::AuthorizationCode {
            code,
            state: &attempt.state,
            client_id: config.claude_oauth_client_id(),
            redirect_uri: anthropic::REDIRECT_URI,
            code_verifier: &attempt.verifier,
        })
        .await?;

    // Spent: the code has been exchanged, so the attempt has done its job.
    // A concurrent second paste of the same code loses at Anthropic, not
    // here, which is why nothing is made of the delete finding it gone.
    expiring::take::<Attempt>(kv, &key).await?;

    let credential = StoredCredential::from_tokens(&tokens, now_unix());
    let label = tokens.email_address().unwrap_or(UNNAMED_ACCOUNT).to_owned();
    harness_accounts::store(
        db,
        config,
        vendors,
        user,
        &label,
        HarnessKind::ClaudeCode,
        &credential,
    )
    .await
}

/// The two authenticated routes of the Claude sign-in.
pub fn routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/harness-accounts/claude/oauth/start".post(start),
        "/v1/harness-accounts/claude/oauth/complete".post(complete),
    ))
    .into_route_nodes()
}

#[cfg(test)]
mod tests {
    use skyzen_services::{Db, Kv};
    use skyzen_test::TestContext;

    use super::{CompleteClaudeOauth, begin, redeem};
    use crate::error::ApiError;
    use crate::testing::{CLAUDE_CODE, migrate, seed_user, test_config, test_vendors};
    use flyco_core::HarnessKind;

    /// A refused paste leaves the sign-in standing, and only a redeemed
    /// code spends it: the typo case and the after-a-failure case are the
    /// same page, and both need a second paste to be possible.
    #[skyzen::test]
    async fn the_attempt_is_spent_by_the_exchange_and_not_before(
        _ctx: TestContext,
        kv: Kv,
        db: Db,
    ) {
        migrate(&db).await;
        let user = seed_user(&db).await;
        let config = test_config();
        let vendors = test_vendors();
        let started = begin(&config, &kv, user.id)
            .await
            .expect("a sign-in starts");
        let paste = |code: &str| CompleteClaudeOauth {
            attempt_id: started.attempt_id,
            code: code.to_owned(),
        };

        let wrong = redeem(&config, &vendors, &kv, &db, user.id, paste("not-the-code")).await;
        assert!(
            matches!(wrong, Err(ApiError::ClaudeOauthRejected { .. })),
            "a wrong paste is Anthropic's refusal, not a spent attempt: {wrong:?}"
        );

        let linked = redeem(&config, &vendors, &kv, &db, user.id, paste(CLAUDE_CODE))
            .await
            .expect("the same sign-in redeems the right paste");
        assert_eq!(linked.harness, HarnessKind::ClaudeCode);

        let again = redeem(&config, &vendors, &kv, &db, user.id, paste(CLAUDE_CODE)).await;
        assert!(
            matches!(again, Err(ApiError::ClaudeOauthAttemptExpired)),
            "a redeemed sign-in is spent: {again:?}"
        );
    }
}
