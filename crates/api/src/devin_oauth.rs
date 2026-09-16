//! Linking a Devin account by signing in, rather than by pasting a token.
//!
//! Two routes, and between them the user's browser does the only thing it
//! can do here: open Devin's authorize page and bring back the code the
//! redirect carries. [`start`] mints the PKCE verifier and the `state`,
//! keeps both in KV for ten minutes under an opaque attempt id, and hands
//! back the URL. [`complete`] redeems what the user pasted and links the
//! account.
//!
//! The verifier never reaches the browser. That is the whole point of PKCE
//! here: the flow is a public client, so the code alone must not be enough
//! to obtain a token, and the half that completes it stays in the control
//! plane — bound, additionally, to the user who started the attempt, so one
//! signed-in user cannot finish another's sign-in.
//!
//! Nothing here is a flyco redirect: `redirect_uri` is a dead localhost
//! address (see [`devin::REDIRECT_URI`]), because Devin's allowlist admits
//! nothing else. The redirect is meant to fail to connect, the code stays
//! in the address bar of the page that could not load, and the user pastes
//! it back — the pasted URL is the transport.

use flyco_core::{
    CompleteDevinOauth, CurrentUser, DevinOauthAttemptId, DevinOauthStart, HarnessAccountView,
    HarnessKind, UserId,
};
use serde::{Deserialize, Serialize};
use skyzen::routing::{CreateRouteNode, Route, RouteNode, Routes as _};
use skyzen::utils::{Json, State};
use skyzen_services::{Db, Kv};

use crate::config::ApiConfig;
use crate::crypto::{pkce, random_token};
use crate::devin::{self, DevinApi as _, DevinClient, PastedCode};
use crate::error::ApiError;
use crate::expiring;
use crate::harness_accounts::{self, StoredCredential};
use crate::problem::Outcome;
use crate::respond::Created;

/// How long the user has to approve the grant and paste the code back.
const ATTEMPT_TTL_SECONDS: u64 = 10 * 60;

/// Prefix every attempt is stored under.
const ATTEMPT_KEY_PREFIX: &str = "auth:devin-oauth:";

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
fn attempt_key(id: DevinOauthAttemptId) -> String {
    let rendered = id.to_string();
    let mut key = String::with_capacity(ATTEMPT_KEY_PREFIX.len() + rendered.len());
    key.push_str(ATTEMPT_KEY_PREFIX);
    key.push_str(&rendered);
    key
}

/// `POST /v1/harness-accounts/devin/oauth/start` — begins a Devin sign-in.
#[skyzen::openapi]
pub async fn start(State(user): State<CurrentUser>, kv: Kv) -> Outcome<Json<DevinOauthStart>> {
    begin(&kv, user.id).await.map(Json).into()
}

async fn begin(kv: &Kv, user: UserId) -> Result<DevinOauthStart, ApiError> {
    let pkce = pkce()?;
    let state = random_token()?;
    let attempt_id = DevinOauthAttemptId::generate();

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

    let authorize_url = devin::authorize_url(&pkce.challenge, &state);

    tracing::debug!("issued a Devin authorize URL");
    Ok(DevinOauthStart {
        attempt_id,
        authorize_url: authorize_url.into(),
    })
}

/// `POST /v1/harness-accounts/devin/oauth/complete` — links the account.
#[skyzen::openapi]
pub async fn complete(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(devin): State<DevinClient>,
    Json(request): Json<CompleteDevinOauth>,
    kv: Kv,
    db: Db,
) -> Outcome<Created<Json<HarnessAccountView>>> {
    redeem(&config, &devin, &kv, &db, user.id, request)
        .await
        .map(|view| Created(Json(view)))
        .into()
}

async fn redeem(
    config: &ApiConfig,
    devin: &DevinClient,
    kv: &Kv,
    db: &Db,
    user: UserId,
    request: CompleteDevinOauth,
) -> Result<HarnessAccountView, ApiError> {
    // Read, not yet spent: a paste Devin refuses — a typo, a code from
    // the wrong tab — has to be retryable against the same sign-in, and a
    // control plane that fails after the exchange must not leave the user
    // with a page that can only fail again. The attempt is spent below,
    // once Devin has accepted the code; the code itself is single-use at
    // Devin, so nothing here can be replayed.
    let key = attempt_key(request.attempt_id);
    let attempt = expiring::get::<Attempt>(kv, &key)
        .await?
        .ok_or(ApiError::DevinOauthAttemptExpired)?;
    if attempt.user != user {
        return Err(ApiError::DevinOauthAttemptExpired);
    }

    let (code, pasted_state) = match devin::split_pasted_code(&request.code) {
        PastedCode::Code { code, state } => (code, state),
        // The consent was declined, or Devin refused the grant before any
        // code was issued — the redirect still landed, carrying the
        // refusal instead of a code.
        PastedCode::Refused { error, description } => {
            return Err(ApiError::DevinOauthRejected {
                reason: match description {
                    Some(description) => format!("{error}: {description}"),
                    None => error.into_owned(),
                },
            });
        }
        PastedCode::Empty => {
            return Err(ApiError::InvalidHarnessCredential(
                "the pasted code must not be empty",
            ));
        }
    };
    if let Some(pasted_state) = pasted_state
        && *pasted_state != attempt.state
    {
        return Err(ApiError::DevinOauthStateMismatch);
    }

    let token = devin.redeem_grant(&code, &attempt.verifier).await?;

    // Spent: the code has been exchanged, so the attempt has done its job.
    // A concurrent second paste of the same code loses at Devin, not here,
    // which is why nothing is made of the delete finding it gone.
    expiring::take::<Attempt>(kv, &key).await?;

    // The label is Devin's own name for the principal, as it is for a
    // pasted key — and the read doubles as the token's validation: a token
    // Devin will not open never reaches the table.
    let label = devin
        .self_identity(&token)
        .await?
        .account_name()
        .unwrap_or(harness_accounts::UNNAMED_DEVIN_ACCOUNT)
        .to_owned();
    let credential = StoredCredential::ApiKey { key: token };
    harness_accounts::store(db, config, user, &label, HarnessKind::Devin, &credential).await
}

