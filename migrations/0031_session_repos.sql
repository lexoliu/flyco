-- The repositories a session works across.
--
-- A session has always named exactly one repository in `sessions.repo`,
-- which left no room for the two ways a session grows past one: the user
-- picking several at creation, and the agent asking for one mid-session
-- (issue #280). `session_repos` holds the whole set — one row per
-- checkout, ordered by `position`, with the first row being the primary
-- the header names — and `repo`/`branch` leave `sessions` for it.
--
-- `dir` is the checkout's identity inside the workspace: the directory
-- under the session's workdir it is cloned into, and the name a diff
-- request, a dirty status or a stored patch refers to it by. It is
-- unique per session, which is what makes a directory name a key.
--
-- `branch` is nullable for the same reason `sessions.branch` was: a row
-- recorded before the branch could be resolved says NULL rather than a
-- guess, and the provisioning queue resolves the repository's default
-- branch and writes it back.
--
-- `added_by` records which of the two doors a repository came through:
-- 'user' for a pick at creation or a mid-session add, 'agent' for one the
-- agent asked for and the user approved. The distinction is kept because
-- the two carry different weight in the UI — a repository the user chose
-- and one they approved are both theirs, but they are not the same fact.
CREATE TABLE session_repos (
    session_id    TEXT    NOT NULL REFERENCES sessions(id),
    position      INTEGER NOT NULL,
    slug          TEXT    NOT NULL,
    branch        TEXT,
    dir           TEXT    NOT NULL,
    added_by      TEXT    NOT NULL CHECK (added_by IN ('user', 'agent')),
    added_at_unix INTEGER NOT NULL,
    PRIMARY KEY (session_id, slug)
);
CREATE INDEX session_repos_by_session ON session_repos (session_id, position);
CREATE UNIQUE INDEX session_repos_one_dir ON session_repos (session_id, dir);

-- Every session's one repository becomes its first `session_repos` row at
-- position 0. `dir` is the repository's own name — what a fresh session
-- would derive — and `added_at` borrows the session's birth rather than
-- claiming a second timestamp.
INSERT INTO session_repos (session_id, position, slug, branch, dir, added_by, added_at_unix)
SELECT id, 0, repo, branch, substr(repo, instr(repo, '/') + 1), 'user', created_at_unix
FROM sessions;

-- Neither column is indexed or constrained, so a plain DROP carries them
-- out — the rename-first rebuild of 0028 was for the CHECK lists SQLite
-- cannot alter, and these are not one.
ALTER TABLE sessions DROP COLUMN repo;
ALTER TABLE sessions DROP COLUMN branch;
