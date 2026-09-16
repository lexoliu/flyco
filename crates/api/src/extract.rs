//! Extractors flyco supplies for itself, and the two ways a handler reads a
//! path parameter.

use core::convert::Infallible;
use core::future::{Future, ready};
use core::str::FromStr;

use flyco_core::RegionLocation;
use skyzen::Request;
use skyzen::extract::Extractor;
use skyzen::header::{AUTHORIZATION, HeaderMap};
use skyzen::routing::Params;

use crate::error::ApiError;

/// Reads a path parameter as a typed identifier.
///
/// A parameter the router never bound is a routing bug rather than
/// something a caller can provoke, which is why the two failures are told
/// apart: one is a 500 the operator sees in the log, the other a 400 that
/// names the value.
///
/// # Errors
///
/// Returns [`ApiError::MalformedId`] if the value does not parse, or
/// [`ApiError::CorruptRecord`] if the router bound no such parameter.
pub fn path_id<T: FromStr>(params: &Params, name: &'static str) -> Result<T, ApiError> {
    let value = raw(params, name)?;
    value
        .parse()
        .map_or_else(|_| Err(ApiError::MalformedId(value)), Ok)
}

/// Reads a path parameter as an owned string.
///
/// # Errors
///
/// Returns [`ApiError::CorruptRecord`] if the router bound no such
/// parameter.
pub fn path_segment(params: &Params, name: &'static str) -> Result<String, ApiError> {
    raw(params, name)
}

fn raw(params: &Params, name: &'static str) -> Result<String, ApiError> {
    params
        .get(name)
        .map(ToOwned::to_owned)
        .map_err(|_| ApiError::CorruptRecord("the router did not bind a path parameter"))
}

/// The request's headers, verbatim.
///
/// Skyzen's `TypedHeader<T>` reads one RFC-defined header into its typed
/// form, which is not what these routes need: the relay hops read a
/// credential out of `Authorization`, and the Worker→room hop reads a
/// role out of an internal header with a name of flyco's own. Cloning the
/// map is the honest cost of reading arbitrary headers in a handler
/// rather than in middleware.
#[derive(Debug, Clone)]
pub struct Headers(HeaderMap);

impl Headers {
    /// Wraps an existing header map.
    #[must_use]
    pub const fn new(headers: HeaderMap) -> Self {
        Self(headers)
    }

    /// One header's value, if it is present and is valid UTF-8.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name)?.to_str().ok()
    }

    /// The credential of an `Authorization: Bearer …` header.
    ///
    /// The scheme is matched case-insensitively per RFC 7235.
    #[must_use]
    pub fn bearer(&self) -> Option<&str> {
        let value = self.0.get(AUTHORIZATION)?.to_str().ok()?;
        let (scheme, token) = value.split_once(char::is_whitespace)?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return None;
        }
        let token = token.trim_start();
        (!token.is_empty()).then_some(token)
    }
}

impl Extractor for Headers {
    type Error = Infallible;

    fn extract(request: &mut Request) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        ready(Ok(Self::new(request.headers().clone())))
    }
}

/// Where the caller is, as Cloudflare's edge saw it.
///
/// `request.cf` is a Cloudflare value with no native counterpart — skyzen
/// deliberately types it `wasm32`-only rather than lie about the caller's
/// location on another host — so this extractor's two halves are compiled
/// apart. On the Worker it decodes what the runtime put into request
/// extensions during conversion: a city-centroid position rather than a
/// GPS fix, which is exactly the resolution a region ranking needs. Off
/// the edge it answers `None`, which asks `auto_linux_choice` for the
/// geography-blind ordering it has always produced rather than guessing at
/// a place.
#[derive(Debug, Clone, Copy)]
pub struct CallerLocation(pub Option<RegionLocation>);

impl Extractor for CallerLocation {
    type Error = Infallible;

    #[cfg(target_arch = "wasm32")]
    fn extract(request: &mut Request) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        use skyzen::runtime::CfPropertiesSlot;

        // An undecodable `cf` reads the same as an absent one: either way
        // there is no position to rank by, and the runtime already logged
        // why. The slot itself is absent off Cloudflare and on requests the
        // Durable Object glue did not decorate.
        let location = match request.extensions().get::<CfPropertiesSlot>() {
            Some(CfPropertiesSlot(Ok(cf))) => cf.location(),
            _ => None,
        };
        ready(Ok(Self(location)))
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn extract(_request: &mut Request) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        ready(Ok(Self(None)))
    }
}

/// What flyco asks of a `request.cf` position. A private trait because
/// `CfProperties` exists only on the Worker and an impl on a foreign type
/// cannot be cfg-gated alone.
#[cfg(target_arch = "wasm32")]
trait CfPropertiesExt {
    /// The caller's position parsed out of the edge metadata, or `None`
    /// when the platform did not place the request.
    ///
    /// A coordinate that does not parse to a finite number makes the whole
    /// position `None` rather than half a place — and keeps every
    /// [`RegionLocation`] the API produces finite, which is what the type's
    /// `Eq` stands on.
    fn location(&self) -> Option<RegionLocation>;
}

#[cfg(target_arch = "wasm32")]
impl CfPropertiesExt for skyzen::runtime::CfProperties {
    fn location(&self) -> Option<RegionLocation> {
        let location = RegionLocation {
            latitude: self.latitude.as_deref()?.parse().ok()?,
            longitude: self.longitude.as_deref()?.parse().ok()?,
        };
        (location.latitude.is_finite() && location.longitude.is_finite()).then_some(location)
    }
}

#[cfg(test)]
mod tests {
    use skyzen::header::{AUTHORIZATION, HeaderMap, HeaderValue};

    use super::Headers;

    fn headers(name: &'static str, value: &str) -> Headers {
        let mut map = HeaderMap::new();
        map.insert(name, HeaderValue::from_str(value).expect("header value"));
        Headers::new(map)
    }

    #[test]
    fn the_bearer_scheme_is_case_insensitive() {
        assert_eq!(
            headers(AUTHORIZATION.as_str(), "bEaReR  fd_abc").bearer(),
            Some("fd_abc")
        );
    }

    #[test]
    fn other_authorization_schemes_carry_no_bearer_token() {
        assert_eq!(headers(AUTHORIZATION.as_str(), "Basic abc").bearer(), None);
        assert_eq!(headers(AUTHORIZATION.as_str(), "Bearer   ").bearer(), None);
        assert_eq!(Headers::new(HeaderMap::new()).bearer(), None);
    }

    #[test]
    fn a_named_header_reads_back() {
        let headers = headers("x-flyco-role", "daemon");
        assert_eq!(headers.get("x-flyco-role"), Some("daemon"));
        assert_eq!(headers.get("x-flyco-session"), None);
    }
}
