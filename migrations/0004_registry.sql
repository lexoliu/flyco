-- Flyco control plane, milestone M2c: the registries behind the frozen REST
-- contract.
--
-- The handlers over these tables are still `todo!()` — this milestone locks
-- the API surface, not its behaviour — but the schema lands now so migration
-- numbering stays stable and the milestones that fill the handlers in do not
-- each have to renumber around each other.
--
-- Every CHECK below lists exactly the snake_case tokens the matching
-- flyco_core enum serializes to, the same discipline 0002 established, so the
-- schema and the domain model cannot drift apart silently. Booleans are
-- INTEGER 0/1 with a CHECK, because SQLite and D1 have no boolean type and an
-- unconstrained column would eventually hold a 2.
--
-- Secrets follow the rules the auth stack already set: anything flyco issues
-- is stored as a SHA-256 (`push_subscriptions` needs none — its keys are the
-- browser's, not flyco's), and anything a third party issued is stored sealed
-- by `flyco_api::crypto::TokenCipher`, in a column named `*_enc`.

-- Cloud provider accounts. `credentials_enc` is the sealed JSON encoding of
-- flyco_core::providers::ProviderCredentials, whose own tag names the
-- provider; `kind` repeats it as a column so listings and the usage panel can
-- filter without unsealing anything.
CREATE TABLE provider_accounts (
    id              TEXT    PRIMARY KEY,
    user_id         TEXT    NOT NULL REFERENCES users(id),
    kind            TEXT    NOT NULL CHECK (kind IN ('azure', 'aws', 'gcp', 'byo_ssh')),
    label           TEXT    NOT NULL,
    credentials_enc TEXT    NOT NULL,
    linked_at_unix  INTEGER NOT NULL
);

-- Claude and Codex accounts, linked through each vendor's own authorization
-- page. `token_enc` is the sealed credential; it is provisioned onto session
-- machines and never returned by the API.
CREATE TABLE harness_accounts (
    id              TEXT    PRIMARY KEY,
    user_id         TEXT    NOT NULL REFERENCES users(id),
    harness         TEXT    NOT NULL CHECK (harness IN ('claude_code', 'codex')),
    label           TEXT    NOT NULL,
    token_enc       TEXT    NOT NULL,
    linked_at_unix  INTEGER NOT NULL,
    expires_at_unix INTEGER
);

-- The MCP registry. `config` is the JSON encoding of
-- flyco_core::mcp::McpServerConfig, tagged by transport. A name is unique per
-- user because that is the name the harness announces the server under, and
-- two servers answering to one name is a collision on the machine.
CREATE TABLE mcp_servers (
    id              TEXT    PRIMARY KEY,
    user_id         TEXT    NOT NULL REFERENCES users(id),
    name            TEXT    NOT NULL,
    config          TEXT    NOT NULL,
    enabled         INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    updated_at_unix INTEGER NOT NULL,
    UNIQUE (user_id, name)
);

-- Skills. The zip itself lives in R2 under `skills/{id}.zip`; this table is
-- the index. `scope` picks the harness whose global skills directory the
-- bundle is installed into, which is why the same name may exist twice.
CREATE TABLE skills (
    id               TEXT    PRIMARY KEY,
    user_id          TEXT    NOT NULL REFERENCES users(id),
    name             TEXT    NOT NULL,
    scope            TEXT    NOT NULL CHECK (scope IN ('claude', 'codex')),
    size_bytes       INTEGER NOT NULL,
    uploaded_at_unix INTEGER NOT NULL,
    UNIQUE (user_id, scope, name)
);

-- Tree memory. `parent_id` is self-referential and NULL for a root; `repo` is
-- NULL for memory that applies wherever the user's agents run. The tree is
-- walked top-down, one level per request, so the read index is (user, repo,
-- parent).
CREATE TABLE memory_nodes (
    id              TEXT    PRIMARY KEY,
    user_id         TEXT    NOT NULL REFERENCES users(id),
    parent_id       TEXT    REFERENCES memory_nodes(id),
    repo            TEXT,
    title           TEXT    NOT NULL,
    content         TEXT    NOT NULL,
    updated_at_unix INTEGER NOT NULL
);

-- Web push subscriptions (RFC 8030). The endpoint is unique because a browser
-- re-subscribing produces the same one, and a duplicate row would send every
-- notification twice.
CREATE TABLE push_subscriptions (
    id                  TEXT    PRIMARY KEY,
    user_id             TEXT    NOT NULL REFERENCES users(id),
    endpoint            TEXT    NOT NULL UNIQUE,
    p256dh              TEXT    NOT NULL,
    auth                TEXT    NOT NULL,
    expiration_time_ms  INTEGER,
    created_at_unix     INTEGER NOT NULL
);

-- The shared AGENTS.md. Exactly one document per user, so the user id is the
-- primary key rather than a column beside a surrogate one.
CREATE TABLE agents_md (
    user_id         TEXT    PRIMARY KEY REFERENCES users(id),
    content         TEXT    NOT NULL,
    updated_at_unix INTEGER NOT NULL
);

CREATE INDEX provider_accounts_by_user ON provider_accounts (user_id, kind);
CREATE INDEX harness_accounts_by_user ON harness_accounts (user_id, harness);
CREATE INDEX mcp_servers_by_user ON mcp_servers (user_id, name);
CREATE INDEX skills_by_user_scope ON skills (user_id, scope, name);
CREATE INDEX memory_nodes_by_parent ON memory_nodes (user_id, repo, parent_id);
CREATE INDEX push_subscriptions_by_user ON push_subscriptions (user_id);
