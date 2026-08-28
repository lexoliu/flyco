//! Transcript persistence behind the Agent SDK's `SessionStore`.
//!
//! The SDK asks its store to mirror every transcript entry and to read a
//! stream back when a session resumes on a different host. flycod answers
//! those calls itself so the transcript is flyco's, not the VM's — which is
//! what makes History (resume onto any machine) possible at all.
//!
//! M3a ships [`JsonlTranscriptStore`], the local append-only implementation
//! used for development and as the on-VM write-behind buffer. The control
//! plane's R2-backed store lands in M3b as a second implementation of
//! [`TranscriptStore`].

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tokio::io::AsyncWriteExt as _;

use super::protocol::SessionKey;

/// A transcript stream could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The filesystem refused an operation.
    #[error("transcript store I/O failed at {path}")]
    Io {
        /// The path being read or written.
        path: PathBuf,
        /// The underlying cause.
        #[source]
        source: std::io::Error,
    },
    /// A [`SessionKey`] component could not be used as a path segment.
    ///
    /// The key comes from the harness, so this is a containment check, not
    /// a formatting nicety: a `..` in a project key would otherwise write
    /// outside the store root.
    #[error("{field} {value:?} is not a usable transcript path segment")]
    UnusableKeySegment {
        /// Which part of the key was rejected.
        field: &'static str,
        /// The offending value.
        value: String,
    },
    /// A stored line is not valid JSON.
    #[error("transcript line {line} of {path} is not valid JSON")]
    CorruptLine {
        /// The file being read.
        path: PathBuf,
        /// 1-based line number.
        line: usize,
        /// The parse failure.
        #[source]
        source: serde_json::Error,
    },
}

/// Where flyco keeps a session's transcript.
///
/// Implementations are owned outright by the harness driver's actor task —
/// there is exactly one writer per session, so no locking is involved.
pub trait TranscriptStore: Send + 'static {
    /// Mirrors a batch of raw SDK entries, skipping any whose `uuid` this
    /// store already holds.
    ///
    /// `uuid` is an idempotency key, not a requirement: the SDK documents
    /// that some entries (titles, tags, mode markers) carry none, and those
    /// are appended unconditionally.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the key is unusable or the write fails.
    fn append(
        &mut self,
        key: &SessionKey,
        entries: Vec<Value>,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Reads a stream back in append order.
    ///
    /// A stream that was never appended to is empty, not missing: that is
    /// the state of every session before its first turn.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the key is unusable or the stored data is
    /// unreadable.
    fn load(
        &mut self,
        key: &SessionKey,
    ) -> impl Future<Output = Result<Vec<Value>, StoreError>> + Send;
}

/// Rejects anything that cannot stand alone as one path component.
fn segment(field: &'static str, value: &str) -> Result<(), StoreError> {
    let usable =
        !value.is_empty() && value != "." && value != ".." && !value.contains(['/', '\\', '\0']);
    if usable {
        Ok(())
    } else {
        Err(StoreError::UnusableKeySegment {
            field,
            value: value.to_owned(),
        })
    }
}

/// The `uuid` a transcript entry deduplicates on, if it has one.
///
/// The SDK's `SessionStore` contract is explicit that most entries carry a
/// `uuid` and some (titles, tags, mode markers) do not; the ones that do
/// not are appended every time they arrive.
fn entry_uuid(entry: &Value) -> Option<&str> {
    entry.get("uuid").and_then(Value::as_str)
}

/// An append-only JSONL transcript store rooted at one directory.
///
/// Layout, one file per SDK session stream:
///
/// ```text
/// <root>/<project_key>/<session_id>/entries.jsonl        # subpath: None
/// <root>/<project_key>/<session_id>/sub/<subpath>.jsonl  # subpath: Some
/// ```
///
/// Sub-streams live under their own directory so a subpath can never
/// collide with the session's own stream, whatever it is named.
#[derive(Debug)]
pub struct JsonlTranscriptStore {
    root: PathBuf,
    seen: HashMap<PathBuf, HashSet<String>>,
}

