-- Flyco control plane: Devin joins the harnesses a session may run.
--
-- `sessions.harness` and `harness_accounts.harness` are `CHECK`ed token
-- lists — which SQLite cannot alter — so both tables are rebuilt. Unlike
-- 0019's shape this rebuild is rename-first: D1 enforces foreign keys and
-- a DROP of a referenced parent is an implicit delete, so `sessions` —
-- named by six children — cannot leave while they point at it. The
-- rename makes every child follow to `_old`; the new tables take the real
-- names; each child is then dropped and recreated pointing at the new
-- `sessions` (a child's own DROP is always legal — nothing references the
-- children); and the `_old` tables drop last with no referrers left.

ALTER TABLE sessions RENAME TO sessions_old;
DROP INDEX sessions_by_user_state;
DROP INDEX idx_sessions_usage_limit;

ALTER TABLE harness_accounts RENAME TO harness_accounts_old;
DROP INDEX harness_accounts_by_user;
DROP INDEX one_harness_account_per_user;

CREATE TABLE sessions (
    id                            TEXT    PRIMARY KEY,
    user_id                       TEXT    NOT NULL REFERENCES users(id),
    harness                       TEXT    NOT NULL
        CHECK (harness IN ('claude_code', 'codex', 'devin')),
    repo                          TEXT    NOT NULL,
    state                         TEXT    NOT NULL,
    budget_id                     TEXT    NOT NULL REFERENCES budgets(id),
    created_at_unix               INTEGER NOT NULL,
    last_active_unix              INTEGER NOT NULL,
    daemon_token_hash             TEXT,
    failure_reason                TEXT,
    harness_session_id            TEXT,
    title                         TEXT    NOT NULL DEFAULT '',
    machine_origin                TEXT    NOT NULL DEFAULT 'auto'
        CHECK (machine_origin IN ('auto', 'user')),
    branch                        TEXT,
    interrupted_reason            TEXT,
    activity                      TEXT    NOT NULL DEFAULT 'idle'
        CHECK (activity IN ('working', 'needs_input', 'idle')),
    model                         TEXT,
    effort                        TEXT,
    paused_reason                 TEXT
        CHECK (paused_reason IN ('budget', 'usage_limit')),
    usage_limit_window            TEXT,
    usage_limit_resets_at_unix    INTEGER,
    usage_limit_resume_at_unix    INTEGER,
    usage_limit_queued_message    TEXT,
    permission_mode               TEXT
);

INSERT INTO sessions
    (id, user_id, harness, repo, state, budget_id, created_at_unix,
     last_active_unix, daemon_token_hash, failure_reason, harness_session_id,
     title, machine_origin, branch, interrupted_reason, activity, model,
     effort, paused_reason, usage_limit_window, usage_limit_resets_at_unix,
     usage_limit_resume_at_unix, usage_limit_queued_message, permission_mode)
SELECT id, user_id, harness, repo, state, budget_id, created_at_unix,
       last_active_unix, daemon_token_hash, failure_reason, harness_session_id,
       title, machine_origin, branch, interrupted_reason, activity, model,
       effort, paused_reason, usage_limit_window, usage_limit_resets_at_unix,
       usage_limit_resume_at_unix, usage_limit_queued_message, permission_mode
FROM sessions_old;

CREATE INDEX sessions_by_user_state ON sessions (user_id, state);
CREATE INDEX idx_sessions_usage_limit
    ON sessions (usage_limit_resets_at_unix)
    WHERE paused_reason = 'usage_limit';

CREATE TABLE harness_accounts (
    id              TEXT    PRIMARY KEY,
    user_id         TEXT    NOT NULL REFERENCES users(id),
    harness         TEXT    NOT NULL
        CHECK (harness IN ('claude_code', 'codex', 'devin')),
    label           TEXT    NOT NULL,
    credential_enc  TEXT    NOT NULL,
    linked_at_unix  INTEGER NOT NULL,
    expires_at_unix INTEGER,
    models_json     TEXT,
    usage_json      TEXT
);

INSERT INTO harness_accounts
    (id, user_id, harness, label, credential_enc, linked_at_unix,
     expires_at_unix, models_json, usage_json)
SELECT id, user_id, harness, label, credential_enc, linked_at_unix,
       expires_at_unix, models_json, usage_json
FROM harness_accounts_old;

CREATE INDEX harness_accounts_by_user ON harness_accounts (user_id, harness);
CREATE UNIQUE INDEX one_harness_account_per_user
    ON harness_accounts (user_id, harness);

-- The children of `sessions` (and `harness_accounts`): renamed parents
-- left them pointing at `_old`, so each is staged under a `_new` name
-- against the real tables, the old child is dropped — always legal, since
-- nothing references a child — and the stage takes its name back.

CREATE TABLE approvals_new (
    id              TEXT    PRIMARY KEY,
    session_id      TEXT    NOT NULL REFERENCES sessions(id),
    payload         TEXT    NOT NULL,
    state           TEXT    NOT NULL
        CHECK (state IN ('pending', 'approved', 'denied')),
    created_at_unix INTEGER NOT NULL,
    decided_at_unix INTEGER
);
INSERT INTO approvals_new SELECT * FROM approvals;
DROP TABLE approvals;
ALTER TABLE approvals_new RENAME TO approvals;
CREATE INDEX approvals_by_session_state ON approvals (session_id, state);

CREATE TABLE session_env_new (
    session_id      TEXT    PRIMARY KEY REFERENCES sessions(id),
    entries_enc     TEXT    NOT NULL,
    updated_at_unix INTEGER NOT NULL
);
INSERT INTO session_env_new SELECT * FROM session_env;
DROP TABLE session_env;
ALTER TABLE session_env_new RENAME TO session_env;

