//! The control-plane HTTP client.
//!
//! One small type over zenwave rather than a request builder per call site:
//! every request carries the API key, every failure becomes a [`Failure`]
//! whose text is the server's `application/problem+json` verbatim, and only
//! idempotent `GET`s retry — a mutation that might have happened is never
//! sent twice, because a second `POST /v1/sessions` is a second billed
//! machine.

use core::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use url::Url;
use zenwave::{Client as _, Method, ResponseExt as _, sse::SseStream};

use crate::{Exit, Failure};

/// How many times a retryable `GET` is sent after the first.
const GET_ATTEMPTS: u32 = 3;

/// The backoff ladder a retry climbs when the server named no `Retry-After`.
const RETRY_BACKOFF: [Duration; GET_ATTEMPTS as usize] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];

/// The longest a single request may take before the transport gives up.
///
/// Generous because several routes relay to a session's daemon and wait on
/// its answer — the control plane's own deadline sits under this.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// The longest `Retry-After` a request will sleep out.
///
/// The per-minute window never names more than a minute or two; a `429`
/// asking for longer is the daily budget spent, and the answer to that is
/// the problem document in the failure — not a process asleep for an hour
/// inside a `GET` retry.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);

/// A bearer-token-authenticated handle on the control plane.
#[derive(Debug, Clone)]
pub struct Api {
    base: Url,
    token: Option<String>,
}

/// The verbs the client speaks, so `send` can tell a `GET` worth retrying
/// from everything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Verb {
    const fn method(self) -> Method {
        match self {
            Self::Get => Method::GET,
            Self::Post => Method::POST,
            Self::Put => Method::PUT,
            Self::Patch => Method::PATCH,
            Self::Delete => Method::DELETE,
        }
    }

    /// `GET` alone is retried: it is the only verb here that cannot create
    /// anything, so a retry is a re-read, never a second charge.
    const fn safe(self) -> bool {
        matches!(self, Self::Get)
    }
}

impl Api {
    /// A client rooted at `base` (`https://flyco.dev` unless `FLYCO_API_URL`
    /// said otherwise), presenting `token` as `Authorization: Bearer`.
    #[must_use]
    pub const fn new(base: Url, token: Option<String>) -> Self {
        Self { base, token }
    }

    /// The control-plane root this client talks to.
    #[must_use]
    pub fn base(&self) -> Url {
        self.base.clone()
    }

    /// The absolute URL of `path`, which always begins `/v1/…`.
    fn url(&self, path: &str) -> Result<Url, Failure> {
        self.base
            .join(path)
            .map_err(|error| Failure::usage(format!("bad API path {path}: {error}")))
    }

