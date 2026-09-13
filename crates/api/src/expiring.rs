//! Time-to-live that belongs to flyco *and* to the store.
//!
//! Every entry carries its own deadline, and a read past it is
//! indistinguishable from a miss — that is the deadline flyco states, and it
//! holds on any backend, whatever the store's own expiry can express.
//!
//! The store's expiry is set as well, because a logical deadline alone
//! removes nothing: single-use OAuth `state` entries would sit in the
//! namespace forever after the ten minutes they are readable for. Cloudflare
//! KV refuses an `expirationTtl` below sixty seconds, so a shorter life is
//! stored under [`STORE_TTL_FLOOR`] and the read-side check is what actually
//! enforces it. The two together mean an entry is unreadable at its real
//! deadline and gone from the store shortly after.

use core::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use skyzen_services::{Kv, KvError};

use crate::clock::now_unix;

/// The shortest expiry a store is asked for.
///
/// Cloudflare KV rejects anything below a minute, and the shortest-lived
/// entries flyco keeps there are seconds long. Clamping here rather than trusting each backend's own
/// rounding keeps one rule for every store flyco can run on: the entry is
/// readable for its logical life and physically present for at least a
/// minute.
const STORE_TTL_FLOOR: Duration = Duration::from_secs(60);

/// A stored value together with the moment it stops counting.
#[derive(Debug, Serialize, serde::Deserialize)]
struct Expiring<T> {
    /// Seconds since the Unix epoch after which the entry is a miss.
    expires_at_unix: u64,
    /// The payload.
    value: T,
}

/// How long the store is asked to keep an entry whose logical life is
/// `ttl_seconds`.
fn store_ttl(ttl_seconds: u64) -> Duration {
    Duration::from_secs(ttl_seconds).max(STORE_TTL_FLOOR)
}

/// Stores `value` under `key`, expiring `ttl_seconds` from now.
///
/// # Errors
///
/// Returns [`KvError`] if the value cannot be serialized or the store
/// rejects the write.
pub async fn put<T>(kv: &Kv, key: &str, value: &T, ttl_seconds: u64) -> Result<(), KvError>
where
    T: Serialize + Sync,
{
    let bytes = serde_json::to_vec(&Expiring {
        expires_at_unix: now_unix().saturating_add(ttl_seconds),
        value,
    })?;

    kv.put_with_ttl(key, &bytes, store_ttl(ttl_seconds)).await
}

/// Reads `key`, treating an expired entry as absent.
///
/// # Errors
///
/// Returns [`KvError`] if the store fails or the stored bytes do not
/// deserialize.
pub async fn get<T>(kv: &Kv, key: &str) -> Result<Option<T>, KvError>
where
    T: DeserializeOwned,
{
    let Some(entry) = kv.get_json::<Expiring<T>>(key).await? else {
        return Ok(None);
    };

    if entry.expires_at_unix <= now_unix() {
        return Ok(None);
    }
    Ok(Some(entry.value))
}

/// Reads `key` and deletes it, whether or not it had expired.
///
/// Single-use values — the OAuth `state` above all — must not survive the
/// read that consumed them, so the delete is unconditional.
///
/// # Errors
///
/// Returns [`KvError`] if the store fails or the stored bytes do not
/// deserialize.
pub async fn take<T>(kv: &Kv, key: &str) -> Result<Option<T>, KvError>
where
    T: DeserializeOwned,
{
    let value = get::<T>(kv, key).await;
    kv.delete(key).await?;
    value
}

#[cfg(test)]
mod tests {
    use core::future::{Future, ready};
    use core::time::Duration;
    use std::sync::mpsc::{Receiver, Sender, channel};

    use skyzen_services::Kv;
    use skyzen_services::kv::{KeyValueStore, KvError, KvListOptions, KvListResult};
    use skyzen_test::mock::InMemoryKv;

    use super::{STORE_TTL_FLOOR, get, put, store_ttl, take};

    /// An [`InMemoryKv`] that reports the expiry every write asked for.
    ///
    /// The mock honours a TTL but keeps no way to read one back, and "the
    /// entry is still there" cannot tell a sixty-second expiry from no
    /// expiry at all — which is exactly the bug this module had. So each
    /// write announces itself down a channel the test owns: `Some(ttl)` for
    /// an expiring write, `None` for one that would live forever.
    #[derive(Debug, Clone)]
    struct RecordingKv {
        inner: InMemoryKv,
        writes: Sender<Option<Duration>>,
    }