CREATE TABLE machines_new (
    id                            TEXT    PRIMARY KEY,
    session_id                    TEXT    NOT NULL UNIQUE REFERENCES sessions(id),
    provider_account_id           TEXT    NOT NULL REFERENCES provider_accounts(id),
    provider                      TEXT    NOT NULL
        CHECK (provider IN ('azure', 'aws', 'gcp', 'codespaces', 'host')),
    machine_type                  TEXT    NOT NULL,
    region                        TEXT    NOT NULL,
    disk_gib                      INTEGER NOT NULL,
    requested_spot                INTEGER NOT NULL CHECK (requested_spot IN (0, 1)),
    spot                          INTEGER NOT NULL CHECK (spot IN (0, 1)),
    state                         TEXT    NOT NULL
        CHECK (state IN ('provisioning', 'running', 'deallocated', 'destroyed')),
    hourly_micros                 INTEGER,
    native_id                     TEXT,
    address                       TEXT,
    created_at_unix               INTEGER NOT NULL,
    storage_hourly_micros         INTEGER,
    compute_meter_started_at_unix INTEGER,
    compute_metered_at_unix       INTEGER,
    storage_meter_started_at_unix INTEGER,
    storage_metered_at_unix       INTEGER,
    vcpus                         INTEGER,
    memory_mib                    INTEGER,
    minimum_hours                 INTEGER,
    minimum_charge_micros         INTEGER,
    volume_name                   TEXT,
    runtime                       TEXT    NOT NULL DEFAULT 'vm'
        CHECK (runtime IN ('vm', 'container')),
    stopping_since_unix           INTEGER,
    stopping_reason               TEXT
        CHECK (stopping_reason IN ('sigterm')),
    bootstrap_enc                 TEXT
);
INSERT INTO machines_new
    (id, session_id, provider_account_id, provider, machine_type, region, disk_gib,
     requested_spot, spot, state, hourly_micros, native_id, address, created_at_unix,
     storage_hourly_micros, compute_meter_started_at_unix, compute_metered_at_unix,
     storage_meter_started_at_unix, storage_metered_at_unix, vcpus, memory_mib,
     minimum_hours, minimum_charge_micros, volume_name, runtime, stopping_since_unix,
     stopping_reason, bootstrap_enc)
SELECT id, session_id, provider_account_id, provider, machine_type, region, disk_gib,
       requested_spot, spot, state, hourly_micros, native_id, address, created_at_unix,
       storage_hourly_micros, compute_meter_started_at_unix, compute_metered_at_unix,
       storage_meter_started_at_unix, storage_metered_at_unix, vcpus, memory_mib,
       minimum_hours, minimum_charge_micros, volume_name, runtime, stopping_since_unix,
       stopping_reason, bootstrap_enc
FROM machines;
DROP TABLE machines;
ALTER TABLE machines_new RENAME TO machines;
CREATE INDEX machines_by_account ON machines (provider_account_id, state);

CREATE TABLE harness_observations_new (
    id                   TEXT    PRIMARY KEY,
    user_id              TEXT    NOT NULL REFERENCES users(id),
    harness_account_id   TEXT    NOT NULL REFERENCES harness_accounts(id),
    session_id           TEXT    NOT NULL REFERENCES sessions(id),
    observed_cost_micros INTEGER,
    rate_limited_at_unix INTEGER,
    resets_at_unix       INTEGER,
    at_unix              INTEGER NOT NULL,
    CHECK (observed_cost_micros IS NOT NULL OR rate_limited_at_unix IS NOT NULL)
);
INSERT INTO harness_observations_new SELECT * FROM harness_observations;
DROP TABLE harness_observations;
ALTER TABLE harness_observations_new RENAME TO harness_observations;
CREATE INDEX harness_observations_by_account
    ON harness_observations (harness_account_id, at_unix);

CREATE TABLE budget_signals_new (
    id              TEXT    PRIMARY KEY,
    budget_id       TEXT    NOT NULL REFERENCES budgets(id),
    session_id      TEXT    NOT NULL REFERENCES sessions(id),
    signal          TEXT    NOT NULL
        CHECK (signal IN ('notice50', 'warn80', 'final_warn90', 'pause')),
    ordinal         INTEGER NOT NULL CHECK (ordinal BETWEEN 1 AND 4),
    delivered       INTEGER NOT NULL DEFAULT 0 CHECK (delivered IN (0, 1)),
    created_at_unix INTEGER NOT NULL,
    UNIQUE (budget_id, signal)
);
INSERT INTO budget_signals_new SELECT * FROM budget_signals;
DROP TABLE budget_signals;
ALTER TABLE budget_signals_new RENAME TO budget_signals;
CREATE INDEX budget_signals_pending
    ON budget_signals (delivered, created_at_unix, ordinal, id);

CREATE TABLE idempotency_keys_new (
    user_id         TEXT    NOT NULL REFERENCES users(id),
    key             TEXT    NOT NULL,
    session_id      TEXT    REFERENCES sessions(id),
    created_at_unix INTEGER NOT NULL,
    PRIMARY KEY (user_id, key)
);
INSERT INTO idempotency_keys_new SELECT * FROM idempotency_keys;
DROP TABLE idempotency_keys;
ALTER TABLE idempotency_keys_new RENAME TO idempotency_keys;

DROP TABLE sessions_old;
DROP TABLE harness_accounts_old;
