//! A session's `.env`: the environment the harness runs under.
//!
//! The user owns this document; the agent reads it and may not write it.
//! Flyco does not control the network a session reaches yet, so a `.env`
//! entry is a secret handed to a process that can still talk to anything —
//! [`NETWORK_CONTROL_WARNING`] says so, and every read of the document
//! carries it rather than leaving the caveat to a release note.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The caveat every reader of a session `.env` is shown.
///
/// Flyco sandboxes the machine but not its egress: until network control
/// ships, a secret placed here is reachable by anything the agent runs.
pub const NETWORK_CONTROL_WARNING: &str =
    "Anything the agent runs can read these and reach the network. Use short-lived credentials.";

/// One `KEY=value` pair of a session's environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct EnvEntry {
    /// Variable name, as the harness process will see it.
    pub key: String,
    /// Variable value, verbatim.
    pub value: String,
}

/// Response of `GET`/`PUT /v1/sessions/{id}/env`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct EnvDocument {
    /// Every variable the session runs with, in the order it is stored.
    pub entries: Vec<EnvEntry>,
    /// [`NETWORK_CONTROL_WARNING`], repeated on every response so a client
    /// renders the caveat beside the values rather than hard-coding it.
    pub warning: String,
}

/// Request body of `PUT /v1/sessions/{id}/env`.
///
/// Separate from [`EnvDocument`] because [`warning`](EnvDocument::warning)
/// is the control plane's to say: a request shape that carried it would
/// invite a client to submit one and be silently ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UpdateEnv {
    /// The complete new set of variables; this replaces the document.
    pub entries: Vec<EnvEntry>,
}

impl EnvDocument {
    /// Builds a document around `entries`, attaching the standing warning.
    #[must_use]
    pub fn new(entries: Vec<EnvEntry>) -> Self {
        Self {
            entries,
            warning: NETWORK_CONTROL_WARNING.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EnvDocument, EnvEntry, NETWORK_CONTROL_WARNING, UpdateEnv};

    #[test]
    fn a_document_always_carries_the_warning() {
        let document = EnvDocument::new(vec![EnvEntry {
            key: "GITHUB_TOKEN".to_owned(),
            value: "gho_example".to_owned(),
        }]);
        assert_eq!(document.warning, NETWORK_CONTROL_WARNING);

        let json = serde_json::to_string(&document).expect("serialize");
        let back: EnvDocument = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, document);
    }

    #[test]
    fn a_submitted_warning_is_not_part_of_an_update() {
        let update: UpdateEnv =
            serde_json::from_str(r#"{"entries":[{"key":"A","value":"1"}],"warning":"mine"}"#)
                .expect("deserialize");
        assert_eq!(update.entries.len(), 1);
    }
}
