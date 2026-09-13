//! The interactive half of the human path.
//!
//! Three primitives over `dialoguer`, used only when stdin is a TTY — the
//! agent contract forbids prompting on a pipe, so every call site is
//! already gated on it.

use dialoguer::{Confirm, Input, Select};

use crate::Failure;

/// Asks the user to pick one of `items`, displayed by `render`.
///
/// `items` must be non-empty — a picker with nothing to pick is a bug in
/// the caller, which should have refused earlier.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when the prompt cannot be answered.
///
/// # Panics
///
/// When `items` is empty — a picker with nothing to pick is a caller bug.
pub fn pick<T>(prompt: &str, items: &[T], render: impl Fn(&T) -> String) -> crate::Outcome<usize> {
    assert!(!items.is_empty(), "a picker needs at least one item");
    let labels: Vec<String> = items.iter().map(&render).collect();
    Select::new()
        .with_prompt(prompt)
        .items(&labels)
        .default(0)
        .interact()
        .map_err(|error| Failure::usage(format!("{prompt}: {error}")))
}

/// Asks for a line of text, with `default` preselected when given.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when the prompt cannot be answered.
pub fn input(prompt: &str, default: Option<&str>) -> crate::Outcome<String> {
    let mut ask = Input::<String>::new().with_prompt(prompt);
    if let Some(default) = default {
        ask = ask.default(default.to_owned());
    }
    ask.interact_text()
        .map_err(|error| Failure::usage(format!("{prompt}: {error}")))
}

/// A yes/no question.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when the prompt cannot be answered.
pub fn confirm(prompt: &str, default: bool) -> crate::Outcome<bool> {
    Confirm::new()
        .with_prompt(prompt)
        .default(default)
        .interact()
        .map_err(|error| Failure::usage(format!("{prompt}: {error}")))
}
