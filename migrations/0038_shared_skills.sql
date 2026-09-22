-- Flyco control plane: a skill belongs to the user, not to a harness.
--
-- `skills.scope` picked which of the harnesses' global skills directories
-- a bundle was installed into — `claude` or `codex`, and the same name
-- could exist once per scope. The daemon mounts the same set into every
-- harness's directory now, so a skill is unique per user and name and the
-- column is gone.
--
-- SQLite cannot drop a column's `CHECK` or its place in a `UNIQUE`, so
-- the table is rebuilt beside the old one: a name that was uploaded for
-- two scopes folds into one row, and the newest upload is the one that
-- survives — its id keeps naming the bundle in R2, so no object moves.
CREATE TABLE skills_new (
    id               TEXT    PRIMARY KEY,
    user_id          TEXT    NOT NULL REFERENCES users(id),
    name             TEXT    NOT NULL,
    size_bytes       INTEGER NOT NULL,
    uploaded_at_unix INTEGER NOT NULL,
    UNIQUE (user_id, name)
);

INSERT INTO skills_new (id, user_id, name, size_bytes, uploaded_at_unix)
SELECT id, user_id, name, size_bytes, uploaded_at_unix
FROM skills
WHERE rowid IN (
    SELECT rowid FROM skills AS newer
    WHERE newer.user_id = skills.user_id AND newer.name = skills.name
    ORDER BY newer.uploaded_at_unix DESC, newer.id DESC
    LIMIT 1
);

DROP TABLE skills;

ALTER TABLE skills_new RENAME TO skills;

CREATE INDEX skills_by_user ON skills (user_id, name);
