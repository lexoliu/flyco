//! The Microsoft sign-in against the real `login.microsoftonline.com`.
//!
//! Every other test of this flow stands in for Microsoft. This one does not,
//! and it is deliberately the *only* thing about the flow that can be
//! checked without a human at a consent screen: that this deployment's
//! registered client is one Microsoft accepts. A code exchange with a code
//! that was never issued is refused either way — but Microsoft says
//! `invalid_client` when it does not recognise the application or its
//! secret, and something about the *code* when it does. The second answer is
//! the one that proves the two configured values are right.
//!
//! Ignored by default because it needs the network and the client
//! credentials; run it with
//! `FLYCO_AZURE_OAUTH_CLIENT_ID=… FLYCO_AZURE_OAUTH_CLIENT_SECRET=…
//! FLYCO_REDIRECT_URI=… cargo nextest run -p flyco-api --run-ignored
//! ignored-only -E 'test(live_)'`.
#![allow(missing_docs)]

use flyco_api::microsoft::{MicrosoftError, OauthClient, sign_in_over};
use flyco_api::provider_oauth::AZURE_CALLBACK_PATH;
use flyco_provider::http::LiveTransport;

const CLIENT_ID_VAR: &str = "FLYCO_AZURE_OAUTH_CLIENT_ID";
const CLIENT_SECRET_VAR: &str = "FLYCO_AZURE_OAUTH_CLIENT_SECRET";
const REDIRECT_URI_VAR: &str = "FLYCO_REDIRECT_URI";

/// The refusals that mean "flyco's own application is wrong", rather than
/// "that code is no good" — which is the whole point of the assertion.
const CLIENT_REFUSALS: [&str; 2] = ["invalid_client", "unauthorized_client"];

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required by this live test"))
}

#[skyzen::test]
#[ignore = "talks to login.microsoftonline.com; needs the Microsoft client credentials"]
async fn live_microsoft_recognises_this_deployments_client() {
    let client_id = required(CLIENT_ID_VAR);
    let client_secret = required(CLIENT_SECRET_VAR);
    let redirect_uri = url::Url::parse(&required(REDIRECT_URI_VAR))
        .expect("the configured redirect URI is absolute")
        .join(AZURE_CALLBACK_PATH)
        .expect("a rooted path resolves against it");

    let error = sign_in_over(
        &LiveTransport::new(),
        OauthClient {
            id: &client_id,
            secret: &client_secret,
        },
        "a-code-microsoft-never-issued",
        redirect_uri.as_str(),
    )
    .await
    .expect_err("a code that was never issued cannot be redeemed");

    let MicrosoftError::Rejected { code, description } = error else {
        panic!("Microsoft answered with something other than a refusal: {error}");
    };
    assert!(
        !CLIENT_REFUSALS.contains(&code.as_str()),
        "Microsoft does not accept this deployment's client: {code}: {description}"
    );
}
