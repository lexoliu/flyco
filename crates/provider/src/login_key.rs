//! The login key of every machine flyco builds.
//!
//! A cloud will not create a Linux machine that has neither a password nor
//! an SSH key, and flyco sets no passwords. The key is flyco's: the user
//! has a browser and signs in from anywhere, so there is nobody to hand a
//! private key to and nowhere for them to keep it. The control plane mints
//! one pair per linked account, seals the private half beside the account's
//! other credentials, and installs the public half on each machine it
//! builds. Nothing about it reaches the browser.

use core::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// An Ed25519 key pair, held by flyco for the machines of one account.
#[derive(Clone, PartialEq, Eq)]
pub struct LoginKey(ssh_key::PrivateKey);

/// Why stored key text could not be read back as a key.
#[derive(Debug, thiserror::Error)]
#[error("the stored machine login key is not an OpenSSH private key: {0}")]
pub struct LoginKeyError(ssh_key::Error);

impl LoginKey {
    /// A fresh pair.
    ///
    /// # Panics
    ///
    /// If the platform's random source fails, which is a broken runtime
    /// rather than a condition to handle.
    #[must_use]
    pub fn generate() -> Self {
        Self(
            ssh_key::PrivateKey::random(&mut rand_core::OsRng, ssh_key::Algorithm::Ed25519)
                .expect("an Ed25519 key is always generable"),
        )
    }

    /// Reads back what [`to_openssh`](Self::to_openssh) wrote.
    ///
    /// # Errors
    ///
    /// Returns [`LoginKeyError`] if the text is not an OpenSSH private key.
    pub fn from_openssh(text: &str) -> Result<Self, LoginKeyError> {
        ssh_key::PrivateKey::from_openssh(text)
            .map(Self)
            .map_err(LoginKeyError)
    }

    /// The private half, in OpenSSH's own text form. A secret.
    ///
    /// # Panics
    ///
    /// Never for a key this type made: an unencrypted Ed25519 key always
    /// encodes.
    #[must_use]
    pub fn to_openssh(&self) -> String {
        self.0
            .to_openssh(ssh_key::LineEnding::LF)
            .expect("an unencrypted Ed25519 key always encodes as OpenSSH")
            .to_string()
    }

    /// The public half, as `authorized_keys` takes it.
    ///
    /// # Panics
    ///
    /// Never for a key this type made: an Ed25519 public key always
    /// encodes.
    #[must_use]
    pub fn public_openssh(&self) -> String {
        self.0
            .public_key()
            .to_openssh()
            .expect("an Ed25519 public key always encodes as OpenSSH")
    }
}

impl fmt::Debug for LoginKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LoginKey(..)")
    }
}

impl Serialize for LoginKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_openssh())
    }
}

impl<'de> Deserialize<'de> for LoginKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::from_openssh(&text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::LoginKey;

    #[test]
    fn a_key_survives_the_round_trip_through_its_stored_form() {
        let key = LoginKey::generate();
        let stored = serde_json::to_string(&key).expect("a key serializes");
        let back: LoginKey = serde_json::from_str(&stored).expect("a stored key reads back");
        assert_eq!(back.public_openssh(), key.public_openssh());
        assert!(key.public_openssh().starts_with("ssh-ed25519 "));
    }

    #[test]
    fn the_debug_form_shows_nothing_of_the_key() {
        assert_eq!(format!("{:?}", LoginKey::generate()), "LoginKey(..)");
    }
}
