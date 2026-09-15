-- `flyco handoff`: a session created from a local harness session's
-- working state. One row per handoff, keyed by the cloud session it
-- feeds.
--
-- The row is created with the session (CreateSession.source) holding
-- only the provenance the sender declared; `completed_at_unix` stays
-- NULL until `handoff/complete` verifies the uploaded objects. That
-- NULL is also the provisioning gate: a session whose handoff is
-- pending has a reserved machine but no queued job, and the stall
-- sweep must leave it alone — the payload is uploaded by a client on
-- the user's own network, not on a provisioning clock.
--
-- Checksums live here rather than beside the objects so `complete` can
-- verify declared-against-stored without reading the payloads back, and
-- the daemon can verify the patch before `git apply` without trusting
-- the sender's manifest.

CREATE TABLE handoffs (
    session_id          TEXT PRIMARY KEY REFERENCES sessions(id),
    source_harness      TEXT NOT NULL,
    harness_session_id  TEXT NOT NULL,
    base_commit         TEXT NOT NULL,
    local_workdir       TEXT NOT NULL,
    patch_sha256        TEXT,
    patch_bytes         INTEGER,
    transcript_sha256   TEXT,
    transcript_bytes    INTEGER,
    created_at_unix     INTEGER NOT NULL,
    completed_at_unix   INTEGER
);
