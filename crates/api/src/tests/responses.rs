//! Every exported operation says what it answers with.
//!
//! The document derives itself from the handlers now, so there is no table
//! to keep honest — but a derivation can still come out empty, and an
//! operation with no response schema ships as an untyped hole in the
//! generated TypeScript client. This is the assertion that stops that:
//! carrying a response body is the rule, and
//! [`BODILESS`](crate::responses::BODILESS) is the closed list of exceptions
//! with the reason each one cannot describe a body.
//!
//! It runs against the *checked-in* `openapi.json` as well as the live
//! router, because the client is generated from the file.

use std::collections::BTreeMap;

use crate::responses::BODILESS;

/// The document the export produces, as JSON.
fn document() -> serde_json::Value {
    serde_json::to_value(crate::openapi_document()).expect("the OpenAPI document serializes")
}

/// The checked-in artifact the TypeScript client is generated from.
fn checked_in() -> serde_json::Value {
    serde_json::from_str(include_str!("../../../../openapi.json"))
        .expect("the checked-in openapi.json is valid JSON")
}

/// Every operation of a document, by operation id.
fn operations(document: &serde_json::Value) -> BTreeMap<String, serde_json::Value> {
    let mut found = BTreeMap::new();
    for item in document["paths"]
        .as_object()
        .expect("paths is an object")
        .values()
    {
        for (verb, operation) in item.as_object().expect("a path item is an object") {
            if matches!(verb.as_str(), "summary" | "description" | "parameters") {
                continue;
            }
            let id = operation["operationId"]
                .as_str()
                .expect("every exported operation has an id")
                .to_owned();
            found.insert(id, operation.clone());
        }
    }
    found
}

/// Whether any of an operation's responses carries a body schema.
fn carries_content(operation: &serde_json::Value) -> bool {
    operation["responses"]
        .as_object()
        .expect("responses is an object")
        .values()
        .any(|response| response["content"].is_object())
}

#[cfg(test)]
mod tests {
    use super::{BODILESS, carries_content, checked_in, document, operations};

    #[test]
    fn the_list_of_bodiless_operations_is_sorted_and_names_each_once() {
        let mut sorted = BODILESS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            BODILESS, sorted,
            "`BODILESS` must be sorted by operation id"
        );
    }

    #[test]
    fn every_operation_either_carries_content_or_says_why_it_cannot() {
        for (id, operation) in operations(&document()) {
            let bodiless = BODILESS.contains(&id.as_str());
            assert_eq!(
                carries_content(&operation),
                !bodiless,
                "`{id}` {}",
                if bodiless {
                    "is listed as answering with no document, but carries one"
                } else {
                    "describes no response body; give its handler a return type that does, or \
                     add it to `flyco_api::responses::BODILESS` with the reason it cannot"
                }
            );
        }
    }

    #[test]
    fn the_checked_in_document_carries_the_same_response_bodies() {
        // The generated TypeScript client is built from the checked-in file,
        // not from the live router, so this is the assertion that decides
        // whether a client has types for the responses it receives.
        let live = operations(&document());
        let file = operations(&checked_in());

        for (id, operation) in &live {
            let stored = file.get(id).unwrap_or_else(|| {
                panic!(
                    "`{id}` is missing from the checked-in openapi.json; re-run \
                     `cargo run -p flyco-api --bin openapi > openapi.json`"
                )
            });
            assert_eq!(
                carries_content(stored),
                carries_content(operation),
                "`{id}` describes a different response in the checked-in openapi.json"
            );
        }
    }

    #[test]
    fn no_operation_is_listed_as_bodiless_without_being_exported() {
        let exported = operations(&document());
        for id in BODILESS {
            assert!(
                exported.contains_key(*id),
                "`{id}` is listed in `BODILESS` but is no longer exported; remove its row"
            );
        }
    }
}
