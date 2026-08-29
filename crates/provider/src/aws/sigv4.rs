//! Signature Version 4, over the same [`HttpRequest`] every other driver
//! builds.
//!
//! AWS authenticates a request by having the caller reproduce a *canonical
//! request* — method, path, sorted query, sorted lowercased headers, a hash
//! of the body — and sign a string derived from it with a key derived in
//! turn from the date, the region and the service. Getting any of that a
//! byte wrong yields `SignatureDoesNotMatch` and nothing else, so the
//! signature comes from AWS's own [`aws_sigv4`] rather than from a
//! reimplementation here.
//!
//! # `host` is signed but not sent
//!
//! The canonical request always covers the `Host` header, so it is handed to
//! the signer. It is deliberately *not* added to the outgoing request:
//! `Host` is a forbidden header for the Worker's `fetch`, and it does not
//! need to be set, because the value the service receives is the URL's own
//! authority — exactly what was signed.
//!
//! # Why the instant is a parameter
//!
//! A signature is only valid within five minutes of the `X-Amz-Date` it
//! carries, so signing needs civil time — the one thing the crate's
//! monotonic clock cannot supply. It arrives as a [`WallClock`] rather than
//! being read from the host, which is also what makes a signature something
//! a test can assert byte for byte.

use core::fmt;
use core::time::Duration;
use std::time::UNIX_EPOCH;

use aws_credential_types::Credentials as SigningIdentity;
use aws_sigv4::http_request::{
    SignableBody, SignableRequest, SigningSettings, sign as sign_request,
};
use aws_sigv4::sign::v4;

use crate::ProviderError;
use crate::http::HttpRequest;

/// An AWS access key, and the session token that may come with it.
///
/// The secret is a credential; the hand-written [`fmt::Debug`] is what keeps
/// it out of a provisioning log.
#[derive(Clone, PartialEq, Eq)]
pub struct AccessKey {
    /// Access key id (`AKIA…` for a long-lived key, `ASIA…` for a session).
    pub access_key_id: String,
    /// The secret half.
    pub secret_access_key: String,
    /// Session token, for temporary credentials.
    pub session_token: Option<String>,
}

impl fmt::Debug for AccessKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessKey")
            .field("access_key_id", &self.access_key_id)
            .field("has_session_token", &self.session_token.is_some())
            .finish_non_exhaustive()
    }
}

impl AccessKey {
    /// A long-lived IAM access key.
    #[must_use]
    pub fn new(access_key_id: impl Into<String>, secret_access_key: impl Into<String>) -> Self {
        Self {
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
            session_token: None,
        }
    }

    /// The same key with a session token attached.
    #[must_use]
    pub fn with_session_token(mut self, token: impl Into<String>) -> Self {
        self.session_token = Some(token.into());
        self
    }
}

/// Where a request is being sent, in the two terms a signature is scoped by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scope<'a> {
    /// Signing region, e.g. `us-east-1`. Global services still name one.
    pub region: &'a str,
    /// Service signing name, e.g. `ec2`, `sts`, `pricing`.
    pub service: &'a str,
}

/// The `Host` header value a URL implies.
///
/// The authority, port included when the URL states one — the signer needs
/// exactly the string the service will see.
fn host_of(url: &str) -> Result<String, ProviderError> {
    url::Url::parse(url)
        .ok()
        .and_then(|parsed| {
            let host = parsed.host_str()?.to_owned();
            Some(match parsed.port() {
                Some(port) => format!("{host}:{port}"),
                None => host,
            })
        })
        .ok_or(ProviderError::Malformed(
            "an AWS request was built with a URL that names no host",
        ))
}

/// Signs one request, returning it with the headers AWS requires attached.
///
/// # Errors
///
/// Returns [`ProviderError::Malformed`] if the request cannot be signed,
/// which means it was built wrong rather than refused.
pub fn sign(
    request: HttpRequest,
    key: &AccessKey,
    scope: Scope<'_>,
    now_unix: u64,
) -> Result<HttpRequest, ProviderError> {
    let identity = SigningIdentity::new(
        key.access_key_id.clone(),
        key.secret_access_key.clone(),
        key.session_token.clone(),
        None,
        "flyco",
    )
    .into();

    let params: aws_sigv4::http_request::SigningParams<'_> = v4::SigningParams::builder()
        .identity(&identity)
        .region(scope.region)
        .name(scope.service)
        .time(UNIX_EPOCH + Duration::from_secs(now_unix))
        .settings(SigningSettings::default())
        .build()
        .map_err(|_| ProviderError::Malformed("an AWS signature could not be parameterized"))?
        .into();

    let host = host_of(&request.url)?;
    let mut headers: Vec<(&str, &str)> = vec![("host", host.as_str())];
    headers.extend(
        request
            .headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str())),
    );

    let signable = SignableRequest::new(
        request.method.as_str(),
        request.url.clone(),
        headers.into_iter(),
        SignableBody::Bytes(&request.body),
    )
    .map_err(|_| ProviderError::Malformed("an AWS request could not be made signable"))?;

    let (instructions, _signature) = sign_request(signable, &params)
        .map_err(|_| ProviderError::Malformed("an AWS request could not be signed"))?
        .into_parts();

    let (signed_headers, signed_query) = instructions.into_parts();
    if !signed_query.is_empty() {
        // Every flyco call signs with headers; a query-parameter signature is
        // for presigned URLs, which nothing here mints.
        return Err(ProviderError::Malformed(
            "an AWS signature landed in the query string, which flyco never presigns",
        ));
    }

    Ok(signed_headers.into_iter().fold(request, |request, header| {
        request.header(header.name().to_ascii_lowercase(), header.value())
    }))
}

