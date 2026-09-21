-- Plugin marketplaces a user added: GitHub repositories carrying
-- `.claude-plugin/marketplace.json`, which is where the skill catalog reads
-- its skills from. `anthropics/skills` is built in for everyone and is
-- deliberately not a row here — it cannot be removed, and a row per user
-- saying so would be the same fact written once per account.
--
-- `git_ref` is NULL for "whatever the repository's default branch is",
-- which is what an unpinned marketplace follows.
CREATE TABLE marketplaces (
    id            TEXT    PRIMARY KEY,
    user_id       TEXT    NOT NULL REFERENCES users(id),
    repo          TEXT    NOT NULL,
    git_ref       TEXT,
    added_at_unix INTEGER NOT NULL,
    UNIQUE (user_id, repo)
);
