//! RFC 9457 problem details — the single shape every control-plane error takes.
//!
//! Clients never have to guess at flyco's error format: a failed request
//! answers with `application/problem+json` and this document, whatever went
//! wrong and wherever in the stack it went wrong.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Media type RFC 9457 registers for these documents.
pub const CONTENT_TYPE: &str = "application/problem+json";

/// Base URI under which flyco documents its own problem types.
pub const TYPE_BASE: &str = "https://flyco.dev/problems/";

/// The type URI RFC 9457 reserves for "the status code says it all".
pub const ABOUT_BLANK: &str = "about:blank";

/// An RFC 9457 problem detail document.
///
/// `instance` is deliberately absent: flyco has no per-occurrence URI to
/// point at yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Problem {
    /// URI identifying the problem type, or [`ABOUT_BLANK`] when the status
    /// code carries all the meaning there is.
    #[serde(rename = "type")]
    pub kind: String,
    /// Short, human-readable summary of the problem type.
    pub title: String,
    /// The HTTP status code, repeated in the body as RFC 9457 advises.
    pub status: u16,
    /// Human-readable explanation of this particular occurrence.
    pub detail: String,
}

impl Problem {
    /// A problem whose type adds nothing to its status code.
    #[must_use]
    pub fn about_blank(status: u16, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            kind: ABOUT_BLANK.to_owned(),
            title: title.into(),
            status,
            detail: detail.into(),
        }
    }

    /// A flyco problem type, named by its slug under [`TYPE_BASE`].
    #[must_use]
    pub fn of_type(
        slug: &str,
        status: u16,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        let mut kind = String::with_capacity(TYPE_BASE.len() + slug.len());
        kind.push_str(TYPE_BASE);
        kind.push_str(slug);

        Self {
            kind,
            title: title.into(),
            status,
            detail: detail.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ABOUT_BLANK, Problem};

    #[test]
    fn a_typed_problem_names_its_slug_under_the_flyco_namespace() {
        let problem = Problem::of_type("invalid-credential", 401, "Unauthorized", "nope");
        assert_eq!(
            problem.kind,
            "https://flyco.dev/problems/invalid-credential"
        );
    }

    #[test]
    fn the_type_field_is_serialized_as_type() {
        let json = serde_json::to_value(Problem::about_blank(400, "Bad Request", "nope"))
            .expect("serialize");
        assert_eq!(json["type"], ABOUT_BLANK);
        assert_eq!(json["status"], 400);
        assert!(json.get("instance").is_none());
    }
}
