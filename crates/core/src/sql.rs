//! How the stored domain types map onto SQL columns.
//!
//! The control plane keeps its state in SQL, and the orphan rule puts this
//! here: [`FromColumn`] is skyzen's trait and these are flyco's types, so
//! flyco-api could not write the mapping even though it is the only crate
//! that uses it. The `sql` feature is what keeps that from costing `flycod`,
//! which shares this crate and opens no database, a web framework.
//!
//! Most of the mapping is `#[derive(skyzen::Column)]` on the type itself —
//! every enum stored as a text token, and [`Usd`](crate::Usd), which is one
//! integer column of microdollars. Two types cannot be derived and are
//! written out below:
//!
//! * [`Id<T>`] is generic, and the derive works on a concrete newtype.
//! * [`RepoSlug`] has an invariant. The newtype derive rebuilds the wrapper
//!   from the column without asking, which would let a corrupt row become a
//!   slug that never passed [`RepoSlug::from_str`].
//!
//! Both are text columns read back through their own [`FromStr`], so a value
//! the type would refuse is a decode failure rather than a value in the
//! wrong shape travelling on.

use core::fmt::Display;
use core::str::FromStr;

use serde_json::Value;
use skyzen_services::sql::{ColumnError, DbValue, FromColumn};

use crate::{Id, RepoSlug};

/// Reads a text column back through the type's own parser.
fn parse_text<T>(value: &Value, expected: &'static str) -> Result<T, ColumnError>
where
    T: FromStr,
    T::Err: Display,
{
    let text = String::from_column(value)?;
    T::from_str(&text).map_err(|error| ColumnError::invalid(expected, value, &error))
}

/// An id is stored as its hyphenated text form, which is what every `TEXT`
/// primary key in the schema holds — not as the sixteen bytes a `Uuid`
/// column would bind as on `SQLite`.
impl<T> From<Id<T>> for DbValue {
    fn from(id: Id<T>) -> Self {
        Self::Text(id.to_string())
    }
}

impl<T> FromColumn for Id<T> {
    fn from_column(value: &Value) -> Result<Self, ColumnError> {
        parse_text(value, "a UUID in its hyphenated text form")
    }
}

impl From<RepoSlug> for DbValue {
    fn from(slug: RepoSlug) -> Self {
        Self::Text(slug.as_str().to_owned())
    }
}

impl From<&RepoSlug> for DbValue {
    fn from(slug: &RepoSlug) -> Self {
        Self::Text(slug.as_str().to_owned())
    }
}

impl FromColumn for RepoSlug {
    fn from_column(value: &Value) -> Result<Self, ColumnError> {
        parse_text(value, "a GitHub repository in `owner/name` form")
    }
}