    impl RecordingKv {
        /// The store, plus the end of the channel its writes arrive on.
        fn recording_store() -> (Kv, Receiver<Option<Duration>>) {
            let (writes, recorded) = channel();
            (
                Kv::new(Self {
                    inner: InMemoryKv::new(),
                    writes,
                }),
                recorded,
            )
        }

        fn record(&self, ttl: Option<Duration>) {
            self.writes
                .send(ttl)
                .expect("the test still holds the receiver");
        }
    }

    impl KeyValueStore for RecordingKv {
        fn get(&self, key: &str) -> impl Future<Output = Result<Option<Vec<u8>>, KvError>> + Send {
            self.inner.get(key)
        }

        fn put(&self, key: &str, value: &[u8]) -> impl Future<Output = Result<(), KvError>> + Send {
            self.record(None);
            self.inner.put(key, value)
        }

        fn put_with_ttl(
            &self,
            key: &str,
            value: &[u8],
            ttl: Duration,
        ) -> impl Future<Output = Result<(), KvError>> + Send {
            self.record(Some(ttl));
            self.inner.put_with_ttl(key, value, ttl)
        }

        fn delete(&self, key: &str) -> impl Future<Output = Result<(), KvError>> + Send {
            self.inner.delete(key)
        }

        fn list(
            &self,
            options: KvListOptions,
        ) -> impl Future<Output = Result<KvListResult, KvError>> + Send {
            let _ = options;
            ready(Err(KvError::Unsupported(
                "the expiry tests never list the store",
            )))
        }
    }

    fn store() -> Kv {
        Kv::new(InMemoryKv::new())
    }

    #[skyzen::test]
    async fn a_live_entry_reads_back() {
        let kv = store();
        put(&kv, "k", &"v".to_owned(), 600).await.expect("put");
        assert_eq!(
            get::<String>(&kv, "k").await.expect("get").as_deref(),
            Some("v")
        );
    }

    #[skyzen::test]
    async fn an_expired_entry_reads_as_a_miss() {
        let kv = store();
        put(&kv, "k", &"v".to_owned(), 0).await.expect("put");
        assert!(get::<String>(&kv, "k").await.expect("get").is_none());
    }

    #[skyzen::test]
    async fn taking_an_entry_consumes_it() {
        let kv = store();
        put(&kv, "k", &"v".to_owned(), 600).await.expect("put");

        assert_eq!(
            take::<String>(&kv, "k").await.expect("take").as_deref(),
            Some("v")
        );
        assert!(take::<String>(&kv, "k").await.expect("take").is_none());
    }

    #[skyzen::test]
    async fn a_put_asks_the_store_to_expire_the_entry_too() {
        let (kv, recorded) = RecordingKv::recording_store();
        put(&kv, "auth:oauth-state:s", &(), 600).await.expect("put");

        assert_eq!(
            recorded.try_recv().expect("one write"),
            Some(Duration::from_secs(600)),
            "a logical deadline the store is never told about is a key that lives forever"
        );
    }

    #[skyzen::test]
    async fn a_life_shorter_than_the_stores_floor_is_stored_at_the_floor() {
        let (kv, recorded) = RecordingKv::recording_store();
        put(&kv, "oauth:state", &(), 5).await.expect("put");

        assert_eq!(
            recorded.try_recv().expect("one write"),
            Some(STORE_TTL_FLOOR)
        );
    }

    #[test]
    fn the_floor_only_ever_lengthens_a_life() {
        assert_eq!(store_ttl(0), STORE_TTL_FLOOR);
        assert_eq!(store_ttl(60), STORE_TTL_FLOOR);
        assert_eq!(store_ttl(600), Duration::from_secs(600));
    }

    #[skyzen::test]
    async fn an_entry_the_store_still_holds_is_a_miss_once_its_own_deadline_passes() {
        let kv = store();
        // Under the floor the store keeps this for a minute, so the logical
        // check is the only thing that makes it a miss.
        put(&kv, "k", &"v".to_owned(), 0).await.expect("put");

        assert!(kv.get("k").await.expect("raw get").is_some());
        assert!(get::<String>(&kv, "k").await.expect("get").is_none());
    }
}