    /// `GET path` and decode the JSON body. Retried per the contract.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when the request cannot be sent,
    /// the answer is a problem document, or the body does not decode.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> crate::Outcome<T> {
        self.request::<(), T>(Verb::Get, path, None, &[]).await
    }

    /// `POST path` with a JSON body, decoding the JSON answer.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when the request cannot be sent,
    /// the answer is a problem document, or the body does not decode.
    pub async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> crate::Outcome<T> {
        self.request(Verb::Post, path, Some(body), &[]).await
    }

    /// `POST path` with a JSON body and extra headers (`Idempotency-Key`).
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when the request cannot be sent,
    /// the answer is a problem document, or the body does not decode.
    pub async fn post_with_headers<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        headers: &[(&str, &str)],
    ) -> crate::Outcome<T> {
        self.request(Verb::Post, path, Some(body), headers).await
    }

    /// `POST path` with no body, accepting whatever the route answers.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when the request cannot be sent,
    /// the answer is a problem document, or the body does not decode.
    pub async fn post_empty(&self, path: &str) -> crate::Outcome<()> {
        self.request::<(), serde_json::Value>(Verb::Post, path, None, &[])
            .await?;
        Ok(())
    }

    /// `PATCH path` with a JSON body, decoding the JSON answer.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when the request cannot be sent,
    /// the answer is a problem document, or the body does not decode.
    pub async fn patch<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> crate::Outcome<T> {
        self.request(Verb::Patch, path, Some(body), &[]).await
    }

    /// `PUT path` with a JSON body, decoding the JSON answer.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when the request cannot be sent,
    /// the answer is a problem document, or the body does not decode.
    pub async fn put<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> crate::Outcome<T> {
        self.request(Verb::Put, path, Some(body), &[]).await
    }

    /// `PUT path` with an opaque byte body, answering no document.
    ///
    /// The handoff uploads are the callers: a patch is not JSON and the
    /// route answers `204`, so the JSON path does not fit.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when the request cannot be sent
    /// or the answer is a problem document.
    pub async fn put_bytes(&self, path: &str, body: Vec<u8>) -> crate::Outcome<()> {
        let url = self.url(path)?;
        let mut client = zenwave::client();
        let mut builder = client
            .put(url.as_str())
            .map_err(|error| transport("PUT", path, error))?;
        if let Some(token) = &self.token {
            builder = builder.bearer_auth(token.clone());
        }
        let response = with_timeout(async move { builder.bytes_body(body).await })
            .await
            .map_err(|error| refused_or_transport("PUT", path, error))?;
        refused(path, response.status().as_u16(), response).await
    }

    /// `PUT path` streaming a file as the body — the transcript upload's
    /// shape, so a large one is not read into memory twice.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when the file cannot be opened,
    /// the request cannot be sent, or the answer is a problem document.
    pub async fn put_file(&self, path: &str, file: &std::path::Path) -> crate::Outcome<()> {
        let url = self.url(path)?;
        let mut client = zenwave::client();
        let mut builder = client
            .put(url.as_str())
            .map_err(|error| transport("PUT", path, error))?;
        if let Some(token) = &self.token {
            builder = builder.bearer_auth(token.clone());
        }
        let builder = builder
            .file_body(file)
            .await
            .map_err(|error| transport("PUT", path, error))?;
        let response = with_timeout(async move { builder.await })
            .await
            .map_err(|error| refused_or_transport("PUT", path, error))?;
        refused(path, response.status().as_u16(), response).await
    }

    /// `DELETE path`.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when the request cannot be sent,
    /// the answer is a problem document, or the body does not decode.
    pub async fn delete(&self, path: &str) -> crate::Outcome<()> {
        self.request::<(), serde_json::Value>(Verb::Delete, path, None, &[])
            .await?;
        Ok(())
    }

    /// `GET path` and answer with the status code alone.
    ///
    /// The cli-session poll is the caller: `202` pending, `200` carrying the
    /// key, `410` gone — statuses rather than documents, so it needs the
    /// code and the body both.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when the request cannot be sent,
    /// the answer is a problem document, or the body does not decode.
    pub async fn poll(&self, path: &str) -> crate::Outcome<zenwave::Response> {
        let url = self.url(path)?;
        let mut client = zenwave::client();
        let mut builder = client
            .get(url.as_str())
            .map_err(|error| transport("GET", path, error))?;
        if let Some(token) = &self.token {
            builder = builder.bearer_auth(token.clone());
        }
        with_timeout(async move { builder.await })
            .await
            .map_err(|error| refused_or_transport("GET", path, error))
    }

    /// `GET path` as an SSE stream.
    ///
    /// The response's status is checked before the body is handed over —
    /// zenwave's `into_sse` would otherwise happily parse a problem
    /// document as event data.
    ///
    /// # Errors
    /// Returns [`Failure`](crate::Failure) when the request cannot be sent,
    /// the answer is a problem document, or the body does not decode.
    pub async fn sse(&self, path: &str) -> crate::Outcome<SseStream> {
        let url = self.url(path)?;
        let mut client = zenwave::client();
        let mut builder = client
            .get(url.as_str())
            .map_err(|error| transport("GET", path, error))?;
        if let Some(token) = &self.token {
            builder = builder.bearer_auth(token.clone());
        }
        let response = with_timeout(async move { builder.await })
            .await
            .map_err(|error| refused_or_transport("GET", path, error))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let wait = refused_wait(&response, status);
            let body = response.into_string().await.unwrap_or_default();
            return Err(problem("GET", path, status, body.as_str(), wait));
        }
        Ok(response.into_sse())
    }

    /// One request, retried when the verb allows it and the failure says to.
    async fn request<B: Serialize, T: DeserializeOwned>(
        &self,
        verb: Verb,
        path: &str,
        body: Option<&B>,
        headers: &[(&str, &str)],
    ) -> crate::Outcome<T> {
        let url = self.url(path)?;
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.send(verb, &url, body, headers).await {
                Ok(response) => {
                    return response
                        .into_json::<T>()
                        .await
                        .map_err(|error| transport(verb_name(verb), path, error));
                }
                Err(error) => {
                    let retryable = verb.safe() && attempt <= GET_ATTEMPTS;
                    let Some(wait) = retry_wait(&error, retryable, attempt) else {
                        return Err(refused_or_transport(verb_name(verb), path, error));
                    };
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }

    async fn send<B: Serialize>(
        &self,
        verb: Verb,
        url: &Url,
        body: Option<&B>,
        headers: &[(&str, &str)],
    ) -> Result<zenwave::Response, zenwave::Error> {
        let mut client = zenwave::client();
        let mut builder = client.method(verb.method(), url.as_str())?;
        if let Some(token) = &self.token {
            builder = builder.bearer_auth(token.clone());
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value)?;
        }
        let builder = match body {
            Some(body) => builder.json_body(body)?,
            None => builder,
        };
        with_timeout(async move { builder.await }).await
    }
}