/// Signs a request with the instant a [`WallClock`](crate::clock::WallClock)
/// reads.
///
/// # Errors
///
/// Returns [`ProviderError`] for the same reasons [`sign`] does.
pub fn sign_at<C: crate::clock::WallClock>(
    request: HttpRequest,
    key: &AccessKey,
    scope: Scope<'_>,
    clock: &C,
) -> Result<HttpRequest, ProviderError> {
    sign(request, key, scope, clock.unix_seconds())
}

#[cfg(test)]
mod tests {
    use super::{AccessKey, Scope, host_of, sign};
    use crate::http::{HttpRequest, Method};

    /// 2026-08-29T12:00:00Z.
    const SIGNED_AT: u64 = 1_788_004_800;

    fn key() -> AccessKey {
        AccessKey::new(
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        )
    }

    fn call(action: &str) -> HttpRequest {
        let request = HttpRequest::new(Method::Post, "https://ec2.us-west-2.amazonaws.com/")
            .form_body(&[("Action", action), ("Version", "2016-11-15")]);
        sign(
            request,
            &key(),
            Scope {
                region: "us-west-2",
                service: "ec2",
            },
            SIGNED_AT,
        )
        .expect("sign")
    }

    fn signed() -> HttpRequest {
        call("DescribeRegions")
    }

    fn header(request: &HttpRequest, name: &str) -> String {
        request
            .headers
            .iter()
            .find(|(header, _)| header == name)
            .map_or_else(
                || panic!("the signed request carries `{name}`"),
                |(_, value)| value.clone(),
            )
    }

    #[test]
    fn the_host_a_url_implies_is_what_gets_signed() {
        assert_eq!(
            host_of("https://ec2.eu-west-1.amazonaws.com/").expect("a host"),
            "ec2.eu-west-1.amazonaws.com"
        );
        host_of("not a url").expect_err("a request must name a host");
    }

    #[test]
    fn a_signature_names_the_date_region_service_and_the_headers_it_covers() {
        let request = signed();

        assert_eq!(header(&request, "x-amz-date"), "20260829T120000Z");

        let authorization = header(&request, "authorization");
        assert!(authorization.starts_with("AWS4-HMAC-SHA256 "));
        assert!(
            authorization
                .contains("Credential=AKIAIOSFODNN7EXAMPLE/20260829/us-west-2/ec2/aws4_request")
        );
        // `host` is in the canonical request even though it is never sent:
        // the value the service sees is the URL's own authority.
        assert!(
            authorization.contains("SignedHeaders=content-type;host;x-amz-date"),
            "the signed header list is wrong: {authorization}"
        );
        assert!(authorization.contains("Signature="));
    }

    #[test]
    fn the_signature_is_deterministic_for_one_instant_and_moves_with_the_body() {
        let first = header(&signed(), "authorization");
        assert_eq!(first, header(&signed(), "authorization"));

        assert_ne!(
            first,
            header(&call("DescribeInstances"), "authorization"),
            "the body is part of the canonical request, so a different body signs differently"
        );
    }

    #[test]
    fn a_session_token_is_signed_and_presented() {
        let request = sign(
            HttpRequest::new(Method::Post, "https://sts.amazonaws.com/"),
            &key().with_session_token("FQoGZXIvYXdzEExample"),
            Scope {
                region: "us-east-1",
                service: "sts",
            },
            SIGNED_AT,
        )
        .expect("sign");

        assert_eq!(
            header(&request, "x-amz-security-token"),
            "FQoGZXIvYXdzEExample"
        );
        assert!(header(&request, "authorization").contains("x-amz-security-token"));
    }

    #[test]
    fn an_access_key_never_debug_prints_its_secret() {
        assert!(!format!("{:?}", key()).contains("wJalrXUtnFEMI"));
    }
}
