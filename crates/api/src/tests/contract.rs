//! The frozen REST contract, and the work still owed behind it.
//!
//! Milestone M2c locked the whole API surface at once: every route flyco will
//! serve exists in the router, is typed in `flyco_core`, and appears in the
//! exported `OpenAPI` document, so the frontend can generate a client and
//! build against a contract that is not going to move while the handler
//! bodies are filled in behind it.
//!
//! That trade only works if the debt stays visible, which is what this module
//! is for. [`unimplemented_handlers`] reads the crate's own sources and finds
//! every handler whose body is still a `todo!("M…: …")`, and
//! [`tests::the_unimplemented_handlers_are_exactly_the_expected_set`] pins the
//! result against [`EXPECTED`]. The list is therefore a ratchet: a milestone
//! that implements a handler must delete its row, and a route that quietly
//! regresses into a `todo!()` cannot be added without one. It can only shrink.
//!
//! Every entry names the milestone that owes it, taken from the `todo!`
//! message itself rather than from a second list that could disagree.

use std::path::{Path, PathBuf};

/// One handler that is routed and typed but not yet implemented.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Pending {
    /// The handler function's name.
    handler: String,
    /// Milestone named by its `todo!` message.
    milestone: String,
}

/// Every handler still owed, and by which milestone.
///
/// Sorted by handler name, which is how [`unimplemented_handlers`] returns
/// them, so a diff on this list reads as one line per handler.
const EXPECTED: &[(&str, &str)] = &[
    ("cloud_usage", "M6"),
    ("complete_harness_link", "M4"),
    ("delete_skill", "M6"),
    ("get_skill", "M6"),
    ("list_skills", "M6"),
    ("llm_usage", "M6"),
    ("receive_github_webhook", "M6"),
    ("resume_session", "M4"),
    ("start_harness_link", "M4"),
    ("upload_skill", "M6"),
];

/// Marks the start of a handler.
const FN_MARKER: &str = "async fn ";

/// Marks an unimplemented body.
const TODO_MARKER: &str = "todo!(";

/// Longest a milestone tag may be, which is what keeps this scanner from
/// mistaking an unrelated `todo!()` in prose for an owed handler.
const MILESTONE_MAX: usize = 4;

/// Every unimplemented handler in the crate, sorted by name.
fn unimplemented_handlers() -> Vec<Pending> {
    let mut found: Vec<Pending> = handler_sources()
        .iter()
        .flat_map(|source| pending_in(source))
        .collect();
    found.sort();
    found
}

/// The crate's handler sources: everything under `src`, except this test
/// tree, which is not where handlers live and whose own text mentions the
/// markers below.
fn handler_sources() -> Vec<String> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut sources = Vec::new();
    collect(&root, &root.join("tests"), &mut sources);
    sources
}

fn collect(directory: &Path, skip: &Path, sources: &mut Vec<String>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
        .map(|entry| entry.expect("a readable directory entry").path())
        .filter(|path| path != skip)
        .collect();
    entries.sort();

    for path in entries {
        if path.is_dir() {
            collect(&path, skip, sources);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            sources.push(
                std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("read {}: {error}", path.display())),
            );
        }
    }
}

/// Finds each `todo!("M…: …")` in one source file and attributes it to the
/// `async fn` it sits inside.
///
/// A scan of the text rather than of a registry the handlers would have to
/// remember to join: a handler cannot be left out of this by forgetting
/// something, only by being implemented.
fn pending_in(source: &str) -> Vec<Pending> {
    let mut found = Vec::new();
    let mut handler: Option<String> = None;
    let mut at = 0;

    loop {
        let next_fn = source[at..].find(FN_MARKER).map(|index| at + index);
        let next_todo = source[at..].find(TODO_MARKER).map(|index| at + index);

        let is_fn = match (next_fn, next_todo) {
            (None, None) => break,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (Some(function), Some(todo)) => function < todo,
        };

        if is_fn {
            let start = next_fn.expect("a function match") + FN_MARKER.len();
            handler = Some(identifier(&source[start..]));
            at = start;
        } else {
            let start = next_todo.expect("a todo match") + TODO_MARKER.len();
            if let Some(milestone) = milestone(&source[start..]) {
                found.push(Pending {
                    handler: handler
                        .clone()
                        .expect("a `todo!` milestone always sits inside a handler"),
                    milestone,
                });
            }
            at = start;
        }
    }

    found
}

/// The Rust identifier at the start of `tail`.
fn identifier(tail: &str) -> String {
    tail.chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

/// The milestone tag of a `todo!` message, if the message carries one.
///
/// `todo!("M4: provision the machine")` yields `M4`; a bare `todo!()`, or
/// the word appearing in prose, yields nothing.
fn milestone(tail: &str) -> Option<String> {
    let message = tail.trim_start().strip_prefix('"')?;
    let (tag, _) = message.split_once(':')?;
    let plausible = tag.len() <= MILESTONE_MAX
        && tag.starts_with('M')
        && tag.chars().all(char::is_alphanumeric);
    plausible.then(|| tag.to_owned())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{EXPECTED, Pending, unimplemented_handlers};

    #[test]
    fn the_unimplemented_handlers_are_exactly_the_expected_set() {
        let expected: Vec<Pending> = EXPECTED
            .iter()
            .map(|(handler, milestone)| Pending {
                handler: (*handler).to_owned(),
                milestone: (*milestone).to_owned(),
            })
            .collect();

        assert_eq!(
            unimplemented_handlers(),
            expected,
            "the set of unimplemented handlers changed: a milestone that implemented one must \
             remove its row here, and nothing may be added to it"
        );
    }

    #[test]
    fn no_handler_name_is_owed_twice() {
        let names: BTreeSet<&str> = EXPECTED.iter().map(|(handler, _)| *handler).collect();
        assert_eq!(
            names.len(),
            EXPECTED.len(),
            "two handlers share a name, so one of them is attributed to the wrong milestone"
        );
    }

    #[test]
    fn every_owed_handler_names_a_planned_milestone() {
        // The plan's milestones, so a typo in a `todo!` message cannot
        // invent one and quietly park work nowhere.
        let planned = ["M3c", "M4", "M5", "M6", "M7"];
        for (handler, milestone) in EXPECTED {
            assert!(
                planned.contains(milestone),
                "`{handler}` is owed by `{milestone}`, which is not a milestone in the plan"
            );
        }
    }

    #[test]
    fn the_checked_in_document_describes_the_router_it_was_exported_from() {
        // `openapi.json` is generated output that a client is built from, so
        // a route added without re-exporting it would ship a contract the
        // frontend cannot see. CI regenerates and diffs the whole file; this
        // catches the part that matters before the push.
        let checked_in: serde_json::Value =
            serde_json::from_str(include_str!("../../../../openapi.json"))
                .expect("the checked-in openapi.json is valid JSON");
        let live = serde_json::to_value(crate::openapi_document())
            .expect("the live OpenAPI document serializes");

        assert_eq!(
            keys(&checked_in["paths"]),
            keys(&live["paths"]),
            "the checked-in openapi.json describes different routes; re-run \
             `cargo run -p flyco-api --bin openapi > openapi.json`"
        );
        assert_eq!(
            keys(&checked_in["components"]["schemas"]),
            keys(&live["components"]["schemas"]),
            "the checked-in openapi.json describes different schemas; re-run \
             `cargo run -p flyco-api --bin openapi > openapi.json`"
        );
    }

    fn keys(value: &serde_json::Value) -> BTreeSet<String> {
        value
            .as_object()
            .expect("an object")
            .keys()
            .cloned()
            .collect()
    }
}