/// `REQUEST_TIMEOUT` around one request's send-and-headers await.
async fn with_timeout<F, T>(future: F) -> Result<T, zenwave::Error>
where
    F: core::future::Future<Output = Result<T, zenwave::Error>>,
{
    match tokio::time::timeout(REQUEST_TIMEOUT, future).await {
        Ok(done) => done,
        Err(_elapsed) => Err(zenwave::Error::Timeout),
    }
}

const fn verb_name(verb: Verb) -> &'static str {
    match verb {
        Verb::Get => "GET",
        Verb::Post => "POST",
        Verb::Put => "PUT",
        Verb::Patch => "PATCH",
        Verb::Delete => "DELETE",
    }
}

/// What a failed request's answer says about trying again.
#[derive(Debug, Clone, Copy)]
enum Retry {
    /// The failure is final — a 4xx, a body that did not decode.
    Never,
    /// Retryable, and the server named no wait: climb the backoff ladder.
    Default,
    /// Retryable after exactly this long — `Retry-After` honored.
    After(Duration),
}

/// The wait before `attempt`'s retry, or `None` when the failure is
/// final. A named `Retry-After` is honored up to [`MAX_RETRY_AFTER`] —
/// a `429` asking for more is the daily budget spent, and the answer to
/// that is the problem document, not a sleeping process.
fn retry_wait(error: &zenwave::Error, retryable: bool, attempt: u32) -> Option<Duration> {
    if !retryable {
        return None;
    }
    match retry_after(error) {
        Retry::After(named) if named <= MAX_RETRY_AFTER => Some(named),
        Retry::Default => Some(RETRY_BACKOFF[(attempt - 1) as usize]),
        _ => None,
    }
}

/// The wait a failed request asked for, if the failure is one worth
/// waiting out: `429` and `5xx` honor `Retry-After`, and a transport or
/// timeout failure retries on the default ladder.
fn retry_after(error: &zenwave::Error) -> Retry {
    match error {
        zenwave::Error::Http {
            status, response, ..
        } => {
            let retryable = status.as_u16() == 429 || status.is_server_error();
            if !retryable {
                return Retry::Never;
            }
            named_wait(&response.response).map_or(Retry::Default, Retry::After)
        }
        zenwave::Error::Transport(_) | zenwave::Error::Tls(_) | zenwave::Error::Timeout => {
            Retry::Default
        }
        _ => Retry::Never,
    }
}

