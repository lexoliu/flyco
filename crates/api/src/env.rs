//! A session's `.env`: the environment its harness runs under.
//!
//! The user owns this document and the agent may only read it, so these two
//! routes are the only writers. Both are scoped through `sessions.user_id`:
//! the environment of a session belonging to somebody else is
//! indistinguishable from one that does not exist.
//!
//! # The values are secrets, and are treated as such
//!
//! Everything in here is a credential somebody pasted into a text box. The
//! document is sealed with [`TokenCipher`](crate::crypto::TokenCipher)
//! before it reaches D1, so a database dump — or a query log — carries
//! ciphertext; and nothing on this path logs a key or a value, only how many
//! there are. Flyco does not control a session's egress yet, which
//! [`NETWORK_CONTROL_WARNING`](flyco_core::NETWORK_CONTROL_WARNING) says on
//! every read rather than leaving it to a release note.
//!
//! # Why nothing is pushed to the daemon
//!
//! A process's environment is fixed when it starts, so there is no live
//! update to deliver: what a harness runs with is decided when flycod
//! launches it, and that is read from here at provisioning time (M4). A
//! `ControlToDaemon` variant that "pushed" a new environment would announce
//! a change that could not take effect until the next start anyway.

use flyco_core::{EnvDocument, EnvEntry, SessionId, UserId};
use serde::Deserialize;
use skyzen_services::Db;

use crate::clock::now_unix;
use crate::crypto::TokenCipher;
use crate::error::ApiError;
use crate::sql::to_column;

/// The sealed document, as the column holds it.
#[derive(Debug, Deserialize)]
struct EnvRow {
    entries_enc: String,
}

/// Reads a session's environment.
///
/// A session that has never been given one has an empty environment rather
/// than a missing resource: every session runs with *some* environment, and
/// the empty one is the honest description of a session nobody has
/// configured.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is not the caller's,
/// or a database or decryption failure otherwise.
pub async fn read(
    db: &Db,
    cipher: &TokenCipher,
    user: UserId,
    session: SessionId,
) -> Result<EnvDocument, ApiError> {
    if !crate::sessions::is_owned_by(db, user, session).await? {
        return Err(ApiError::SessionNotFound);
    }

    let row: Option<EnvRow> = db
        .query("SELECT entries_enc FROM session_env WHERE session_id = ?")
        .bind(session.to_string())
        .fetch_optional()
        .await?;

    let entries = match row {
        Some(row) => unseal(cipher, &row.entries_enc)?,
        None => Vec::new(),
    };
    Ok(EnvDocument::new(entries))
}

/// Replaces a session's environment.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is not the caller's,
/// [`ApiError::InvalidEnvKey`] if a name is not one a shell can export, or a
/// database or encryption failure otherwise.
pub async fn replace(
    db: &Db,
    cipher: &TokenCipher,
    user: UserId,
    session: SessionId,
    entries: Vec<EnvEntry>,
) -> Result<EnvDocument, ApiError> {
    if !crate::sessions::is_owned_by(db, user, session).await? {
        return Err(ApiError::SessionNotFound);
    }
    for entry in &entries {
        if !is_valid_key(&entry.key) {
            return Err(ApiError::InvalidEnvKey(entry.key.clone()));
        }
    }

    db.query(
        "INSERT INTO session_env (session_id, entries_enc, updated_at_unix) VALUES (?, ?, ?) \
         ON CONFLICT (session_id) DO UPDATE SET \
         entries_enc = excluded.entries_enc, updated_at_unix = excluded.updated_at_unix",
    )
    .bind(session.to_string())
    .bind(seal(cipher, &entries)?)
    .bind(to_column(now_unix()))
    .execute()
    .await?;

    // The count, never the keys: a variable's *name* is often enough to say
    // which service a session holds a credential for.
    tracing::info!(%session, variables = entries.len(), "replaced a session environment");
    Ok(EnvDocument::new(entries))
}

/// Whether `key` is a name a shell can export.
///
/// POSIX's definition of a name, plus the refusal of a leading digit: this
/// document becomes environment variables on a machine, and a name the shell
/// cannot express would be silently dropped there rather than here.
#[must_use]
pub fn is_valid_key(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with(|character: char| character.is_ascii_digit())
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Seals the whole document as one JSON value.
///
/// The entries are sealed together rather than one at a time so their order
/// — which the document promises to keep — is inside the ciphertext instead
/// of being carried by a column that a later write could reorder.
fn seal(cipher: &TokenCipher, entries: &[EnvEntry]) -> Result<String, ApiError> {
    let json = serde_json::to_string(entries)
        .map_err(|_| ApiError::CorruptRecord("a session environment failed to encode"))?;
    Ok(cipher.seal(&json)?)
}

fn unseal(cipher: &TokenCipher, sealed: &str) -> Result<Vec<EnvEntry>, ApiError> {
    let json = cipher.open(sealed)?;
    serde_json::from_str(&json)
        .map_err(|_| ApiError::CorruptRecord("session_env.entries_enc is not an environment"))
}

#[cfg(test)]
mod tests {
    use super::is_valid_key;

    #[test]
    fn a_name_a_shell_can_export_is_accepted() {
        for key in ["PATH", "GITHUB_TOKEN", "_private", "A1"] {
            assert!(is_valid_key(key), "`{key}` must be accepted");
        }
    }

    #[test]
    fn a_name_a_shell_cannot_export_is_refused() {
        for key in ["", "1ST", "with space", "with-dash", "a=b", "PATH\n"] {
            assert!(!is_valid_key(key), "`{key}` must be refused");
        }
    }
}
