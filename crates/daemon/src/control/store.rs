//! The control-plane-backed transcript store.
//!
//! [`JsonlTranscriptStore`](crate::harness::claude::store::JsonlTranscriptStore)
//! keeps a session's transcript on the VM's disk, which is exactly what a
//! spot eviction takes away. This one keeps it in the control plane's object
//! storage instead, which is what makes History work: a session resumes onto
//! *any* machine because the transcript was never on the old one.
//!
//! # Batches, and why the sequence is read before it is written
//!
//! Each `append` becomes one immutable object. Numbering restarts nowhere:
//! the first write of a stream asks the control plane how many batches it
//! already holds and continues from there, because a session that moved host
//! would otherwise write batch 0 over the batch 0 its predecessor wrote —
//! and the control plane refuses that with a conflict rather than silently
//! reordering the transcript.
//!
//! # Why there is no local dedup
//!
//! The local store dedups on entry `uuid` because it can cheaply read back
//! what it wrote. Here a read is a network round trip over the whole stream,
//! so the batch *sequence* carries idempotency instead: re-appending the
//! same batch is refused by its key, and the SDK's own re-sends land as new
//! batches whose duplicate uuids the reader collapses. Correctness lives in
//! the immutable key, not in a cache that a reconnect would invalidate.

use std::collections::HashMap;

use serde_json::Value;

use crate::control::rest::{ControlApi, ControlApiError};
use crate::harness::claude::protocol::SessionKey;
use crate::harness::claude::store::{StoreError, TranscriptStore};

/// Longest stream key the control plane accepts as one path segment.
const MAX_STREAM_LEN: usize = 128;

/// Derives the control plane's stream key from an SDK [`SessionKey`].
///
/// The SDK's key is three free-form parts and the control plane's is one
/// path segment, so the parts are joined with `.` after every character
/// outside `[A-Za-z0-9._-]` is replaced. Replacing rather than dropping
/// keeps two keys that differ only in punctuation from colliding into one
/// stream.
///
/// # Errors
///
/// Returns [`StoreError::UnusableKeySegment`] if a part is empty, or if the
/// joined key is longer than the control plane accepts.
pub fn stream_key(key: &SessionKey) -> Result<String, StoreError> {
    fn part(field: &'static str, value: &str) -> Result<String, StoreError> {
        if value.is_empty() {
            return Err(StoreError::UnusableKeySegment {
                field,
                value: value.to_owned(),
            });
        }
        Ok(value
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                    character
                } else {
                    '_'
                }
            })
            .collect())
    }

    let mut stream = part("project_key", &key.project_key)?;
    stream.push('.');
    stream.push_str(&part("session_id", &key.session_id)?);
    if let Some(subpath) = &key.subpath {
        stream.push('.');
        stream.push_str(&part("subpath", subpath)?);
    }

    if stream.len() > MAX_STREAM_LEN {
        return Err(StoreError::UnusableKeySegment {
            field: "stream",
            value: stream,
        });
    }
    Ok(stream)
}

/// A [`TranscriptStore`] whose batches live in the control plane.
#[derive(Debug)]
pub struct RemoteTranscriptStore<A> {
    api: A,
    /// The sequence number each stream's next batch takes.
    ///
    /// Seeded from the control plane on a stream's first use, then advanced
    /// locally: this daemon is the only writer of its own session's
    /// transcript for as long as it is alive.
    next_seq: HashMap<String, u64>,
}

impl<A: ControlApi> RemoteTranscriptStore<A> {
    /// Opens a store against a control-plane client.
    #[must_use]
    pub fn new(api: A) -> Self {
        Self {
            api,
            next_seq: HashMap::new(),
        }
    }

    /// The sequence number `stream`'s next batch takes.
    async fn next_seq(&mut self, stream: &str) -> Result<u64, StoreError> {
        if let Some(seq) = self.next_seq.get(stream) {
            return Ok(*seq);
        }
        let read = self
            .api
            .get_transcript(stream)
            .await
            .map_err(|error| remote(&error))?;
        self.next_seq.insert(stream.to_owned(), read.batches);
        Ok(read.batches)
    }
}

/// A control-plane failure, in the shape the SDK's store contract speaks.
///
/// `StoreError` is an I/O vocabulary because M3a's store is a directory;
/// a network failure is the same *kind* of fact — the transcript could not
/// be written — so it arrives as one rather than widening the harness's
/// error surface for a second backend.
fn remote(error: &ControlApiError) -> StoreError {
    StoreError::Io {
        path: std::path::PathBuf::from("<control plane>"),
        source: std::io::Error::other(error.to_string()),
    }
}

impl<A: ControlApi> TranscriptStore for RemoteTranscriptStore<A> {
    async fn append(&mut self, key: &SessionKey, entries: Vec<Value>) -> Result<(), StoreError> {
        if entries.is_empty() {
            return Ok(());
        }

        let stream = stream_key(key)?;
        let seq = self.next_seq(&stream).await?;

        let mut body = Vec::new();
        for entry in &entries {
            let line = serde_json::to_vec(entry)
                .expect("a serde_json::Value always serializes back to JSON");
            body.extend_from_slice(&line);
            body.push(b'\n');
        }

        self.api
            .put_transcript_batch(&stream, seq, body)
            .await
            .map_err(|error| remote(&error))?;
        self.next_seq.insert(stream.clone(), seq + 1);

        tracing::debug!(
            stream,
            seq,
            entries = entries.len(),
            "mirrored a transcript batch to the control plane"
        );
        Ok(())
    }

    async fn load(&mut self, key: &SessionKey) -> Result<Vec<Value>, StoreError> {
        let stream = stream_key(key)?;
        let read = self
            .api
            .get_transcript(&stream)
            .await
            .map_err(|error| remote(&error))?;
        self.next_seq.insert(stream.clone(), read.batches);

        let text = String::from_utf8(read.body).map_err(|error| StoreError::Io {
            path: std::path::PathBuf::from(&stream),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, error),
        })?;

        let entries = text
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(index, line)| {
                serde_json::from_str(line).map_err(|source| StoreError::CorruptLine {
                    path: std::path::PathBuf::from(&stream),
                    line: index + 1,
                    source,
                })
            })
            .collect::<Result<Vec<Value>, StoreError>>()?;

        tracing::debug!(
            stream,
            batches = read.batches,
            loaded = entries.len(),
            "read a transcript stream from the control plane"
        );
        Ok(entries)
    }
}
