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

/// The extension members RFC 9457 §3.2 lets a problem document carry.
///
/// A refusal often knows a number the client wants to act on, and prose is
/// the wrong place to keep it: a sentence is written for a person, and a
/// client that reads one back out is parsing English to find an integer.
/// So every such fact is a member of its own, typed here and flattened onto
/// the document beside `type`, `title`, `status` and `detail`.
///
/// One struct rather than a map of `serde_json::Value`: which members exist
/// is a fact about this API, the generated `OpenAPI` document states each of
/// them with its type, and a member that is dropped or renamed fails the
/// build instead of quietly disappearing from the wire.
///
/// Every member is optional, because each belongs to the one problem type
/// that defines it and is absent everywhere else — RFC 9457's rule that a
/// consumer must ignore extensions it does not recognise.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProblemExtensions {
    /// How many sessions are still running, on
    /// `problems/host-has-active-sessions`.
    ///
    /// The count the refusal is *about*: removing a host stops the work on
    /// it, so the confirmation the user is shown has to state how much work
    /// that is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_sessions: Option<u32>,
}

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
    /// The typed facts this problem type carries beyond its prose.
    ///
    /// Flattened, because RFC 9457 §3.2 puts extension members at the top
    /// level of the document rather than under an object of their own.
    #[serde(flatten)]
    pub extensions: ProblemExtensions,
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
            extensions: ProblemExtensions::default(),
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
            extensions: ProblemExtensions::default(),
        }
    }

    /// The same document, carrying the typed facts its type defines.
    #[must_use]
    pub const fn with_extensions(mut self, extensions: ProblemExtensions) -> Self {
        self.extensions = extensions;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::{ABOUT_BLANK, Problem, ProblemExtensions};

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

    #[test]
    fn an_extension_member_sits_beside_the_documents_own_fields() {
        let problem = Problem::of_type("host-has-active-sessions", 409, "Conflict", "3 of them")
            .with_extensions(ProblemExtensions {
                active_sessions: Some(3),
            });
        let json = serde_json::to_value(&problem).expect("serialize");

        // RFC 9457 §3.2: an extension is a member of the document, not a
        // field of an object nested inside it.
        assert_eq!(json["active_sessions"], 3);
        assert!(json.get("extensions").is_none());

        let decoded: Problem = serde_json::from_value(json).expect("deserialize");
        assert_eq!(decoded, problem);
    }

    #[test]
    fn a_problem_that_defines_no_extension_carries_none() {
        let json = serde_json::to_value(Problem::of_type("host-not-found", 404, "Not Found", "no"))
            .expect("serialize");

        // Absent rather than `null`: a consumer ignores members it does not
        // know, and a null would be a value it has to know to ignore.
        assert!(json.get("active_sessions").is_none());
    }
}
