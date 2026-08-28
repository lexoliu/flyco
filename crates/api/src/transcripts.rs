//! Session transcripts in object storage.
//!
//! The Agent SDK's `SessionStore` is what makes flyco's History feature
//! possible — a session resumes onto any machine because the transcript
//! lives in the control plane rather than on the VM. The daemon mirrors
//! every batch of entries here as it produces them, and reads the whole
//! stream back when a session resumes somewhere else.
//!
//! # Why batches, and why not the WebSocket
//!
//! Cloudflare caps a WebSocket frame at 1 MiB and a transcript is
//! unbounded, so bulk transcript data never rides the relay. It is written
//! with an ordinary authenticated `PUT`, one object per batch:
//!
//! ```text
//! transcripts/{session}/{stream}/{seq:08}.jsonl
//! ```
//!
//! Zero-padding the sequence number to eight digits makes lexicographic
//! order — the only order object storage offers — equal to numeric order,
//! so a read is a prefix list with no sorting and no index to keep
//! consistent. A batch is immutable once written: the same `seq` written
//! twice is a daemon bug, and overwriting it would silently reorder the
//! transcript, so the second write is refused.

use skyzen_services::{ListOptions, Storage};

use crate::error::ApiError;

/// Prefix every transcript object lives under.
const ROOT: &str = "transcripts";

/// Media type of a transcript stream: newline-delimited JSON.
pub const CONTENT_TYPE: &str = "application/x-ndjson";

/// Header naming how many batches a read concatenated.
///
/// A resuming daemon numbers its next `PUT` from this, so batches keep
/// increasing across hosts instead of restarting at zero and colliding with
/// what the previous host already wrote.
pub const BATCH_COUNT_HEADER: &str = "x-flyco-transcript-batches";

/// Widest sequence number a batch key can express.
///
/// Eight digits is 100 million batches for one stream; a session that
/// produced that many has a runaway daemon, not a long transcript.
pub const MAX_BATCH_SEQ: u64 = 99_999_999;

/// Whether `stream` is usable as one path segment of a transcript key.
///
/// The stream key is derived by the daemon from harness-supplied values
/// (project key, session id, sub-stream path), so this is a containment
/// check rather than a formatting nicety: anything else could address an
/// object outside the session's own prefix.
/// `.` is in the accepted alphabet because the daemon composes a stream key
/// out of dotted parts, but a key that is *only* dots is the relative-path
/// syntax every object store still resolves, so those two are named out.
#[must_use]
pub fn is_valid_stream(stream: &str) -> bool {
    !stream.is_empty()
        && stream.len() <= 128
        && stream != "."
        && stream != ".."
        && stream
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// The object key one batch is stored under.
fn batch_key(session: &str, stream: &str, seq: u64) -> String {
    format!("{ROOT}/{session}/{stream}/{seq:08}.jsonl")
}

/// The prefix every batch of one stream shares.
fn stream_prefix(session: &str, stream: &str) -> String {
    format!("{ROOT}/{session}/{stream}/")
}

/// Stores one batch of JSONL transcript bytes.
///
/// # Errors
///
/// Returns [`ApiError::InvalidStreamKey`] if `stream` is not a usable path
/// segment, [`ApiError::BatchSeqOutOfRange`] if `seq` is too large,
/// [`ApiError::BatchAlreadyStored`] if that sequence number was already
/// written, or [`ApiError::Storage`] if the store fails.
pub async fn put_batch(
    storage: &Storage,
    session: flyco_core::SessionId,
    stream: &str,
    seq: u64,
    body: Vec<u8>,
) -> Result<(), ApiError> {
    if !is_valid_stream(stream) {
        return Err(ApiError::InvalidStreamKey(stream.to_owned()));
    }
    if seq > MAX_BATCH_SEQ {
        return Err(ApiError::BatchSeqOutOfRange { seq });
    }

    let key = batch_key(&session.to_string(), stream, seq);
    if storage.head(&key).await?.is_some() {
        return Err(ApiError::BatchAlreadyStored { seq });
    }

    storage.put(&key, body).await?;
    tracing::debug!(%session, stream, seq, "stored a transcript batch");
    Ok(())
}

/// A stream read back from storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stream {
    /// Every batch concatenated, in sequence order.
    pub body: Vec<u8>,
    /// How many batches were concatenated.
    ///
    /// A resuming daemon numbers its next `PUT` from here, so batches keep
    /// increasing across hosts instead of restarting at zero and colliding.
    pub batches: usize,
}

