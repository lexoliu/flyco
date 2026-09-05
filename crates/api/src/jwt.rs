//! Reading a token the control plane was just handed, without verifying its
//! signature.
//!
//! Three vendors hand flyco an `id_token` in the TLS response to a token
//! exchange this control plane made, authenticated by its own client secret
//! or PKCE verifier: `OpenAI`, Microsoft and Google. What flyco reads out of
//! them — an address to label a card with, the tenant a subscription lives
//! in — is never an authorization decision, so a forged claim would misname
//! a card rather than admit anybody to anything.
//!
//! Verifying the signature would mean fetching and caching each vendor's
//! JWKS to re-answer a question the transport already answered, so this
//! module deliberately does not: it splits the compact serialization and
//! deserializes the payload, which is what
//! [`deserialize_claims_unchecked`](jwt_compact::UntrustedToken::deserialize_claims_unchecked)
//! does.
//!
//! One helper rather than one per vendor, so the choice above is stated —
//! and reviewable — in exactly one place.

use jwt_compact::UntrustedToken;

/// Why a token's claims could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum JwtError {
    /// The value is not a compact JWT at all.
    #[error("the token is not a JWT")]
    NotAJwt,
    /// The payload is not the JSON the caller asked for.
    #[error("the token's claims are not readable")]
    Unreadable,
}

impl JwtError {
    /// This failure as a `'static` sentence, for an error type that carries
    /// one rather than a source.
    #[must_use]
    pub const fn detail(self) -> &'static str {
        match self {
            Self::NotAJwt => "the token is not a JWT",
            Self::Unreadable => "the token's claims are not readable",
        }
    }
}

/// Reads one token's custom claims without checking its signature.
///
/// See this module's own documentation for why that is the right thing
/// here and would not be elsewhere.
///
/// # Errors
///
/// Returns [`JwtError`] if the value is not a compact JWT, or if its
/// payload is not `T`.
pub fn claims<T: serde::de::DeserializeOwned>(jwt: &str) -> Result<T, JwtError> {
    Ok(UntrustedToken::new(jwt)
        .map_err(|_| JwtError::NotAJwt)?
        .deserialize_claims_unchecked::<T>()
        .map_err(|_| JwtError::Unreadable)?
        .custom)
}

#[cfg(test)]
mod tests {
    use super::{JwtError, claims};

    /// A `ChatGPT` id token, which is the fixture nearest to hand: the point
    /// here is the split and the payload, not whose token it is.
    const ID_TOKEN: &str = include_str!("../fixtures/openai/id_token.jwt");

    #[derive(Debug, serde::Deserialize)]
    struct Email {
        #[serde(default)]
        email: Option<String>,
    }

    #[test]
    fn a_payload_reads_back_without_a_key() {
        let read: Email = claims(ID_TOKEN.trim()).expect("the fixture is a JWT");
        assert_eq!(read.email.as_deref(), Some("me@lexo.cool"));
    }

    #[test]
    fn something_that_is_not_a_jwt_is_refused_rather_than_guessed_at() {
        assert_eq!(
            claims::<Email>("not-a-jwt").expect_err("a bare string is not a JWT"),
            JwtError::NotAJwt
        );
    }
}
