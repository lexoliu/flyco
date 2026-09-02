//! The `CHECK` constraints and the domain enums say the same thing.
//!
//! Every text column holding a domain enum spells its tokens twice: once in
//! the enum, once in the migration's `CHECK (… IN (…))`. Before
//! `#[derive(skyzen::Column)]` the second copy was a comment asking the next
//! reader to keep them equal; [`ColumnEnum::TOKENS`] makes it a fact the
//! build checks.
//!
//! Both directions are covered. Every constrained column below must list
//! exactly its enum's tokens, in declaration order — and every text `CHECK`
//! the migrations contain must be one of the columns below, so adding a
//! constrained column without saying which enum guards it fails here rather
//! than drifting quietly.

use flyco_core::{
    ApprovalState, BudgetSignal, CloudProviderKind, HarnessKind, MachineOrigin, MachineState,
    SkillScope, SpendKind,
};
use skyzen_services::sql::ColumnEnum;

use crate::testing::MIGRATIONS;

/// Every text-valued `CHECK` in the schema, and the enum that owns it.
const CONSTRAINED: &[(&str, &str, &[&str])] = &[
    ("sessions", "harness", HarnessKind::TOKENS),
    ("sessions", "machine_origin", MachineOrigin::TOKENS),
    ("spend_events", "kind", SpendKind::TOKENS),
    ("approvals", "state", ApprovalState::TOKENS),
    ("provider_accounts", "kind", CloudProviderKind::TOKENS),
    ("harness_accounts", "harness", HarnessKind::TOKENS),
    ("skills", "scope", SkillScope::TOKENS),
    ("machines", "provider", CloudProviderKind::TOKENS),
    ("machines", "state", MachineState::TOKENS),
    ("budget_signals", "signal", BudgetSignal::TOKENS),
];

/// One `CHECK (<column> IN ('a', 'b'))` found in a migration.
#[derive(Debug, PartialEq, Eq)]
struct Constraint {
    table: String,
    column: String,
    tokens: Vec<String>,
}

/// Reads every text-valued `IN` constraint out of the migrations.
///
/// Deliberately a scanner over the statements flyco actually writes rather
/// than a SQL parser: the migrations are the only input, one `CREATE TABLE`
/// per line-block with one column per line, and a parser would be a second
/// dialect to keep correct.
fn constraints() -> Vec<Constraint> {
    let mut found = Vec::new();
    let mut table = String::new();

    for line in MIGRATIONS.iter().flat_map(|sql| sql.lines()) {
        let trimmed = line.trim();
        // An `ALTER TABLE … ADD COLUMN` names its table too, and a column
        // added later is as constrained as one declared at the start.
        if let Some(rest) = trimmed.strip_prefix("ALTER TABLE ") {
            table = rest
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_owned();
        }
        if let Some(rest) = trimmed.strip_prefix("CREATE TABLE ") {
            table = rest
                .trim_start_matches("IF NOT EXISTS ")
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_end_matches('(')
                .to_owned();
            continue;
        }

        let Some(check) = trimmed.split_once("CHECK (").map(|(_, rest)| rest) else {
            continue;
        };
        let Some((column, list)) = check.split_once(" IN (") else {
            continue;
        };
        let Some((list, _)) = list.split_once(')') else {
            continue;
        };
        // `INTEGER … CHECK (enabled IN (0, 1))` is a boolean, not an enum.
        if !list.contains('\'') {
            continue;
        }

        found.push(Constraint {
            table: table.clone(),
            column: column.trim().to_owned(),
            tokens: list
                .split(',')
                .map(|token| token.trim().trim_matches('\'').to_owned())
                .collect(),
        });
    }
    found
}

#[test]
fn every_constrained_column_lists_exactly_its_enum_tokens() {
    let found = constraints();

    for (table, column, tokens) in CONSTRAINED {
        let constraint = found
            .iter()
            .find(|found| found.table == *table && found.column == *column)
            .unwrap_or_else(|| panic!("`{table}.{column}` has no CHECK constraint"));

        assert_eq!(
            constraint.tokens, *tokens,
            "`{table}.{column}` and its enum disagree about what may be stored"
        );
    }
}

#[test]
fn no_constrained_column_is_left_unguarded_by_an_enum() {
    for constraint in constraints() {
        assert!(
            CONSTRAINED.iter().any(|(table, column, _)| {
                *table == constraint.table && *column == constraint.column
            }),
            "`{}.{}` constrains a token set that no enum owns; add it to `CONSTRAINED`",
            constraint.table,
            constraint.column
        );
    }
}
