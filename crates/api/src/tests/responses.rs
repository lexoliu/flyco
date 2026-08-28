//! The guard that keeps [`crate::responses`] honest.
//!
//! The response half of the exported document is a hand-maintained table,
//! because skyzen 0.1.2 cannot derive it — see that module for why. A table
//! is only as good as the thing that stops it drifting from the code, so
//! this one reads the crate's own sources, works out what each annotated
//! handler actually returns, and refuses to agree with a table that says
//! anything else. Changing a return type without changing its row is a
//! failing test, not a quietly wrong client.
//!
//! It is the same discipline as the `todo!` ledger in
//! [`crate::tests::contract`], and for the same reason: a scan of the text
//! cannot be evaded by forgetting to register something.
//!
//! # Why the return type is parsed by balancing parentheses
//!
//! A handler's argument list contains parentheses of its own —
//! `State(user): State<CurrentUser>` has two — so the naive "everything up
//! to the first `)`" reading stops in the middle of the signature and
//! mistakes the rest of it for a return type. The scanner therefore counts
//! depth from the opening parenthesis and takes what follows the one that
//! closes it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::responses::{DECLARED, Payload, Success, UNDECLARED};

/// Marks a handler as exported into the `OpenAPI` document.
const ANNOTATION: &str = "#[skyzen::openapi]";

/// Marks the start of a handler.
const FN_MARKER: &str = "async fn ";

/// Crate name every operation id is rooted at.
const CRATE: &str = "flyco_api";

/// What a handler's return type says about its success response.
///
/// The status is only derivable when the type names it: `Created<T>` is
/// `201` by construction, a bare document is `200`, and a handler returning
/// a whole [`Response`](skyzen::Response) chooses its status at runtime, so
/// the table is what declares that one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// `200` with a document of this shape.
    Document(Payload),
    /// `201` with the created resource.
    Created(Payload),
    /// No document; the status is the table's to declare.
    Empty,
}

impl Shape {
    /// The shape a declared success response has.
    const fn of(success: Success) -> Self {
        match success {
            Success::Ok(payload) => Self::Document(payload),
            Success::Created(payload) => Self::Created(payload),
            Success::Accepted | Success::NoContent | Success::SeeOther => Self::Empty,
        }
    }
}

/// One annotated handler, as the sources describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Handler {
    /// The operation id the export emits for it.
    id: String,
    /// What its return type says it answers with.
    shape: Shape,
}

/// Every annotated handler in the crate, by operation id.
fn annotated_handlers() -> BTreeMap<String, Shape> {
    let mut found = BTreeMap::new();
    for (module, source) in handler_sources() {
        for handler in handlers_in(&module, &source) {
            assert!(
                found.insert(handler.id.clone(), handler.shape).is_none(),
                "two handlers export the operation id `{}`",
                handler.id
            );
        }
    }
    found
}

/// The crate's handler sources, each with the module it defines.
///
/// Everything under `src` except this test tree, which is not where handlers
/// live and whose own text contains the markers below.
fn handler_sources() -> Vec<(String, String)> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut sources = Vec::new();
    collect(&root, &root.join("tests"), &mut sources);
    sources
}

fn collect(directory: &Path, skip: &Path, sources: &mut Vec<(String, String)>) {
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
            let module = path
                .file_stem()
                .expect("a file with an extension has a stem")
                .to_str()
                .expect("the crate's file names are UTF-8")
                .to_owned();
            sources.push((
                module,
                std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("read {}: {error}", path.display())),
            ));
        }
    }
}

/// Finds every `#[skyzen::openapi]` handler in one module's source.
///
/// The annotation is matched as a whole line, so the several places that
/// *name* it in prose — this file included — are not mistaken for one.
fn handlers_in(module: &str, source: &str) -> Vec<Handler> {
    let mut found = Vec::new();

    for (at, _) in source.match_indices(ANNOTATION) {
        if !on_its_own_line(source, at) {
            continue;
        }

        let tail = &source[at + ANNOTATION.len()..];
        let start = tail
            .find(FN_MARKER)
            .unwrap_or_else(|| panic!("`{module}` annotates something that is not a function"))
            + FN_MARKER.len();
        let (name, arguments) = tail[start..]
            .split_once('(')
            .unwrap_or_else(|| panic!("`{module}` annotates a function with no argument list"));

        let handler = format!("{module}::{name}");
        found.push(Handler {
            id: format!("{CRATE}::{handler}"),
            shape: shape_of(&handler, returns(&handler, arguments)),
        });
    }

    found
}

/// Whether the match at `at` is an attribute rather than prose about one.
///
/// Several doc comments in this crate name the annotation — this file's own
/// does — and an attribute is the only occurrence that starts a line.
fn on_its_own_line(source: &str, at: usize) -> bool {
    source[..at]
        .rsplit_once('\n')
        .is_none_or(|(_, prefix)| prefix.trim().is_empty())
}

/// The return type of a signature, given everything after its opening
/// parenthesis.
///
/// Counts depth rather than looking for the first `)`, because an argument
/// list is full of parentheses of its own.
fn returns<'a>(handler: &str, arguments: &'a str) -> &'a str {
    let mut depth = 1_usize;
    for (index, character) in arguments.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    let tail = &arguments[index + 1..];
                    let body = tail
                        .find('{')
                        .unwrap_or_else(|| panic!("`{handler}` has no body"));
                    return tail[..body]
                        .trim()
                        .strip_prefix("->")
                        .unwrap_or_else(|| panic!("`{handler}` returns nothing"))
                        .trim();
                }
            }
            _ => {}
        }
    }
    panic!("`{handler}` has an argument list that never closes")
}

