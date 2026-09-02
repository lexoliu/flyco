-- Flyco control plane, issue #64: a machine the user owns, enrolled.
--
-- The control plane runs on Cloudflare Workers and has no TCP sockets, so it
-- can never dial somebody's machine. `byo_ssh` — a host reached over SSH from
-- the Worker — was therefore `Unsupported` on every hosted deployment: a
-- linked host could never actually run a session. It is replaced by
-- enrollment: the machine installs `flycod host`, registers itself, and holds
-- one outbound WebSocket that the control plane sends container work down.
--
-- Three changes, in the order they have to happen.

-- 1. The hosts themselves.
--
-- `token_hash` is the SHA-256 of the long-lived `fh_` token the machine keeps
-- root-only on disk, stored exactly as a daemon token is: minting replaces it,
-- which is what makes rotation a revocation. It is nullable because a removed
-- host has none — the row outlives the credential so the sessions that ran
-- there still name something.
--
-- `facts` is the JSON document the machine reported about itself:
-- architecture, vCPUs, memory, free disk, Podman version, kernel, hostname.
-- Rewritten on every `Hello` rather than only at enrollment, because a machine
-- that gained memory or filled its disk is a different machine to schedule
-- onto and flyco has no other way to learn it.
--
-- `last_seen_unix` is nullable for the moment between enrolling and the first
-- socket, and is what tells "offline since yesterday" from "offline for a
-- second while systemd restarted the unit".
CREATE TABLE hosts (
    id              TEXT    PRIMARY KEY,
    user_id         TEXT    NOT NULL REFERENCES users(id),
    label           TEXT    NOT NULL,
    facts           TEXT    NOT NULL,
    token_hash      TEXT,
    state           TEXT    NOT NULL
        CHECK (state IN ('online', 'offline', 'draining', 'removed')),
    last_seen_unix  INTEGER,
    created_at_unix INTEGER NOT NULL
);

CREATE INDEX hosts_by_user ON hosts (user_id, state);

-- Single-use enrollment tokens.
--
-- Hashed like every other flyco credential, ten minutes long, and bound to the
-- user who minted them: what the wizard shows in its one-line install command
-- is the only copy of the plaintext that will ever exist.
--
-- `spent_at_unix` and `host_id` are written together by the enrollment that
-- consumes the token, and are what `GET /v1/hosts/enrollment-tokens/{id}`
-- polls: NULL means the machine has not arrived yet. A spent row is kept
-- rather than deleted, because a wizard that is still polling has to be told
-- *which* host arrived rather than that its token vanished.
CREATE TABLE host_enrollment_tokens (
    id              TEXT    PRIMARY KEY,
    user_id         TEXT    NOT NULL REFERENCES users(id),
    token_hash      TEXT    NOT NULL,
    expires_at_unix INTEGER NOT NULL,
    spent_at_unix   INTEGER,
    host_id         TEXT    REFERENCES hosts(id),
    created_at_unix INTEGER NOT NULL
);

CREATE INDEX host_enrollment_tokens_by_user ON host_enrollment_tokens (user_id, created_at_unix);

-- 2. The provider kind is `host`, not `byo_ssh`.
--
-- Both tables CHECK the token they store, so the token cannot be renamed in
-- the domain model without rewriting the constraint — and SQLite has no
-- `ALTER TABLE … DROP CONSTRAINT`. The table is therefore rebuilt, which is
-- the documented SQLite procedure and the only one D1 supports. Existing rows
-- are carried across with the new spelling: a deployment that linked an
-- SSH host keeps its account row, now naming the host it will be enrolled as.
--
-- The credential blob inside `credentials_enc` is *not* rewritten here. It is
-- sealed, so this migration cannot read it, and it is unsealed as a
-- `ProviderCredentials` whose `byo_ssh` variant no longer exists — which fails
-- loudly on the one account that could hold one, rather than silently
-- provisioning against a host nobody enrolled.
--
-- `host_id` is the join key the sealed credential cannot be: the credential
-- names the machine an account provisions onto, and nothing can read it in
-- SQL, so the same id is a column beside it. That is what lets a catalog read
-- fetch an account's host in the query that fetched the account, and what
-- lets a removed host stop offering itself. NULL for every cloud account, and
-- required in practice for every host one — `POST /v1/hosts/enroll` writes
-- both halves together.
CREATE TABLE provider_accounts_new (
    id              TEXT    PRIMARY KEY,
    user_id         TEXT    NOT NULL REFERENCES users(id),
    kind            TEXT    NOT NULL CHECK (kind IN ('azure', 'aws', 'gcp', 'host')),
    label           TEXT    NOT NULL,
    credentials_enc TEXT    NOT NULL,
    linked_at_unix  INTEGER NOT NULL,
    resource_group  TEXT,
    host_id         TEXT    REFERENCES hosts(id)
);

INSERT INTO provider_accounts_new
    (id, user_id, kind, label, credentials_enc, linked_at_unix, resource_group)
SELECT id, user_id,
       CASE kind WHEN 'byo_ssh' THEN 'host' ELSE kind END,
       label, credentials_enc, linked_at_unix, resource_group
FROM provider_accounts;

DROP TABLE provider_accounts;

ALTER TABLE provider_accounts_new RENAME TO provider_accounts;

CREATE INDEX provider_accounts_by_user ON provider_accounts (user_id, kind);

-- 3. The machine row, with the same rename and the volume a container keeps.
--
-- `volume_name` is the Podman volume holding the session's checkout on a
-- host. `native_id` already carries the container name, and the two are
-- separate columns because they have separate lifetimes: stopping a session
-- keeps the volume, and destroying a machine whose disk is being preserved
-- removes the container and keeps it. NULL for every cloud machine, whose
-- disk is part of the instance flyco already names.
CREATE TABLE machines_new (
    id                            TEXT    PRIMARY KEY,
    session_id                    TEXT    NOT NULL UNIQUE REFERENCES sessions(id),
    provider_account_id           TEXT    NOT NULL REFERENCES provider_accounts(id),
    provider                      TEXT    NOT NULL
        CHECK (provider IN ('azure', 'aws', 'gcp', 'host')),
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
    volume_name                   TEXT
);

INSERT INTO machines_new
    (id, session_id, provider_account_id, provider, machine_type, region, disk_gib,
     requested_spot, spot, state, hourly_micros, native_id, address, created_at_unix,
     storage_hourly_micros, compute_meter_started_at_unix, compute_metered_at_unix,
     storage_meter_started_at_unix, storage_metered_at_unix, vcpus, memory_mib,
     minimum_hours, minimum_charge_micros)
SELECT id, session_id, provider_account_id,
       CASE provider WHEN 'byo_ssh' THEN 'host' ELSE provider END,
       machine_type, region, disk_gib, requested_spot, spot, state, hourly_micros,
       native_id, address, created_at_unix, storage_hourly_micros,
       compute_meter_started_at_unix, compute_metered_at_unix,
       storage_meter_started_at_unix, storage_metered_at_unix, vcpus, memory_mib,
       minimum_hours, minimum_charge_micros
FROM machines;

DROP TABLE machines;

ALTER TABLE machines_new RENAME TO machines;

CREATE INDEX machines_by_account ON machines (provider_account_id, state);
