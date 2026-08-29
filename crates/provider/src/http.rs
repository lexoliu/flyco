//! The one HTTP exchange every provider driver is built out of.
//!
//! Drivers do not reach for [`zenwave`] directly. They describe a request as
//! data and hand it to an [`HttpTransport`], which is [`ZenwaveTransport`] in
//! production — Fetch-backed inside the Cloudflare Worker, hyper-backed
//! natively — and a table of recorded exchanges under test. That is the only
//! way to pin what a driver actually puts on the wire: the exact URL, the
//! exact `api-version`, the exact JSON body. A driver that called
//! `zenwave::client()` itself could only be tested against a live cloud
//! account, which is to say not tested at all.
//!
//! The transport is deliberately dumb. It does not retry, does not follow the
//! provider's asynchronous-operation protocol, and does not interpret a
//! status code: all of that is provider semantics and lives in the driver.

use core::fmt;
use core::future::Future;

use zenwave::{Client as _, ResponseExt as _};

/// The HTTP methods flyco's providers use.
///
/// A closed set rather than a string, so a typo is a compile error and a
/// recorded fixture can be matched by equality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    /// `GET`.
    Get,
    /// `POST`.
    Post,
    /// `PUT`.
    Put,
    /// `PATCH`.
    Patch,
    /// `DELETE`.
    Delete,
}

impl Method {
    /// The method token as it appears on the request line.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }

    const fn zenwave(self) -> zenwave::Method {
        match self {
            Self::Get => zenwave::Method::GET,
            Self::Post => zenwave::Method::POST,
            Self::Put => zenwave::Method::PUT,
            Self::Patch => zenwave::Method::PATCH,
            Self::Delete => zenwave::Method::DELETE,
        }
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One request a driver wants made.
///
/// Headers are an ordered list rather than a map because a fixture asserts
/// them in the order the driver set them, which is one more thing a
/// refactor cannot change unnoticed.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// Verb.
    pub method: Method,
    /// Absolute URL, query string included.
    pub url: String,
    /// Headers, in the order the driver added them.
    pub headers: Vec<(String, String)>,
    /// Body, empty for a request that carries none.
    pub body: Vec<u8>,
}

/// A request renders without its `Authorization` header or its body.
///
/// Both carry credentials — a bearer token in the header, a client secret or
/// a daemon token in the body — and a driver's request is exactly the kind
/// of value that ends up in a log line or an error message.
impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("body_bytes", &self.body.len())
            .finish_non_exhaustive()
    }
}

impl HttpRequest {
    /// A request with no headers and no body.
    #[must_use]
    pub fn new(method: Method, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// Adds one header.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Attaches an `Authorization: Bearer` header.
    ///
    /// Two of the three drivers authenticate exactly this way, and the
    /// header's name is lowercased here so a fixture matches it by equality
    /// rather than by a case-insensitive search.
    #[must_use]
    pub fn bearer(self, token: &str) -> Self {
        let mut value = String::with_capacity(7 + token.len());
        value.push_str("Bearer ");
        value.push_str(token);
        self.header("authorization", value)
    }

    /// Attaches a body and the `Content-Type` that describes it.
    ///
    /// The media type is a parameter rather than a constant per body kind
    /// because for some services it *is* the protocol selector: AWS's
    /// JSON-RPC services refuse `application/json` and want
    /// `application/x-amz-json-1.1`, whose version chooses how the request
    /// is read.
    #[must_use]
    pub fn body(self, media_type: &str, body: Vec<u8>) -> Self {
        let mut request = self.header("content-type", media_type);
        request.body = body;
        request
    }

    /// Attaches a JSON body and the `Content-Type` that describes it.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError::Encoding`] if the value does not serialize.
    pub fn json_body<T: serde::Serialize>(self, value: &T) -> Result<Self, HttpError> {
        Ok(self.body("application/json", encode_json(value)?))
    }

    /// Attaches a `application/x-www-form-urlencoded` body.
    #[must_use]
    pub fn form_body(self, fields: &[(&str, &str)]) -> Self {
        let mut encoder = url::form_urlencoded::Serializer::new(String::new());
        for (name, value) in fields {
            encoder.append_pair(name, value);
        }
        self.body(
            "application/x-www-form-urlencoded",
            encoder.finish().into_bytes(),
        )
    }

    /// The body as UTF-8 text, for a fixture assertion or a decode.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError::Decoding`] if the body is not UTF-8.
    pub fn body_text(&self) -> Result<&str, HttpError> {
        core::str::from_utf8(&self.body)
            .map_err(|_| HttpError::Decoding("request body is not UTF-8"))
    }
}

/// A value as JSON bytes.
///
/// # Errors
///
/// Returns [`HttpError::Encoding`] if the value does not serialize.
pub fn encode_json<T: serde::Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, HttpError> {
    serde_json::to_vec(value).map_err(|error| HttpError::Encoding(error.to_string()))
}

/// What a provider answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// Status code.
    pub status: u16,
    /// Response headers, lowercased by the transport so a lookup is exact.
    pub headers: Vec<(String, String)>,
    /// Response body.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// A response with a body and no headers, which is what most fixtures
    /// are.
    #[must_use]
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    /// Adds one header, lowercasing its name.
    #[must_use]
    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_ascii_lowercase(), value.into()));
        self
    }

    /// One header's value, matched case-insensitively.
    #[must_use]
    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Whether the status is 2xx.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        self.status >= 200 && self.status < 300
    }

    /// Decodes the body as JSON.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError::Decoding`] if the body is not the expected JSON.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, HttpError> {
        serde_json::from_slice(&self.body)
            .map_err(|_| HttpError::Decoding("response body is not the expected JSON document"))
    }

    /// The body as text, lossily, for an error message.
    #[must_use]
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Why an exchange did not happen.
///
/// A non-2xx *response* is not one of these: it is a perfectly good exchange
/// whose meaning belongs to the provider.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    /// The request could not be built or sent.
    #[error("HTTP request failed: {0}")]
    Transport(String),
    /// A request body could not be serialized.
    #[error("could not encode a request body: {0}")]
    Encoding(String),
    /// A response could not be understood.
    #[error("could not decode a response: {0}")]
    Decoding(&'static str),
}