/// The two authenticated routes of the Devin sign-in.
pub fn routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/harness-accounts/devin/oauth/start".post(start),
        "/v1/harness-accounts/devin/oauth/complete".post(complete),
    ))
    .into_route_nodes()
}

#[cfg(test)]
mod tests {
    use skyzen_services::{Db, Kv};
    use skyzen_test::TestContext;

    use super::{CompleteDevinOauth, begin, redeem};
    use crate::devin::{DevinClient, REDIRECT_URI};
    use crate::error::ApiError;
    use crate::testing::{DEVIN_CODE, TestDevin, migrate, seed_other_user, seed_user, test_config};
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
        let devin = DevinClient::Fake(TestDevin);
        let started = begin(&kv, user.id).await.expect("a sign-in starts");
        let paste = |code: &str| CompleteDevinOauth {
            attempt_id: started.attempt_id,
            code: code.to_owned(),
        };

        let wrong = redeem(&config, &devin, &kv, &db, user.id, paste("not-the-code")).await;
        assert!(
            matches!(wrong, Err(ApiError::DevinOauthRejected { .. })),
            "a wrong paste is Devin's refusal, not a spent attempt: {wrong:?}"
        );

        let linked = redeem(&config, &devin, &kv, &db, user.id, paste(DEVIN_CODE))
            .await
            .expect("the same sign-in redeems the right paste");
        assert_eq!(linked.harness, HarnessKind::Devin);

        let again = redeem(&config, &devin, &kv, &db, user.id, paste(DEVIN_CODE)).await;
        assert!(
            matches!(again, Err(ApiError::DevinOauthAttemptExpired)),
            "a redeemed sign-in is spent: {again:?}"
        );
    }

    /// The pasted redirect URL is read as a code and a `state` checked
    /// against the attempt's — a paste from a different sign-in is a
    /// mismatch, not Devin's refusal.
    #[skyzen::test]
    async fn a_pasted_url_carries_the_code_and_answers_for_its_state(
        _ctx: TestContext,
        kv: Kv,
        db: Db,
    ) {
        migrate(&db).await;
        let user = seed_user(&db).await;
        let config = test_config();
        let devin = DevinClient::Fake(TestDevin);
        let started = begin(&kv, user.id).await.expect("a sign-in starts");
        let state = started
            .authorize_url
            .split("state=")
            .nth(1)
            .expect("the authorize URL carries the state")
            .split('&')
            .next()
            .expect("the state parameter");

        let url = format!("{REDIRECT_URI}?code={DEVIN_CODE}&state=somebody-elses");
        let mismatched = redeem(
            &config,
            &devin,
            &kv,
            &db,
            user.id,
            CompleteDevinOauth {
                attempt_id: started.attempt_id,
                code: url,
            },
        )
        .await;
        assert!(
            matches!(mismatched, Err(ApiError::DevinOauthStateMismatch)),
            "a foreign state is a mismatch: {mismatched:?}"
        );

        let url = format!("{REDIRECT_URI}?code={DEVIN_CODE}&state={state}");
        let linked = redeem(
            &config,
            &devin,
            &kv,
            &db,
            user.id,
            CompleteDevinOauth {
                attempt_id: started.attempt_id,
                code: url,
            },
        )
        .await
        .expect("the attempt's own state redeems");
        assert_eq!(linked.label, crate::testing::DEVIN_ACCOUNT_NAME);
    }

    /// A consent the user declined comes back in the same redirect, as
    /// `error` rather than `code`, and is Devin's refusal rather than a
    /// malformed paste.
    #[skyzen::test]
    async fn a_declined_consent_is_the_refusal_devin_stated(_ctx: TestContext, kv: Kv, db: Db) {
        migrate(&db).await;
        let user = seed_user(&db).await;
        let config = test_config();
        let devin = DevinClient::Fake(TestDevin);
        let started = begin(&kv, user.id).await.expect("a sign-in starts");

        let refused = redeem(
            &config,
            &devin,
            &kv,
            &db,
            user.id,
            CompleteDevinOauth {
                attempt_id: started.attempt_id,
                code: format!("{REDIRECT_URI}?error=access_denied"),
            },
        )
        .await;
        assert!(
            matches!(refused, Err(ApiError::DevinOauthRejected { .. })),
            "a declined consent is a refusal, not an empty paste: {refused:?}"
        );
    }

    /// One user's sign-in does not redeem under another's session.
    #[skyzen::test]
    async fn an_attempt_belongs_to_the_user_who_started_it(_ctx: TestContext, kv: Kv, db: Db) {
        migrate(&db).await;
        let user = seed_user(&db).await;
        let other = seed_other_user(&db).await;
        let config = test_config();
        let devin = DevinClient::Fake(TestDevin);
        let started = begin(&kv, user.id).await.expect("a sign-in starts");

        let stolen = redeem(
            &config,
            &devin,
            &kv,
            &db,
            other.id,
            CompleteDevinOauth {
                attempt_id: started.attempt_id,
                code: DEVIN_CODE.to_owned(),
            },
        )
        .await;
        assert!(
            matches!(stolen, Err(ApiError::DevinOauthAttemptExpired)),
            "another user's redeem is indistinguishable from an expired one: {stolen:?}"
        );
    }
}