/// The `Retry-After` seconds a response named, if it named a number —
/// the header is also allowed to be an HTTP date, which this control
/// plane never sends.
fn named_wait(response: &zenwave::Response) -> Option<Duration> {
    response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// The wait a refused request reports: `Retry-After` on a `429`, where it
/// names the budget window's wait. Other statuses either honor the header
/// silently on the retry ladder (`5xx`) or have no wait to report.
fn refused_wait(response: &zenwave::Response, status: u16) -> Option<Duration> {
    if status == 429 {
        named_wait(response)
    } else {
        None
    }
}

/// The status check the raw-body paths share: a success is `()`, anything
/// else is a problem document to repeat.
async fn refused(path: &str, status: u16, response: zenwave::Response) -> crate::Outcome<()> {
    if (200..300).contains(&status) {
        return Ok(());
    }
    let wait = refused_wait(&response, status);
    let body = response.into_string().await.unwrap_or_default();
    Err(problem("PUT", path, status, &body, wait))
}

/// A transport-level failure: nothing was answered, so there is no problem
/// document to repeat. Takes anything displayable — a body that failed to
/// decode is the same kind of "no usable answer" as a dropped connection.
fn transport(verb: &'static str, path: &str, error: impl core::fmt::Display) -> Failure {
    Failure::transport(format!("{verb} {path}: {error}"))
}

/// A refused request: repeat the problem document verbatim when there is
/// one, name the status when there is not, and let the status pick the
/// exit code — auth refusals are 3, not-found and conflict are 4, every
/// other problem is 5.
fn refused_or_transport(verb: &'static str, path: &str, error: zenwave::Error) -> Failure {
    if let zenwave::Error::Http {
        status, response, ..
    } = &error
    {
        let body = response.body_text.as_deref().unwrap_or("");
        let wait = refused_wait(&response.response, status.as_u16());
        return problem(verb, path, status.as_u16(), body, wait);
    }
    transport(verb, path, error)
}

fn problem(
    verb: &str,
    path: &str,
    status: u16,
    body: &str,
    retry_after: Option<Duration>,
) -> Failure {
    let code = match status {
        401 | 403 => Exit::Auth,
        404 | 409 | 410 => Exit::NotFoundOrConflict,
        _ => Exit::Problem,
    };
    let text = if body.trim().is_empty() {
        format!("{verb} {path}: HTTP {status}")
    } else {
        body.trim().to_owned()
    };
    Failure::refused(code, text, retry_after)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `429` failure carrying `Retry-After: <secs>`.
    fn rate_limited(secs: u64) -> zenwave::Error {
        let status = zenwave::StatusCode::TOO_MANY_REQUESTS;
        let mut response = zenwave::Response::new(zenwave::Body::from_bytes("{}"));
        *response.status_mut() = status;
        response.headers_mut().insert(
            "retry-after",
            zenwave::header::HeaderValue::from_str(&secs.to_string())
                .expect("a number is a header value"),
        );
        zenwave::Error::Http {
            status,
            message: "too many requests".to_owned(),
            response: Box::new(zenwave::error::HttpErrorResponse {
                response,
                body_text: None,
            }),
        }
    }

    /// A `Retry-After` past [`MAX_RETRY_AFTER`] is the daily budget spent:
    /// `retry_wait` treats it like a final failure — `None`, never a
    /// sleep — while a wait inside the cap is honored verbatim.
    #[test]
    fn retry_after_over_the_cap_is_never_retried() {
        let over = rate_limited(MAX_RETRY_AFTER.as_secs() + 1);
        assert!(retry_wait(&over, true, 1).is_none());
        let under = rate_limited(30);
        assert_eq!(retry_wait(&under, true, 1), Some(Duration::from_secs(30)));
    }
}