/// Reads every batch of one stream back, concatenated in sequence order.
///
/// A stream that was never written is empty rather than missing: that is
/// the state of every session before its first turn.
///
/// # Errors
///
/// Returns [`ApiError::InvalidStreamKey`] if `stream` is not a usable path
/// segment, or [`ApiError::Storage`] if the store fails.
pub async fn read_stream(
    storage: &Storage,
    session: flyco_core::SessionId,
    stream: &str,
) -> Result<Stream, ApiError> {
    if !is_valid_stream(stream) {
        return Err(ApiError::InvalidStreamKey(stream.to_owned()));
    }

    let prefix = stream_prefix(&session.to_string(), stream);
    let mut keys = Vec::new();
    let mut cursor = None;
    loop {
        let page = storage
            .list(ListOptions {
                prefix: Some(prefix.clone()),
                limit: None,
                cursor: cursor.take(),
            })
            .await?;
        keys.extend(page.objects.into_iter().map(|object| object.key));
        match page.cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }

    // Zero-padded sequence numbers make byte order sequence order, so this
    // sort is the whole of the ordering guarantee.
    keys.sort_unstable();

    let mut body = Vec::new();
    for key in &keys {
        let object = storage.get(key).await?.ok_or(ApiError::CorruptRecord(
            "a listed transcript batch vanished",
        ))?;
        body.extend_from_slice(&object.body);
    }

    tracing::debug!(%session, stream, batches = keys.len(), "read a transcript stream");
    Ok(Stream {
        batches: keys.len(),
        body,
    })
}

#[cfg(test)]
mod tests {
    use flyco_core::SessionId;
    use skyzen_services::Storage;

    use super::{Stream, is_valid_stream, put_batch, read_stream};
    use crate::error::ApiError;

    fn store() -> Storage {
        Storage::new(skyzen_test::mock::InMemoryStorage::new())
    }

    #[skyzen::test]
    async fn batches_read_back_concatenated_in_sequence_order() {
        let storage = store();
        let session = SessionId::generate();

        // Written out of order on purpose: order is the key's job, not the
        // caller's.
        put_batch(&storage, session, "main", 2, b"{\"n\":2}\n".to_vec())
            .await
            .expect("put 2");
        put_batch(&storage, session, "main", 0, b"{\"n\":0}\n".to_vec())
            .await
            .expect("put 0");
        put_batch(&storage, session, "main", 1, b"{\"n\":1}\n".to_vec())
            .await
            .expect("put 1");

        assert_eq!(
            read_stream(&storage, session, "main").await.expect("read"),
            Stream {
                body: b"{\"n\":0}\n{\"n\":1}\n{\"n\":2}\n".to_vec(),
                batches: 3,
            }
        );
    }

    #[skyzen::test]
    async fn a_stream_that_was_never_written_reads_empty() {
        let storage = store();
        assert_eq!(
            read_stream(&storage, SessionId::generate(), "main")
                .await
                .expect("read"),
            Stream {
                body: Vec::new(),
                batches: 0,
            }
        );
    }

    #[skyzen::test]
    async fn streams_and_sessions_do_not_bleed_into_each_other() {
        let storage = store();
        let (first, second) = (SessionId::generate(), SessionId::generate());

        put_batch(&storage, first, "main", 0, b"a".to_vec())
            .await
            .expect("put");
        put_batch(&storage, first, "sub", 0, b"b".to_vec())
            .await
            .expect("put");
        put_batch(&storage, second, "main", 0, b"c".to_vec())
            .await
            .expect("put");

        assert_eq!(
            read_stream(&storage, first, "main")
                .await
                .expect("read")
                .body,
            b"a"
        );
        assert_eq!(
            read_stream(&storage, first, "sub")
                .await
                .expect("read")
                .body,
            b"b"
        );
        assert_eq!(
            read_stream(&storage, second, "main")
                .await
                .expect("read")
                .body,
            b"c"
        );
    }

    #[skyzen::test]
    async fn rewriting_a_batch_is_refused_rather_than_reordering_the_transcript() {
        let storage = store();
        let session = SessionId::generate();
        put_batch(&storage, session, "main", 0, b"first".to_vec())
            .await
            .expect("put");

        assert!(matches!(
            put_batch(&storage, session, "main", 0, b"second".to_vec()).await,
            Err(ApiError::BatchAlreadyStored { seq: 0 })
        ));
        assert_eq!(
            read_stream(&storage, session, "main")
                .await
                .expect("read")
                .body,
            b"first"
        );
    }

    #[skyzen::test]
    async fn a_stream_key_that_could_escape_its_prefix_is_refused() {
        let storage = store();
        let session = SessionId::generate();
        for stream in ["", "..", "a/b", "a\\b", "a b", "a\0b", "sub/../../etc"] {
            assert!(!is_valid_stream(stream), "{stream:?} must be refused");
            assert!(matches!(
                put_batch(&storage, session, stream, 0, b"x".to_vec()).await,
                Err(ApiError::InvalidStreamKey(_))
            ));
            assert!(matches!(
                read_stream(&storage, session, stream).await,
                Err(ApiError::InvalidStreamKey(_))
            ));
        }
    }

    #[test]
    fn the_stream_keys_the_daemon_derives_are_accepted() {
        for stream in ["main", "flyco-work.9d0f4b1a", "sub_entries", "a.b-c_d.0"] {
            assert!(is_valid_stream(stream), "{stream:?} must be accepted");
        }
        assert!(!is_valid_stream(&"x".repeat(129)));
    }

    #[skyzen::test]
    async fn a_sequence_number_beyond_the_key_width_is_refused() {
        let storage = store();
        assert!(matches!(
            put_batch(
                &storage,
                SessionId::generate(),
                "main",
                super::MAX_BATCH_SEQ + 1,
                b"x".to_vec()
            )
            .await,
            Err(ApiError::BatchSeqOutOfRange { .. })
        ));
    }
}
