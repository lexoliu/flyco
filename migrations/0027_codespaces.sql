-- Flyco control plane: GitHub Codespaces as a cloud provider.
--
-- Codespaces joins `provider_accounts.kind` and `machines.provider`, and
-- both joins are `CHECK`ed token lists — which SQLite cannot alter, so the
-- tables are rebuilt the way 0019 rebuilt them — with one difference in
-- order: D1 enforces foreign keys, and a DROP of a referenced parent is an
-- implicit delete, so the live `machines` -> `provider_accounts` edge makes
-- the create-copy-drop order fail. Renaming the old tables out from under
-- their names first leaves the children pointing at `_old`, the new tables
-- take the real names, and the drops at the end have no referrers left.
--
-- `machines.bootstrap_enc` is new: a codespace has no per-machine secret
-- channel — GitHub's repository secrets are shared by every codespace on
-- the repository, so one session's daemon configuration in a secret would
-- be read by the next session that starts — and the codespace instead
-- fetches its `flycod` configuration from the control plane on
-- `postStart`, authenticated by the `CODESPACE_NAME`/`GITHUB_TOKEN` pair
-- GitHub injects into it. What it fetches is this column: the rendered
-- TOML, sealed exactly as `credentials_enc` is, written by the provision
-- that created the codespace and NULL for every machine that boots a
-- different way.

ALTER TABLE provider_accounts RENAME TO provider_accounts_old;
DROP INDEX provider_accounts_by_user;

CREATE TABLE provider_accounts (
    id               TEXT    PRIMARY KEY,
    user_id          TEXT    NOT NULL REFERENCES users(id),
    kind             TEXT    NOT NULL
        CHECK (kind IN ('azure', 'aws', 'gcp', 'codespaces', 'host')),
    label            TEXT    NOT NULL,
    credentials_enc  TEXT    NOT NULL,
    linked_at_unix   INTEGER NOT NULL,
    resource_group   TEXT,
    host_id          TEXT    REFERENCES hosts(id),
    unlinked_at_unix INTEGER
);

INSERT INTO provider_accounts
    (id, user_id, kind, label, credentials_enc, linked_at_unix, resource_group,
     host_id, unlinked_at_unix)
SELECT id, user_id, kind, label, credentials_enc, linked_at_unix, resource_group,
       host_id, unlinked_at_unix
FROM provider_accounts_old;

CREATE INDEX provider_accounts_by_user ON provider_accounts (user_id, kind);

ALTER TABLE machines RENAME TO machines_old;
DROP INDEX machines_by_account;

CREATE TABLE machines (
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

INSERT INTO machines
    (id, session_id, provider_account_id, provider, machine_type, region, disk_gib,
     requested_spot, spot, state, hourly_micros, native_id, address, created_at_unix,
     storage_hourly_micros, compute_meter_started_at_unix, compute_metered_at_unix,
     storage_meter_started_at_unix, storage_metered_at_unix, vcpus, memory_mib,
     minimum_hours, minimum_charge_micros, volume_name, runtime, stopping_since_unix,
     stopping_reason)
SELECT id, session_id, provider_account_id, provider, machine_type, region, disk_gib,
       requested_spot, spot, state, hourly_micros, native_id, address, created_at_unix,
       storage_hourly_micros, compute_meter_started_at_unix, compute_metered_at_unix,
       storage_meter_started_at_unix, storage_metered_at_unix, vcpus, memory_mib,
       minimum_hours, minimum_charge_micros, volume_name, runtime, stopping_since_unix,
       stopping_reason
FROM machines_old;

CREATE INDEX machines_by_account ON machines (provider_account_id, state);

DROP TABLE machines_old;
DROP TABLE provider_accounts_old;
