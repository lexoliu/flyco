//! Linking a Devin account by signing in, rather than by pasting a token.
//!
//! Two routes, and between them the user's browser does the only thing it
//! can do here: open Devin's authorize page and bring back the code the
//! page shows. [`start`] mints the PKCE verifier, keeps it in KV for ten
//! minutes under an opaque attempt id, and hands back the URL.
//! [`complete`] redeems the code the user pasted and links the account.
//!
//! The verifier never reaches the browser. That is the whole point of PKCE
//! here: the flow is a public client, so the code alone must not be enough
//! to obtain a token, and the half that completes it stays in the control
//! plane — bound, additionally, to the user who started the attempt, so one
//! signed-in user cannot finish another's sign-in. The same two facts tie
//! a paste to its sign-in: a code copied into the wrong attempt meets the
//! wrong verifier and Devin refuses the exchange.
//!
//! Nothing here is a redirect at all. Devin's allowlist admits only
//! localhost redirect URIs, so flyco runs the CLI's port-free variant of
//! the flow (see [`devin::authorize_url`]): the page shows the code, and
//! the code is the transport.

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
use crate::devin::{self, DevinApi as _};
use crate::error::ApiError;
use crate::expiring;
use crate::harness_accounts::{self, StoredCredential};
use crate::problem::Outcome;
use crate::respond::Created;
use crate::vendors::Vendors;

/// How long the user has to approve the grant and paste the code back.
const ATTEMPT_TTL_SECONDS: u64 = 10 * 60;

/// Prefix every attempt is stored under.
const ATTEMPT_KEY_PREFIX: &str = "auth:devin-oauth:";

/// One sign-in in flight, as KV holds it.
///
/// A secret by construction: the verifier completes the grant, so this
/// value is never returned to anybody.
#[derive(Serialize, Deserialize)]
struct Attempt {
    /// Who started it. A code redeemed by anybody else is not this attempt.
    user: UserId,
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
    let attempt_id = DevinOauthAttemptId::generate();

    expiring::put(
        kv,
        &attempt_key(attempt_id),
        &Attempt {
            user,
            verifier: pkce.verifier,
        },
        ATTEMPT_TTL_SECONDS,
    )
    .await?;

    // The state is the nonce the CLI sends too; the page shows the bare
    // code and never echoes it, so nothing is kept to compare it against.
    let authorize_url = devin::authorize_url(&pkce.challenge, &random_token()?);

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
    State(vendors): State<Vendors>,
    Json(request): Json<CompleteDevinOauth>,
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

    let code = devin::pasted_code(&request.code).ok_or(ApiError::InvalidHarnessCredential(
        "the pasted code must not be empty",
    ))?;

    let token = vendors.devin.redeem_grant(code, &attempt.verifier).await?;

    // Spent: the code has been exchanged, so the attempt has done its job.
    // A concurrent second paste of the same code loses at Devin, not here,
    // which is why nothing is made of the delete finding it gone.
    expiring::take::<Attempt>(kv, &key).await?;

    // The label is Devin's own name for the principal, as it is for a
    // pasted key — and the read doubles as the token's validation: a token
    // Devin will not open never reaches the table.
    let label = vendors
        .devin
        .self_identity(&token)
        .await?
        .account_name()
        .unwrap_or(harness_accounts::UNNAMED_DEVIN_ACCOUNT)
        .to_owned();
    let credential = StoredCredential::ApiKey { key: token };
    harness_accounts::store(
        db,
        config,
        vendors,
        user,
        &label,
        HarnessKind::Devin,
        &credential,
    )
    .await
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
    use crate::error::ApiError;
    use crate::testing::{
        DEVIN_CODE, migrate, seed_other_user, seed_user, test_config, test_vendors,
    };
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
        let started = begin(&kv, user.id).await.expect("a sign-in starts");
        let paste = |code: &str| CompleteDevinOauth {
            attempt_id: started.attempt_id,
            code: code.to_owned(),
        };

        let wrong = redeem(&config, &vendors, &kv, &db, user.id, paste("not-the-code")).await;
        assert!(
            matches!(wrong, Err(ApiError::DevinOauthRejected { .. })),
            "a wrong paste is Devin's refusal, not a spent attempt: {wrong:?}"
        );

        let linked = redeem(&config, &vendors, &kv, &db, user.id, paste(DEVIN_CODE))
            .await
            .expect("the same sign-in redeems the right paste");
        assert_eq!(linked.harness, HarnessKind::Devin);

        let again = redeem(&config, &vendors, &kv, &db, user.id, paste(DEVIN_CODE)).await;
        assert!(
            matches!(again, Err(ApiError::DevinOauthAttemptExpired)),
            "a redeemed sign-in is spent: {again:?}"
        );
    }

    /// One user's sign-in does not redeem under another's session.
    #[skyzen::test]
    async fn an_attempt_belongs_to_the_user_who_started_it(_ctx: TestContext, kv: Kv, db: Db) {
        migrate(&db).await;
        let user = seed_user(&db).await;
        let other = seed_other_user(&db).await;
        let config = test_config();
        let vendors = test_vendors();
        let started = begin(&kv, user.id).await.expect("a sign-in starts");

        let stolen = redeem(
            &config,
            &vendors,
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