/// Somewhere to send an [`HttpRequest`].
///
/// Not object-safe, and deliberately so: every driver is generic over its
/// transport, which keeps the future unboxed on wasm32 and lets a test
/// substitute a table of recorded exchanges without a trait object.
pub trait HttpTransport {
    /// Performs one exchange.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError`] only when no response was obtained; a provider
    /// error arrives as a response with a status code.
    fn send(&self, request: HttpRequest) -> impl Future<Output = Result<HttpResponse, HttpError>>;
}

/// The production transport: zenwave, which is Fetch on the Worker and
/// hyper natively.
#[derive(Debug, Clone, Copy, Default)]
pub struct ZenwaveTransport;

impl ZenwaveTransport {
    /// Creates the transport.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

fn transport(error: impl fmt::Display) -> HttpError {
    HttpError::Transport(error.to_string())
}

impl HttpTransport for ZenwaveTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        let mut client = zenwave::client();
        let mut builder = client
            .method(request.method.zenwave(), request.url.as_str())
            .map_err(transport)?;
        for (name, value) in &request.headers {
            builder = builder
                .header(name.as_str(), value.as_str())
                .map_err(transport)?;
        }

        let response = builder.bytes_body(request.body).await.map_err(transport)?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_ascii_lowercase(), value.to_owned()))
            })
            .collect();

        let body = response.into_bytes().await.map_err(transport)?;
        Ok(HttpResponse {
            status,
            headers,
            body: body.to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{HttpRequest, HttpResponse, Method};

    #[test]
    fn a_request_never_debug_prints_its_credentials() {
        let request = HttpRequest::new(Method::Post, "https://login.microsoftonline.com/t/token")
            .header("authorization", "Bearer super-secret")
            .form_body(&[("client_secret", "also-secret")]);

        let rendered = format!("{request:?}");
        assert!(!rendered.contains("super-secret"));
        assert!(!rendered.contains("also-secret"));
        assert!(rendered.contains("login.microsoftonline.com"));
    }

    #[test]
    fn a_form_body_is_percent_encoded_in_field_order() {
        let request = HttpRequest::new(Method::Post, "https://flyco.dev/").form_body(&[
            ("grant_type", "client_credentials"),
            ("scope", "a/.default"),
        ]);

        assert_eq!(
            request.body_text().expect("UTF-8"),
            "grant_type=client_credentials&scope=a%2F.default"
        );
    }

    #[test]
    fn a_response_header_lookup_ignores_case() {
        let response =
            HttpResponse::new(202, Vec::new()).header("Azure-AsyncOperation", "https://x");
        assert_eq!(
            response.header_value("azure-asyncoperation"),
            Some("https://x")
        );
        assert!(response.header_value("location").is_none());
    }
}
