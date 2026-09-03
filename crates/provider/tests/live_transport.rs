//! The production transport against a real server.
//!
//! A recorded-exchange transport can only show what a driver *would* do;
//! these show what the wire actually delivers. The one that matters most
//! is the refusal: a 4xx must arrive as a response with its status and
//! body, because every driver reads provider refusals that way — `OpenAI`'s
//! "still pending" is a 403, Azure's "wrong secret" a 401 — and a
//! transport that turned them into errors hid all of them behind a 502.
#![allow(missing_docs)]

use flyco_provider::http::{HttpRequest, HttpTransport, LiveTransport, Method};

const USER_AGENT: &str = "flyco-live-transport-test";

fn get(url: &str) -> HttpRequest {
    HttpRequest::new(Method::Get, url).header("user-agent", USER_AGENT)
}

#[tokio::test]
async fn a_refusal_is_a_response_with_its_status_and_body() {
    let response = LiveTransport::new()
        .send(get("https://httpbingo.org/status/403"))
        .await
        .expect("a 403 is an answer, not a transport failure");
    assert_eq!(response.status, 403);
    assert!(!response.is_success());
}

#[tokio::test]
async fn an_error_body_arrives_intact() {
    let response = LiveTransport::new()
        .send(get(
            "https://httpbingo.org/base64/eyJlcnJvciI6eyJjb2RlIjoiZGV2aWNlYXV0aF9hdXRob3JpemF0aW9uX3BlbmRpbmcifX0=",
        ))
        .await
        .expect("a 200 with a body");
    assert_eq!(response.status, 200);
    let body: serde_json::Value = response.json().expect("JSON body");
    assert_eq!(body["error"]["code"], "deviceauth_authorization_pending");
}

#[tokio::test]
async fn a_success_carries_lowercased_headers_and_the_body() {
    let response = LiveTransport::new()
        .send(get("https://httpbingo.org/json"))
        .await
        .expect("a 200");
    assert_eq!(response.status, 200);
    assert!(response.header_value("content-type").is_some());
    let body: serde_json::Value = response.json().expect("JSON body");
    assert!(body.is_object());
}
