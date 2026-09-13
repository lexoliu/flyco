-- Flyco control plane, issue #265: `Idempotency-Key` on `POST /v1/sessions`.
--
-- An agent that retries a create whose response was lost would otherwise
-- provision a second billed machine: the request is not idempotent by
-- nature and the caller cannot tell "never arrived" from "arrived and the
-- answer did not come back". A presented key claims the create under
-- `(user_id, key)` before any session row exists; `session_id` is NULL
-- while that create is in flight (a second caller under the same key is
-- told to wait and reconcile) and set once the session exists (a second
-- caller gets that session replayed).
--
-- Rows are deleted lazily at claim time once they are a day old — the
-- dedup window the API documents — so the table never needs a sweeper.
CREATE TABLE idempotency_keys (
    user_id         TEXT    NOT NULL REFERENCES users(id),
    key             TEXT    NOT NULL,
    session_id      TEXT    REFERENCES sessions(id),
    created_at_unix INTEGER NOT NULL,
    PRIMARY KEY (user_id, key)
);