impl JsonlTranscriptStore {
    /// Opens a store rooted at `root`. Directories are created on first
    /// write, so the root need not exist yet.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            seen: HashMap::new(),
        }
    }

    /// The file backing one session stream.
    fn path_for(&self, key: &SessionKey) -> Result<PathBuf, StoreError> {
        segment("project_key", &key.project_key)?;
        segment("session_id", &key.session_id)?;
        let mut path = self.root.join(&key.project_key).join(&key.session_id);
        match &key.subpath {
            Some(subpath) => {
                segment("subpath", subpath)?;
                path.push("sub");
                path.push(format!("{subpath}.jsonl"));
            }
            None => path.push("entries.jsonl"),
        }
        Ok(path)
    }

    /// Reads one stream file, or an empty stream if it was never written.
    async fn read(path: &Path) -> Result<Vec<Value>, StoreError> {
        let text = match tokio::fs::read_to_string(path).await {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(StoreError::Io {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        text.lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(index, line)| {
                serde_json::from_str(line).map_err(|source| StoreError::CorruptLine {
                    path: path.to_owned(),
                    line: index + 1,
                    source,
                })
            })
            .collect()
    }

    /// The uuids already stored for `path`, reading the file once.
    async fn seen_uuids(&mut self, path: &Path) -> Result<&mut HashSet<String>, StoreError> {
        if !self.seen.contains_key(path) {
            let existing = Self::read(path).await?;
            let uuids = existing
                .iter()
                .filter_map(|entry| entry_uuid(entry).map(str::to_owned))
                .collect::<HashSet<String>>();
            self.seen.insert(path.to_owned(), uuids);
        }
        Ok(self.seen.entry(path.to_owned()).or_default())
    }
}

impl TranscriptStore for JsonlTranscriptStore {
    async fn append(&mut self, key: &SessionKey, entries: Vec<Value>) -> Result<(), StoreError> {
        let path = self.path_for(key)?;

        let known = self.seen_uuids(&path).await?;
        let fresh: Vec<(Option<String>, &Value)> = entries
            .iter()
            .map(|entry| (entry_uuid(entry).map(str::to_owned), entry))
            .filter(|(uuid, _)| uuid.as_ref().is_none_or(|uuid| !known.contains(uuid)))
            .collect();
        if fresh.is_empty() {
            tracing::debug!(?path, "every entry in this batch was already mirrored");
            return Ok(());
        }

        let mut buffer = String::new();
        for (_, entry) in &fresh {
            let line = serde_json::to_string(entry)
                .expect("a serde_json::Value always serializes back to JSON");
            buffer.push_str(&line);
            buffer.push('\n');
        }

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|source| StoreError::Io {
                    path: parent.to_owned(),
                    source,
                })?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|source| StoreError::Io {
                path: path.clone(),
                source,
            })?;
        file.write_all(buffer.as_bytes())
            .await
            .map_err(|source| StoreError::Io {
                path: path.clone(),
                source,
            })?;
        file.flush().await.map_err(|source| StoreError::Io {
            path: path.clone(),
            source,
        })?;

        let written = fresh.len();
        let known = self.seen_uuids(&path).await?;
        for (uuid, _) in fresh {
            if let Some(uuid) = uuid {
                known.insert(uuid);
            }
        }
        tracing::debug!(?path, written, "mirrored transcript entries");
        Ok(())
    }

    async fn load(&mut self, key: &SessionKey) -> Result<Vec<Value>, StoreError> {
        let path = self.path_for(key)?;
        let entries = Self::read(&path).await?;
        tracing::debug!(?path, loaded = entries.len(), "read a transcript stream");
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::{JsonlTranscriptStore, SessionKey, StoreError, TranscriptStore};
    use serde_json::{Value, json};

    /// A stream with nothing in it.
    const NO_ENTRIES: [Value; 0] = [];

    /// A store rooted in a fresh directory under the test target dir.
    fn store(name: &str) -> (JsonlTranscriptStore, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("flycod-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        (JsonlTranscriptStore::new(root.clone()), root)
    }

    fn key(subpath: Option<&str>) -> SessionKey {
        SessionKey {
            project_key: "flyco-work".to_owned(),
            session_id: "9d0f4b1a".to_owned(),
            subpath: subpath.map(str::to_owned),
        }
    }

    #[tokio::test]
    async fn an_unwritten_stream_loads_empty() {
        let (mut store, root) = store("empty");
        assert_eq!(store.load(&key(None)).await.expect("load"), NO_ENTRIES);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn appends_accumulate_and_deduplicate_on_uuid() {
        let (mut store, root) = store("dedupe");
        let first = json!({ "uuid": "a", "type": "user" });
        let second = json!({ "uuid": "b", "type": "assistant" });

        store
            .append(&key(None), vec![first.clone(), second.clone()])
            .await
            .expect("append");
        // The SDK re-sends batches it is unsure about; the store must not
        // grow.
        store
            .append(&key(None), vec![second.clone(), first.clone()])
            .await
            .expect("re-append");
        assert_eq!(
            store.load(&key(None)).await.expect("load"),
            vec![first, second]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn dedupe_survives_a_fresh_store_over_the_same_directory() {
        let (mut store, root) = store("reopen");
        let entry = json!({ "uuid": "a", "type": "user" });
        store
            .append(&key(None), vec![entry.clone()])
            .await
            .expect("append");

        let mut reopened = JsonlTranscriptStore::new(root.clone());
        reopened
            .append(&key(None), vec![entry.clone()])
            .await
            .expect("append");
        assert_eq!(reopened.load(&key(None)).await.expect("load"), vec![entry]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sub_streams_are_separate_from_the_session_stream() {
        let (mut store, root) = store("subpath");
        let main = json!({ "uuid": "a", "where": "main" });
        let sub = json!({ "uuid": "a", "where": "sub" });
        store
            .append(&key(None), vec![main.clone()])
            .await
            .expect("append main");
        store
            .append(&key(Some("entries")), vec![sub.clone()])
            .await
            .expect("append sub");

        assert_eq!(store.load(&key(None)).await.expect("load"), vec![main]);
        assert_eq!(
            store.load(&key(Some("entries"))).await.expect("load"),
            vec![sub]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_traversing_key_is_refused_before_any_write() {
        let (mut store, root) = store("traversal");
        let escaping = SessionKey {
            project_key: "..".to_owned(),
            session_id: "9d0f4b1a".to_owned(),
            subpath: None,
        };
        assert!(matches!(
            store.append(&escaping, vec![json!({ "uuid": "a" })]).await,
            Err(StoreError::UnusableKeySegment {
                field: "project_key",
                ..
            })
        ));
        let nested = SessionKey {
            project_key: "flyco".to_owned(),
            session_id: "a/b".to_owned(),
            subpath: None,
        };
        assert!(matches!(
            store.load(&nested).await,
            Err(StoreError::UnusableKeySegment {
                field: "session_id",
                ..
            })
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn entries_without_a_uuid_are_appended_every_time() {
        // The SDK's SessionStore contract: titles, tags and mode markers
        // carry no uuid and must be stored without dedup.
        let (mut store, root) = store("no-uuid");
        let deduped = json!({ "uuid": "a", "type": "user" });
        let marker = json!({ "type": "mode_marker", "mode": "plan" });

        store
            .append(&key(None), vec![deduped.clone(), marker.clone()])
            .await
            .expect("append");
        store
            .append(&key(None), vec![deduped.clone(), marker.clone()])
            .await
            .expect("re-append");

        assert_eq!(
            store.load(&key(None)).await.expect("load"),
            vec![deduped, marker.clone(), marker]
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
