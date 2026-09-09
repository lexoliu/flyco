//! The flycod⇄sidecar protocol contract, pinned by shared fixtures.
//!
//! `fixtures/protocol/` holds one canonical JSON document per protocol
//! message variant. This test decodes each into the Rust type, re-encodes
//! it, and asserts the bytes come back unchanged; `sidecar/protocol.test.ts`
//! runs the identical assertion against the zod schemas. Neither side can
//! rename a field, drop one, or change a tag without the other's test
//! failing, so the two declarations cannot drift apart.
//!
//! "Canonical" means: object keys sorted, no insignificant whitespace, one
//! trailing newline. The sorting is done here rather than relying on
//! `serde_json`'s map ordering, which depends on whether anything in the
//! build enables its `preserve_order` feature.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use flyco_daemon::harness::claude::protocol::{SidecarCommand, SidecarEvent};
use serde_json::Value;

/// Every `type` tag [`SidecarCommand`] can serialize under.
const COMMAND_TAGS: [&str; 8] = [
    "start",
    "user_message",
    "interrupt",
    "compact",
    "set_model",
    "approval_decision",
    "store_response",
    "shutdown",
];

/// Every `type` tag [`SidecarEvent`] can serialize under.
const EVENT_TAGS: [&str; 9] = [
    "ready",
    "started",
    "capabilities",
    "models",
    "mcp_servers",
    "sdk_message",
    "approval_request",
    "store_request",
    "fatal",
];

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/protocol")
}

/// Sorts every object's keys, recursively.
fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<&String, Value> = map
                .iter()
                .map(|(key, value)| (key, canonical(value)))
                .collect();
            Value::Object(
                sorted
                    .into_iter()
                    .map(|(key, value)| (key.clone(), value))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        scalar => scalar.clone(),
    }
}

/// The canonical text of a value: sorted keys, compact, one newline.
fn canonical_text(value: &Value) -> String {
    let mut text = serde_json::to_string(&canonical(value)).expect("a Value serializes to JSON");
    text.push('\n');
    text
}

/// Every fixture whose name starts with `prefix`, as (name, contents).
fn fixtures(prefix: &str) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = std::fs::read_dir(fixture_dir())
        .expect("fixtures/protocol must exist")
        .map(|entry| entry.expect("read a fixture directory entry").path())
        .filter_map(|path| {
            if path.extension()? != "json" {
                return None;
            }
            let name = path.file_name()?.to_str()?.to_owned();
            name.starts_with(prefix).then(|| {
                let text = std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
                (name, text)
            })
        })
        .collect();
    found.sort();
    assert!(!found.is_empty(), "no {prefix}* fixtures were found");
    found
}

/// Asserts a fixture file is itself canonical, so drift is visible in the
/// diff rather than absorbed by the test.
fn assert_file_is_canonical(name: &str, text: &str) {
    let parsed: Value = serde_json::from_str(text).unwrap_or_else(|error| {
        panic!("{name} is not valid JSON: {error}");
    });
    assert_eq!(
        canonical_text(&parsed),
        text,
        "{name} is not in canonical form (sorted keys, compact, trailing newline)"
    );
}

#[test]
fn every_command_fixture_round_trips_byte_for_byte() {
    let mut seen = BTreeSet::new();
    for (name, text) in fixtures("command_") {
        assert_file_is_canonical(&name, &text);
        let command: SidecarCommand = serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("{name} does not decode as a SidecarCommand: {error}"));
        let re_encoded: Value =
            serde_json::to_value(&command).expect("a SidecarCommand serializes to JSON");
        assert_eq!(
            canonical_text(&re_encoded),
            text,
            "{name} did not survive a decode/encode round trip"
        );
        seen.insert(command.tag());
    }
    assert_eq!(
        seen,
        COMMAND_TAGS.into_iter().collect::<BTreeSet<_>>(),
        "every SidecarCommand variant needs at least one fixture"
    );
}

#[test]
fn every_event_fixture_round_trips_byte_for_byte() {
    let mut seen = BTreeSet::new();
    for (name, text) in fixtures("event_") {
        assert_file_is_canonical(&name, &text);
        let event: SidecarEvent = serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("{name} does not decode as a SidecarEvent: {error}"));
        let re_encoded: Value =
            serde_json::to_value(&event).expect("a SidecarEvent serializes to JSON");
        assert_eq!(
            canonical_text(&re_encoded),
            text,
            "{name} did not survive a decode/encode round trip"
        );
        seen.insert(event.tag());
    }
    assert_eq!(
        seen,
        EVENT_TAGS.into_iter().collect::<BTreeSet<_>>(),
        "every SidecarEvent variant needs at least one fixture"
    );
}

#[test]
fn a_fixture_naming_a_field_neither_side_knows_would_fail() {
    // Guards the guard: the round-trip assertion only means something
    // because an unknown field survives decoding into the JSON but not
    // back out of the typed value.
    let text = r#"{"reason":"because","type":"interrupt"}"#;
    let command: SidecarCommand = serde_json::from_str(text).expect("decode");
    let re_encoded = serde_json::to_value(&command).expect("encode");
    assert_ne!(canonical_text(&re_encoded).trim(), text);
}
