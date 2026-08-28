//! Web push, as the browser defines it.
//!
//! Flyco is a PWA first, so notifications go over the standard stack —
//! [RFC 8030] push, with [RFC 8292] VAPID identifying the application
//! server — rather than a vendor SDK. That has one consequence for these
//! types: [`PushSubscription`] is not flyco's shape to choose. It is exactly
//! what `PushSubscription.toJSON()` produces in the browser, down to the
//! `camelCase` `expirationTime`, so the SPA hands the object over verbatim
//! and nothing has to re-key it on the way.
//!
//! [RFC 8030]: https://www.rfc-editor.org/rfc/rfc8030
//! [RFC 8292]: https://www.rfc-editor.org/rfc/rfc8292

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::id::PushSubscriptionId;

/// Response of `GET /v1/push/vapid-public-key`.
///
/// The SPA passes this to `PushManager.subscribe` as
/// `applicationServerKey`; the matching private key never leaves the
/// control plane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct VapidPublicKey {
    /// The uncompressed P-256 public point, base64url without padding.
    pub key: String,
}

/// The two keys a browser derives for message encryption ([RFC 8291]).
///
/// [RFC 8291]: https://www.rfc-editor.org/rfc/rfc8291
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PushKeys {
    /// The client's P-256 public key, base64url without padding.
    pub p256dh: String,
    /// The client's authentication secret, base64url without padding.
    pub auth: String,
}

/// Request body of `POST /v1/push/subscriptions`: a browser
/// `PushSubscription`, serialized by the browser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PushSubscription {
    /// The push service endpoint this browser's messages are posted to.
    pub endpoint: String,
    /// When the subscription expires, in milliseconds since the Unix epoch,
    /// on the rare push service that sets one. Milliseconds rather than
    /// seconds because that is what the browser reports.
    pub expiration_time: Option<u64>,
    /// Encryption keys for this subscription.
    pub keys: PushKeys,
}

/// Response of `POST /v1/push/subscriptions`.
///
/// Carries the endpoint back but never the keys: they are write-only, and a
/// subscription is identified from here on by [`id`](Self::id).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PushSubscriptionView {
    /// Identifier used to remove the subscription.
    pub id: PushSubscriptionId,
    /// The push service endpoint it posts to.
    pub endpoint: String,
    /// When it was registered, seconds since the Unix epoch.
    pub created_at_unix: u64,
}

#[cfg(test)]
mod tests {
    use super::PushSubscription;

    #[test]
    fn a_browser_subscription_deserializes_verbatim() {
        // Exactly the document `PushSubscription.toJSON()` produces.
        let raw = r#"{"endpoint":"https://fcm.googleapis.com/fcm/send/abc","expirationTime":null,"keys":{"p256dh":"BOr","auth":"k1g"}}"#;

        let subscription: PushSubscription = serde_json::from_str(raw).expect("deserialize");
        assert_eq!(subscription.expiration_time, None);
        assert_eq!(subscription.keys.auth, "k1g");

        let json = serde_json::to_value(&subscription).expect("serialize");
        assert!(
            json.get("expirationTime").is_some(),
            "the field keeps the browser's own spelling"
        );
    }
}
