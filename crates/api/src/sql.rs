//! Mapping domain types onto SQL columns.
//!
//! Two conversions recur across every table and both are easy to get
//! subtly wrong, so they live here once.
//!
//! * **Enums as text.** A domain enum is stored as the `snake_case` token its
//!   serde representation already uses, rather than a hand-written match
//!   that could drift from the wire format. The `CHECK` constraints in the
//!   migrations spell out the same tokens.
//! * **Timestamps and amounts as `i64`.** SQLite and D1 have one integer
//!   type and it is signed, so every unsigned domain value is narrowed on
//!   the way in and widened on the way out.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::ApiError;

/// Encodes a domain enum as the text token its column stores.
///
/// # Errors
///
/// Returns [`ApiError::CorruptRecord`] if the value does not serialize to a
/// plain string — which would mean it is not a unit-variant enum.
pub fn encode_enum<T: Serialize>(value: &T) -> Result<String, ApiError> {
    match serde_json::to_value(value) {
        Ok(Value::String(token)) => Ok(token),
        _ => Err(ApiError::CorruptRecord(
            "only unit-variant enums are stored as text columns",
        )),
    }
}

/// Decodes the text token a column stores back into its domain enum.
///
/// # Errors
///
/// Returns [`ApiError::CorruptRecord`] if the stored token is not one the
/// domain enum knows — a value that no longer matches its `CHECK`.
pub fn decode_enum<T: DeserializeOwned>(token: &str, column: &'static str) -> Result<T, ApiError> {
    serde_json::from_value(Value::String(token.to_owned()))
        .map_err(|_| ApiError::CorruptRecord(column))
}

/// Narrows an unsigned domain value into the signed integer a column holds.
///
/// Saturating rather than fallible: `u64::MAX` microdollars is far beyond
/// any budget, and a clamp keeps the write path infallible.
#[must_use]
pub fn to_column(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Widens a stored integer back into its unsigned domain value.
///
/// # Errors
///
/// Returns [`ApiError::CorruptRecord`] if the column is negative, which no
/// write path can produce.
pub fn from_column(value: i64, column: &'static str) -> Result<u64, ApiError> {
    u64::try_from(value).map_err(|_| ApiError::CorruptRecord(column))
}

#[cfg(test)]
mod tests {
    use flyco_core::{ApprovalState, HarnessKind, SessionState};

    use super::{decode_enum, encode_enum, from_column, to_column};

    #[test]
    fn enums_round_trip_through_their_stored_token() {
        assert_eq!(
            encode_enum(&HarnessKind::ClaudeCode).expect("encode"),
            "claude_code"
        );
        assert_eq!(
            decode_enum::<SessionState>("archived", "sessions.state").expect("decode"),
            SessionState::Archived
        );
        assert_eq!(
            decode_enum::<ApprovalState>("pending", "approvals.state").expect("decode"),
            ApprovalState::Pending
        );
    }

    #[test]
    fn an_unknown_token_is_a_corrupt_record() {
        assert!(decode_enum::<SessionState>("zombie", "sessions.state").is_err());
    }

    #[test]
    fn a_negative_column_is_a_corrupt_record() {
        assert_eq!(from_column(7, "x").expect("widen"), 7);
        assert!(from_column(-1, "x").is_err());
        assert_eq!(to_column(7), 7);
    }
}
