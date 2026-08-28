-- Flyco control plane, milestone M2a: the auth stack.
--
-- Applied to Cloudflare D1 with `wrangler d1 migrations apply flyco-main`, and
-- to the local SQLite file used by the native `skyzen dev` stack. Later
-- milestones add their own numbered files; this one is never edited again.
--
-- Every id is a UUID string. `api_keys.token_hash` is the lowercase hex
-- SHA-256 of the full presented key *including* its `fk_` prefix, so a lookup
-- hashes exactly the bytes the client sent.

CREATE TABLE users (
    id               TEXT    PRIMARY KEY,
    github_id        INTEGER NOT NULL UNIQUE,
    login            TEXT    NOT NULL,
    github_token_enc TEXT    NOT NULL,
    created_at_unix  INTEGER NOT NULL
);

CREATE TABLE api_keys (
    id              TEXT    PRIMARY KEY,
    user_id         TEXT    NOT NULL REFERENCES users(id),
    token_hash      TEXT    NOT NULL UNIQUE,
    label           TEXT    NOT NULL,
    created_at_unix INTEGER NOT NULL,
    last_used_unix  INTEGER
);

CREATE INDEX api_keys_by_user ON api_keys (user_id);
