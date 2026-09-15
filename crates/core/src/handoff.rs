//! `flyco handoff`: a local harness session's working state sent to a
//! fresh cloud session.
//!
//! A handoff is *not* a resume. The cloud session opens a new harness
//! conversation whose first message is a handoff brief — the local
//! session's own compact summary, an explicit environment-migration
//! notice, and a path map — while the working tree arrives as a binary
//! patch and the full transcript lands on disk for the agent to consult.
//!
//! # Wire flow
//!
//! 1. `POST /v1/sessions` with [`SessionSource::LocalHandoff`] records a
//!    pending `handoffs` row; the machine is reserved but its provisioning
//!    job is not yet queued — a daemon that booted before the payload
//!    landed would clone without the patch.
//! 2. `PUT /v1/sessions/{id}/handoff/patch` and
//!    `PUT /v1/sessions/{id}/handoff/transcript` upload the two payloads
//!    to object storage.
//! 3. `POST /v1/sessions/{id}/handoff/complete` carries the
//!    [`HandoffManifest`] of what was uploaded; the control plane checks
//!    the objects exist at the declared sizes, marks the row complete, and
//!    only then enqueues provisioning.
//!
//! On the machine, `flycod` asks `GET /v1/sessions/{id}/handoff` after
//! cloning: the [`HandoffView`] names the base commit to check out before
//! the patch applies and whether a transcript object waits to be written
//! to [`HANDOFF_TRANSCRIPT_PATH`].

use serde::{Deserialize, Serialize};

/// The workdir every provisioned machine checks the session's repository
/// out into.
///
/// A constant rather than a per-machine fact because the handoff prompt's
/// path map is written before the machine exists: the CLI rewrites the
/// sender's local workdir prefix to this path, and could not do that if
/// the destination were only known at provision time.
pub const SESSION_WORKDIR: &str = "/srv/flyco/work";

/// The directory a handoff's artifacts are materialized into, outside the
/// workdir so the session's diff and file views stay clean.
pub const HANDOFF_DIR: &str = "/var/lib/flyco/handoff";

/// Where a handoff's uploaded transcript lands on the cloud machine.
///
/// Outside the workdir on purpose: inside it the file would appear in
/// every diff the browser renders. The handoff prompt names this path so
/// the agent can grep the previous session's history for detail the
/// compact summary dropped.
pub const HANDOFF_TRANSCRIPT_PATH: &str = "/var/lib/flyco/handoff/transcript";

/// Largest handoff patch accepted, in bytes.
///
/// Generous rather than tight — a handoff can carry months of branch
/// divergence — but bounded so a malformed client cannot stream unbounded
/// data into object storage through the API.
pub const HANDOFF_PATCH_BYTES_MAX: u64 = 256 * 1024 * 1024;

/// Largest handoff transcript accepted, in bytes. Transcripts are JSONL
/// and compact-but-not-tiny; a gigabyte is well past any real session.
pub const HANDOFF_TRANSCRIPT_BYTES_MAX: u64 = 1024 * 1024 * 1024;

/// Where a new session's context comes from, as `CreateSession::source`.
///
/// Absent means a fresh session — the overwhelmingly common case, so the
/// field is an `Option` rather than a required tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionSource {
    /// `flyco handoff`: import a local harness session's state.
    LocalHandoff(LocalHandoff),
}

/// The provenance a `flyco handoff` create declares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct LocalHandoff {
    /// Which harness the local session ran under. The cloud session's own
    /// `harness` may differ — a Claude session handed to Codex is a
    /// legitimate move — so the source is recorded rather than assumed.
    pub harness: crate::harness::HarnessKind,
    /// The harness-native session id being handed off — Claude's session
    /// UUID, Codex's rollout/thread id, Devin's session name.
    pub session_id: String,
    /// The commit the local work was based on — `git merge-base HEAD
    /// origin/<branch>` — which the cloud checkout rewinds the branch to
    /// before the patch applies, so the patch reproduces the local tree
    /// byte-for-byte rather than failing against a moved branch tip.
    pub base_commit: String,
    /// The sender's local workdir as an absolute path. Recorded for the
    /// handoff prompt's path map and for forensics; the daemon never
    /// materializes it.
    pub local_workdir: String,
}

/// `POST /v1/sessions/{id}/handoff/complete` body — the integrity record
/// of the two uploaded objects.
///
/// Checksums, not just sizes: the daemon verifies `patch_sha256` before
/// `git apply`, so a corrupted object is a clear startup failure rather
/// than a mysteriously malformed diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HandoffManifest {
    /// Lowercase hex SHA-256 of the patch object.
    pub patch_sha256: String,
    /// Byte length of the patch object.
    pub patch_bytes: u64,
    /// Lowercase hex SHA-256 of the transcript object.
    pub transcript_sha256: String,
    /// Byte length of the transcript object.
    pub transcript_bytes: u64,
}

/// `GET /v1/sessions/{id}/handoff`, daemon-scoped — what the daemon
/// materializes after cloning, or `404` for a session that was never a
/// handoff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HandoffView {
    /// The commit to check out before applying the stored patch.
    pub base_commit: String,
    /// SHA-256 the applied patch must hash to, verified before `git
    /// apply` so a corrupt object fails loudly.
    pub patch_sha256: String,
    /// Whether a transcript object exists for
    /// `GET /v1/sessions/{id}/handoff/transcript` to fetch.
    pub has_transcript: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_handoff_source_serializes_tagged() {
        let source = SessionSource::LocalHandoff(LocalHandoff {
            harness: crate::harness::HarnessKind::Codex,
            session_id: "s-1".to_owned(),
            base_commit: "abc123".to_owned(),
            local_workdir: "/home/u/repo".to_owned(),
        });
        let json = serde_json::to_value(&source).expect("serialize");
        assert_eq!(json["kind"], "local_handoff");
        assert_eq!(json["session_id"], "s-1");
        let back: SessionSource = serde_json::from_value(json).expect("roundtrip");
        assert_eq!(back, source);
    }
}
