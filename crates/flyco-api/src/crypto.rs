//! Secrets handling: sealing third-party tokens at rest, hashing the
//! credentials flyco itself issues, and minting them in the first place.
//!
//! Every primitive here is pure Rust and runs unchanged on
//! `wasm32-unknown-unknown`, because the control plane's only deployment
//! target is a Cloudflare Worker.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD as BASE64URL};
use sha2::{Digest as _, Sha256};

/// Byte length of an AES-256 key.
pub const KEY_LEN: usize = 32;

/// Byte length of the AES-GCM nonce prepended to every ciphertext.
const NONCE_LEN: usize = 12;

/// Byte length of the random material behind session tokens and API keys.
const TOKEN_LEN: usize = 32;

/// Prefix that marks a flyco REST API key, so a leaked key is recognisable in
/// logs and secret scanners.
pub const API_KEY_PREFIX: &str = "fk_";

/// Failures of the cryptographic primitives.
///
/// None of these are recoverable at runtime: they mean the key is wrong, the
/// stored ciphertext is corrupt, or the platform has no entropy source.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    /// The host refused to produce random bytes.
    #[error("the platform random number generator failed: {0}")]
    Entropy(#[from] getrandom::Error),
    /// A sealed value could not be base64-decoded.
    #[error("sealed value is not valid base64")]
    Malformed,
    /// A sealed value is shorter than the nonce it must carry.
    #[error("sealed value is truncated")]
    Truncated,
    /// Authenticated decryption rejected the ciphertext.
    #[error("sealed value failed authentication")]
    Unsealing,
    /// The plaintext recovered from a sealed value is not UTF-8.
    #[error("unsealed value is not valid UTF-8")]
    NotUtf8,
}

/// Fills `N` bytes from the platform CSPRNG.
///
/// # Errors
///
/// Returns [`CryptoError::Entropy`] if the host has no usable entropy source.
pub fn random_bytes<const N: usize>() -> Result<[u8; N], CryptoError> {
    let mut buffer = [0_u8; N];
    getrandom::fill(&mut buffer)?;
    Ok(buffer)
}

/// Mints an opaque credential: 32 random bytes in URL-safe base64.
///
/// # Errors
///
/// Returns [`CryptoError::Entropy`] if the host has no usable entropy source.
pub fn random_token() -> Result<String, CryptoError> {
    Ok(BASE64URL.encode(random_bytes::<TOKEN_LEN>()?))
}

/// Mints a REST API key: [`API_KEY_PREFIX`] followed by [`random_token`].
///
/// # Errors
///
/// Returns [`CryptoError::Entropy`] if the host has no usable entropy source.
pub fn random_api_key() -> Result<String, CryptoError> {
    let mut key = String::with_capacity(API_KEY_PREFIX.len() + 43);
    key.push_str(API_KEY_PREFIX);
    key.push_str(&random_token()?);
    Ok(key)
}

/// Lowercase hex SHA-256 of a credential.
///
/// This is the only form in which a flyco-issued credential is ever stored:
/// KV keys the session index by it, D1 stores it in `api_keys.token_hash`.
#[must_use]
pub fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Seals third-party tokens with AES-256-GCM under a single deployment key.
///
/// The sealed form is `base64(nonce || ciphertext)` with a fresh 96-bit
/// nonce per call, which is what lands in `users.github_token_enc`.
#[derive(Clone)]
pub struct TokenCipher {
    key: [u8; KEY_LEN],
}

impl core::fmt::Debug for TokenCipher {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TokenCipher").finish_non_exhaustive()
    }
}

impl TokenCipher {
    /// Builds a cipher around a 32-byte key.
    #[must_use]
    pub const fn new(key: [u8; KEY_LEN]) -> Self {
        Self { key }
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new((&self.key).into())
    }

    /// Seals `plaintext`, returning `base64(nonce || ciphertext)`.
    ///
    /// # Errors
    ///
    /// Returns [`CryptoError::Entropy`] if the nonce cannot be generated, or
    /// [`CryptoError::Unsealing`] if the AEAD refuses the input.
    pub fn seal(&self, plaintext: &str) -> Result<String, CryptoError> {
        let nonce_bytes = random_bytes::<NONCE_LEN>()?;
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = self
            .cipher()
            .encrypt(nonce, plaintext.as_bytes())
            .map_err(|_| CryptoError::Unsealing)?;

        let mut sealed = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        sealed.extend_from_slice(&nonce_bytes);
        sealed.extend_from_slice(&ciphertext);
        Ok(BASE64.encode(sealed))
    }

    /// Recovers the plaintext of a value produced by [`seal`](Self::seal).
    ///
    /// # Errors
    ///
    /// Returns a [`CryptoError`] if the value is not base64, is shorter than
    /// a nonce, fails authentication, or does not decode as UTF-8.
    pub fn open(&self, sealed: &str) -> Result<String, CryptoError> {
        let bytes = BASE64.decode(sealed).map_err(|_| CryptoError::Malformed)?;
        if bytes.len() <= NONCE_LEN {
            return Err(CryptoError::Truncated);
        }

        let (nonce_bytes, ciphertext) = bytes.split_at(NONCE_LEN);
        let plaintext = self
            .cipher()
            .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
            .map_err(|_| CryptoError::Unsealing)?;
        String::from_utf8(plaintext).map_err(|_| CryptoError::NotUtf8)
    }
}

#[cfg(test)]
mod tests {
    use super::{API_KEY_PREFIX, CryptoError, KEY_LEN, TokenCipher, random_api_key, token_hash};

    fn cipher() -> TokenCipher {
        TokenCipher::new([7_u8; KEY_LEN])
    }

    #[test]
    fn sealing_round_trips_and_hides_the_plaintext() {
        let secret = "gho_a-github-token";
        let sealed = cipher().seal(secret).expect("seal");

        assert!(!sealed.contains(secret));
        assert_eq!(cipher().open(&sealed).expect("open"), secret);
    }

    #[test]
    fn sealing_the_same_value_twice_yields_different_ciphertexts() {
        let first = cipher().seal("gho_token").expect("seal");
        let second = cipher().seal("gho_token").expect("seal");
        assert_ne!(first, second);
    }

    #[test]
    fn a_different_key_cannot_open_a_sealed_value() {
        let sealed = cipher().seal("gho_token").expect("seal");
        let other = TokenCipher::new([9_u8; KEY_LEN]);
        assert!(matches!(
            other.open(&sealed),
            Err(CryptoError::Unsealing | CryptoError::Truncated)
        ));
    }

    #[test]
    fn tampering_with_a_sealed_value_is_rejected() {
        let sealed = cipher().seal("gho_token").expect("seal");
        let mut tampered = sealed;
        tampered.push('A');
        assert!(cipher().open(&tampered).is_err());
    }

    #[test]
    fn api_keys_are_prefixed_and_unique() {
        let first = random_api_key().expect("mint");
        let second = random_api_key().expect("mint");

        assert!(first.starts_with(API_KEY_PREFIX));
        assert_ne!(first, second);
        assert_ne!(token_hash(&first), token_hash(&second));
        assert_eq!(token_hash(&first).len(), 64);
    }
}
