-- Flyco control plane: Devin joins the harnesses a session may run.
--
-- `sessions.harness` and `harness_accounts.harness` are `CHECK`ed token
-- lists — which SQLite cannot alter — so both tables are rebuilt the way
-- 0019 and 0027 rebuilt theirs. Every other column is carried verbatim;
-- the only change either table sees is `'devin'` in the list.

CREATE TABLE sessions_new (
    id                            TEXT    PRIMARY KEY,
    user_id                       TEXT    NOT NULL REFERENCES users(id),
    harness                       TEXT    NOT NULL
        CHECK (harness IN ('claude_code', 'codex', 'devin')),
    repo                          TEXT    NOT NULL,
    state                         TEXT    NOT NULL,
    budget_id                     TEXT    NOT NULL,
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

INSERT INTO sessions_new
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
FROM sessions;

DROP TABLE sessions;

ALTER TABLE sessions_new RENAME TO sessions;

CREATE INDEX sessions_by_user_state ON sessions (user_id, state);
CREATE INDEX IF NOT EXISTS idx_sessions_usage_limit
    ON sessions (usage_limit_resets_at_unix)
    WHERE paused_reason = 'usage_limit';

CREATE TABLE harness_accounts_new (
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

INSERT INTO harness_accounts_new
    (id, user_id, harness, label, credential_enc, linked_at_unix,
     expires_at_unix, models_json, usage_json)
SELECT id, user_id, harness, label, credential_enc, linked_at_unix,
       expires_at_unix, models_json, usage_json
FROM harness_accounts;

DROP TABLE harness_accounts;

ALTER TABLE harness_accounts_new RENAME TO harness_accounts;

CREATE INDEX harness_accounts_by_user ON harness_accounts (user_id, harness);
CREATE UNIQUE INDEX one_harness_account_per_user
ON harness_accounts (user_id, harness);
