-- Flyco control plane, milestone M2b: sessions, budgets, and approvals.
--
-- Every CHECK below lists exactly the snake_case tokens the matching
-- flyco_core enum serializes to, so the schema and the domain model cannot
-- drift apart silently.

ALTER TABLE users ADD COLUMN session_cap INTEGER NOT NULL DEFAULT 5;

CREATE TABLE sessions (
    id               TEXT    PRIMARY KEY,
    user_id          TEXT    NOT NULL REFERENCES users(id),
    harness          TEXT    NOT NULL CHECK (harness IN ('claude_code', 'codex')),
    repo             TEXT    NOT NULL,
    state            TEXT    NOT NULL,
    budget_id        TEXT    NOT NULL,
    created_at_unix  INTEGER NOT NULL,
    last_active_unix INTEGER NOT NULL
);

-- `budgets.session_id` carries no REFERENCES: the budget row is written
-- first so the session can point at it, and D1 has no transactions to
-- order the pair inside.
CREATE TABLE budgets (
    id            TEXT    PRIMARY KEY,
    session_id    TEXT    NOT NULL,
    limit_micros  INTEGER NOT NULL,
    spent_micros  INTEGER NOT NULL DEFAULT 0,
    stage         TEXT    NOT NULL
);

-- Append-only audit ledger. `budgets.spent_micros` and `budgets.stage` are
-- a cache of replaying this table through flyco_core::BudgetState; rows here
-- are never updated or deleted.
CREATE TABLE spend_events (
    id            TEXT    PRIMARY KEY,
    budget_id     TEXT    NOT NULL REFERENCES budgets(id),
    kind          TEXT    NOT NULL CHECK (kind IN ('compute', 'storage')),
    amount_micros INTEGER NOT NULL,
    at_unix       INTEGER NOT NULL,
    detail        TEXT    NOT NULL
);

-- `payload` is the JSON encoding of flyco_core::wire::ApprovalPayload.
CREATE TABLE approvals (
    id              TEXT    PRIMARY KEY,
    session_id      TEXT    NOT NULL REFERENCES sessions(id),
    payload         TEXT    NOT NULL,
    state           TEXT    NOT NULL CHECK (state IN ('pending', 'approved', 'denied')),
    created_at_unix INTEGER NOT NULL,
    decided_at_unix INTEGER
);

CREATE INDEX sessions_by_user_state ON sessions (user_id, state);
CREATE INDEX approvals_by_session_state ON approvals (session_id, state);
CREATE INDEX spend_events_by_budget ON spend_events (budget_id, at_unix, id);
