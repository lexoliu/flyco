-- Flyco control plane, milestone M3b: pairing a session with its daemon.
--
-- `daemon_token_hash` is the lowercase hex SHA-256 of the full presented
-- token *including* its `fd_` prefix, matching how `api_keys.token_hash`
-- stores an API key. It is nullable because a session exists before any
-- daemon is paired to it, and minting a new token overwrites the column, so
-- a session has at most one live daemon credential.
--
-- The `machines` table does not appear here: provisioning owns it, and that
-- is M4. Until then a daemon is paired by hand with
-- `POST /v1/sessions/{id}/daemon-token`.

ALTER TABLE sessions ADD COLUMN daemon_token_hash TEXT;
