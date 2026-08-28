//! Time-to-live on top of skyzen 0.1.2's `Kv`.
//!
//! The portable `KeyValueStore` trait has no expiry parameter, so every
//! entry carries its own deadline and a read past that deadline is
//! indistinguishable from a miss. Cloudflare KV's native TTL would evict the
//! bytes as well, but nothing may depend on that: correctness lives here.

use serde::Serialize;
use serde::de::DeserializeOwned;
use skyzen_services::{Kv, KvError};

use crate::clock::now_unix;

/// A stored value together with the moment it stops counting.
#[derive(Debug, Serialize, serde::Deserialize)]
struct Expiring<T> {
    /// Seconds since the Unix epoch after which the entry is a miss.
    expires_at_unix: u64,
    /// The payload.
    value: T,
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
    kv.put_json(
        key,
        &Expiring {
            expires_at_unix: now_unix().saturating_add(ttl_seconds),
            value,
        },
    )
    .await
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
    use skyzen_services::Kv;
    use skyzen_test::mock::InMemoryKv;

    use super::{get, put, take};

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
}
