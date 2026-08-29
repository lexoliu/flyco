-- Flyco control plane, milestone M4: the machine a session runs on.
--
-- One row per session, which is why `session_id` is UNIQUE rather than
-- merely indexed: a session has exactly one machine at a time, and a second
-- row would be a leaked cloud resource nobody is billing anybody for.
--
-- `provider_account_id` is carried even though `provider` repeats its kind:
-- a user may hold two Azure accounts, and destroying a machine has to reach
-- the one that created it.
--
-- Prices are integer microdollars like every other amount in this schema,
-- and `hourly_micros` is NULL for a machine flyco does not meter — a host
-- the user registered over SSH, which they already own and already pay for.
-- Zero would be a different claim, and a budget told an hour costs nothing
-- concludes the session can run forever.
--
-- `requested_spot` and `spot` are separate because they disagree: a spot
-- request Azure cannot honour is retried as on-demand rather than failing
-- the session, and the price billed follows what was obtained.

CREATE TABLE machines (
    id                  TEXT    PRIMARY KEY,
    session_id          TEXT    NOT NULL UNIQUE REFERENCES sessions(id),
    provider_account_id TEXT    NOT NULL REFERENCES provider_accounts(id),
    provider            TEXT    NOT NULL CHECK (provider IN ('azure', 'aws', 'gcp', 'byo_ssh')),
    machine_type        TEXT    NOT NULL,
    region              TEXT    NOT NULL,
    disk_gib            INTEGER NOT NULL,
    requested_spot      INTEGER NOT NULL CHECK (requested_spot IN (0, 1)),
    spot                INTEGER NOT NULL CHECK (spot IN (0, 1)),
    state               TEXT    NOT NULL
        CHECK (state IN ('provisioning', 'running', 'deallocated', 'destroyed')),
    hourly_micros       INTEGER,
    native_id           TEXT,
    address             TEXT,
    created_at_unix     INTEGER NOT NULL
);

CREATE INDEX machines_by_account ON machines (provider_account_id, state);