/// What a return type says about the response.
fn shape_of(handler: &str, returns: &str) -> Shape {
    let inner = unwrap_generic(returns, "Outcome").unwrap_or(returns);

    if inner == "Response" {
        return Shape::Empty;
    }
    if let Some(created) = unwrap_generic(inner, "Created") {
        return Shape::Created(payload_of(handler, created));
    }
    Shape::Document(payload_of(handler, inner))
}

/// The payload a `Json<…>` return type carries.
fn payload_of(handler: &str, returns: &str) -> Payload {
    let json = unwrap_generic(returns, "Json").unwrap_or_else(|| {
        panic!("`{handler}` returns `{returns}`, which this guard cannot describe")
    });
    unwrap_generic(json, "Vec").map_or_else(
        || Payload::One(leak(json)),
        |item| Payload::Many(leak(item)),
    )
}

/// The argument of `wrapper<…>`, if `returns` is exactly that.
fn unwrap_generic<'a>(returns: &'a str, wrapper: &str) -> Option<&'a str> {
    returns
        .strip_prefix(wrapper)?
        .strip_prefix('<')?
        .strip_suffix('>')
        .map(str::trim)
}

/// The table holds `&'static str` names, so a name read from the sources has
/// to outlive the comparison. The scanner runs once per test, over a fixed
/// set of files, so the leak is bounded by the crate's own size.
fn leak(name: &str) -> &'static str {
    Box::leak(name.to_owned().into_boxed_str())
}

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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        DECLARED, Payload, Shape, Success, UNDECLARED, annotated_handlers, checked_in, document,
        operations,
    };

    /// The declaration table, by operation id.
    fn declared() -> BTreeMap<String, Success> {
        DECLARED
            .iter()
            .map(|(id, success)| ((*id).to_owned(), *success))
            .collect()
    }

    #[test]
    fn every_annotated_handler_declares_exactly_what_it_returns() {
        let handlers = annotated_handlers();
        let declared = declared();

        let expected: BTreeMap<String, Shape> = declared
            .iter()
            .map(|(id, success)| (id.clone(), Shape::of(*success)))
            .collect();

        assert_eq!(
            handlers, expected,
            "the response table disagrees with the handlers it describes: a handler whose \
             return type changed must have its row in `flyco_api::responses::DECLARED` changed \
             with it, and a handler that was added or removed must be added to or removed from \
             that table"
        );
    }

    #[test]
    fn the_table_is_sorted_and_names_each_operation_once() {
        let ids: Vec<&str> = DECLARED.iter().map(|(id, _)| *id).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(ids, sorted, "`DECLARED` must be sorted by operation id");
    }

    #[test]
    fn the_document_describes_every_operation_it_exports() {
        let exported: Vec<String> = operations(&document()).into_keys().collect();
        let known: Vec<String> = declared()
            .into_keys()
            .chain(UNDECLARED.iter().map(|id| (*id).to_owned()))
            .collect();

        for id in &exported {
            assert!(
                known.contains(id),
                "`{id}` is exported but says nothing about its response"
            );
        }
        for id in &known {
            assert!(
                exported.contains(id),
                "`{id}` is described but no longer exported; remove its row"
            );
        }
    }

    #[test]
    fn every_declared_payload_reaches_the_checked_in_document() {
        // The generated TypeScript client is built from the checked-in file,
        // not from the live router, so this is the assertion that decides
        // whether a client has types for the responses it receives.
        let file = checked_in();
        let schemas = file["components"]["schemas"]
            .as_object()
            .expect("the document has component schemas");
        let operations = operations(&file);

        for (id, success) in declared() {
            let operation = operations
                .get(&id)
                .unwrap_or_else(|| panic!("`{id}` is missing from the checked-in openapi.json"));
            let response = &operation["responses"][success.status().to_string()];
            assert!(
                response.is_object(),
                "`{id}` does not answer {} in the checked-in openapi.json; re-run \
                 `cargo run -p flyco-api --bin openapi > openapi.json`",
                success.status()
            );

            let Some(payload) = success.payload() else {
                assert!(
                    response["content"].is_null(),
                    "`{id}` answers with no document but carries content"
                );
                continue;
            };

            let schema = &response["content"]["application/json"]["schema"];
            let reference = match payload {
                Payload::One(_) => &schema["$ref"],
                Payload::Many(_) => &schema["items"]["$ref"],
            };
            let expected = format!("#/components/schemas/{}", payload.schema_name());
            assert_eq!(
                reference.as_str(),
                Some(expected.as_str()),
                "`{id}` does not carry the response schema it declares"
            );
            assert!(
                schemas.contains_key(payload.schema_name()),
                "`{id}` refers to `{}`, which is not in components.schemas",
                payload.schema_name()
            );
        }
    }

    #[test]
    fn every_payload_bearing_operation_carries_content() {
        // The number this whole module exists to keep above zero: before the
        // table, two of sixty-six operations described a response body.
        let file = checked_in();
        let operations = operations(&file);
        let with_content = operations
            .values()
            .filter(|operation| {
                operation["responses"]
                    .as_object()
                    .expect("responses is an object")
                    .values()
                    .any(|response| response["content"].is_object())
            })
            .count();

        let expected = DECLARED
            .iter()
            .filter(|(_, success)| success.payload().is_some())
            .count();
        assert_eq!(
            with_content, expected,
            "the checked-in openapi.json documents a different number of response bodies than \
             the table declares"
        );
    }
}
