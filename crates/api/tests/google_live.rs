//! The Google sign-in against the real `oauth2.googleapis.com`.
//!
//! The Google half of what `microsoft_live.rs` checks, and for the same
//! reason: without a human at a consent screen the only thing that can be
//! established is that Google recognises this deployment's client. It says
//! `invalid_client` when it does not, and `invalid_grant` — about the code —
//! when it does.
//!
//! Ignored by default because it needs the network and the client
//! credentials; run it with
//! `FLYCO_GOOGLE_OAUTH_CLIENT_ID=… FLYCO_GOOGLE_OAUTH_CLIENT_SECRET=…
//! FLYCO_REDIRECT_URI=… cargo nextest run -p flyco-api --run-ignored
//! ignored-only -E 'test(live_)'`.
#![allow(missing_docs)]

use flyco_api::google::{GoogleError, OauthClient, sign_in_over};
use flyco_api::provider_oauth::GCP_CALLBACK_PATH;
use flyco_provider::http::LiveTransport;

const CLIENT_ID_VAR: &str = "FLYCO_GOOGLE_OAUTH_CLIENT_ID";
const CLIENT_SECRET_VAR: &str = "FLYCO_GOOGLE_OAUTH_CLIENT_SECRET";
const REDIRECT_URI_VAR: &str = "FLYCO_REDIRECT_URI";

/// The refusal that means "flyco's own client is wrong" rather than "that
/// code is no good".
const CLIENT_REFUSAL: &str = "invalid_client";

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required by this live test"))
}

#[skyzen::test]
#[ignore = "talks to oauth2.googleapis.com; needs the Google client credentials"]
async fn live_google_recognises_this_deployments_client() {
    let client_id = required(CLIENT_ID_VAR);
    let client_secret = required(CLIENT_SECRET_VAR);
    let redirect_uri = url::Url::parse(&required(REDIRECT_URI_VAR))
        .expect("the configured redirect URI is absolute")
        .join(GCP_CALLBACK_PATH)
        .expect("a rooted path resolves against it");

    let error = sign_in_over(
        &LiveTransport::new(),
        OauthClient {
            id: &client_id,
            secret: &client_secret,
        },
        "a-code-google-never-issued",
        redirect_uri.as_str(),
    )
    .await
    .expect_err("a code that was never issued cannot be redeemed");

    let GoogleError::Rejected { status, message } = error else {
        panic!("Google answered with something other than a refusal: {error}");
    };
    assert_ne!(
        status, CLIENT_REFUSAL,
        "Google does not accept this deployment's client: {message}"
    );
}
