//! The Codex device sign-in against the real `auth.openai.com`.
//!
//! Every other test of this flow stands in for `OpenAI`. This one does not:
//! it starts a real device authorization with flyco's public client id and
//! polls it once, which is exactly what the browser does every few seconds
//! while the user is still on `OpenAI`'s page — and exactly what answered
//! `502 Bad Gateway` on dev.flyco.dev, because `OpenAI` says "pending" with
//! an HTTP 403 and the transport turned that into a failure. Nobody
//! approves the code, so the only correct answer is `Pending`.
//!
//! Ignored by default because it needs the network and the client id;
//! run it with `FLYCO_CODEX_OAUTH_CLIENT_ID=… cargo nextest run -p flyco-api
//! --run-ignored ignored-only -E 'test(live_)'`.
#![allow(missing_docs)]

use flyco_api::openai::{DevicePoll, poll_device_code_over, request_user_code_over};
use flyco_provider::http::LiveTransport;

const CLIENT_ID_VAR: &str = "FLYCO_CODEX_OAUTH_CLIENT_ID";

#[skyzen::test]
#[ignore = "talks to auth.openai.com; needs FLYCO_CODEX_OAUTH_CLIENT_ID"]
async fn live_device_poll_before_approval_is_pending() {
    let client_id = std::env::var(CLIENT_ID_VAR)
        .unwrap_or_else(|_| panic!("{CLIENT_ID_VAR} names flyco's public Codex client id"));
    let transport = LiveTransport::new();

    let auth = request_user_code_over(&transport, &client_id)
        .await
        .expect("OpenAI issues a device authorization");
    assert!(!auth.user_code.is_empty());

    let poll = poll_device_code_over(&transport, &auth.device_auth_id, &auth.user_code)
        .await
        .expect("an unapproved code is pending, not a failure");
    assert!(matches!(poll, DevicePoll::Pending));
}
