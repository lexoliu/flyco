//! The credential the CLI holds: one `fk_` API key per machine.
//!
//! `flyco login` writes it to `~/.config/flyco/credentials.json` mode 0600;
//! `FLYCO_TOKEN` overrides it entirely, which is how a headless agent
//! authenticates without a file at all. `logout` revokes the stored key
//! (when it knows the key's id) and removes the file.

use std::path::{Path, PathBuf};

use flyco_core::ApiKeyId;
use serde::{Deserialize, Serialize};

use crate::{Failure, TOKEN_ENV};

/// What `credentials.json` stores.
///
/// `key_id` is what `flyco logout` revokes. It is `None` only for a key
/// that arrived through `--token`/`FLYCO_TOKEN` — a caller-provided key
/// cannot name its row, because the API never shows the token again, so
/// logout of one is file removal alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    /// The API key's id, when the login flow minted it.
    pub key_id: Option<ApiKeyId>,
    /// The `fk_` bearer token every request sends.
    pub key: String,
}

/// Where the file lives: `~/.config/flyco/credentials.json`.
///
/// `FLYCO_CONFIG_DIR` overrides the directory, which is how tests and
/// containers keep their credentials apart from a user's.
pub fn dir() -> PathBuf {
    if let Ok(custom) = std::env::var("FLYCO_CONFIG_DIR") {
        return PathBuf::from(custom);
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(".config/flyco")
}

/// The file itself.
#[must_use]
pub fn path() -> PathBuf {
    dir().join("credentials.json")
}

/// The credential a command should send, if it has one.
///
/// `FLYCO_TOKEN` wins over the file: an environment that carries a token
/// is stating its credential, and a stale file must not talk over it.
#[must_use]
pub fn resolve() -> Option<Credentials> {
    if let Ok(key) = std::env::var(TOKEN_ENV)
        && !key.is_empty()
    {
        return Some(Credentials { key_id: None, key });
    }
    read(&path())
}

/// Reads the file; a missing or unreadable one is no credential.
fn read(path: &Path) -> Option<Credentials> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Writes the file mode 0600, creating `~/.config/flyco` as needed.
///
/// The permission bits are the point — an API key in a world-readable file
/// is a credential leak, so the mode is set rather than assumed.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when the file cannot be written.
pub fn store(credentials: &Credentials) -> crate::Outcome<PathBuf> {
    let dir = dir();
    std::fs::create_dir_all(&dir)
        .map_err(|error| Failure::usage(format!("cannot create {}: {error}", dir.display())))?;
    let path = dir.join("credentials.json");
    write_secret(&path, credentials)
}

#[cfg(unix)]
fn write_secret(path: &Path, credentials: &Credentials) -> crate::Outcome<PathBuf> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let json = serde_json::to_string_pretty(credentials)
        .map_err(|error| Failure::usage(format!("credentials failed to serialize: {error}")))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| Failure::usage(format!("cannot write {}: {error}", path.display())))?;
    file.write_all(json.as_bytes())
        .map_err(|error| Failure::usage(format!("cannot write {}: {error}", path.display())))?;
    // `mode` applies only at creation; an existing file keeps what it had,
    // so the bits are enforced explicitly.
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .map_err(|error| Failure::usage(format!("cannot chmod {}: {error}", path.display())))?;
    Ok(path.to_path_buf())
}

#[cfg(not(unix))]
fn write_secret(path: &Path, credentials: &Credentials) -> crate::Outcome<PathBuf> {
    let json = serde_json::to_string_pretty(credentials)
        .map_err(|error| Failure::usage(format!("credentials failed to serialize: {error}")))?;
    std::fs::write(path, json)
        .map_err(|error| Failure::usage(format!("cannot write {}: {error}", path.display())))?;
    Ok(path.to_path_buf())
}

/// Removes the file, if there is one.
pub fn remove() {
    let _ = std::fs::remove_file(path());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_round_trips_under_0600() {
        let dir = std::env::temp_dir().join(format!("flyco-creds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("valid");
        let path = dir.join("credentials.json");

        let credentials = Credentials {
            key_id: Some(ApiKeyId::generate()),
            key: "fk_a-test-key".to_owned(),
        };
        write_secret(&path, &credentials).expect("valid");
        assert_eq!(read(&path), Some(credentials));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path)
                .expect("valid")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "the file is owner-only");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
