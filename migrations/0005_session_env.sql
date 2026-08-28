-- Flyco control plane, milestone M3c: a session's environment.
--
-- One row per session, holding the whole `.env` rather than one row per
-- variable: `PUT /v1/sessions/{id}/env` replaces the document, the order the
-- entries were given in is part of it, and a set of rows would need a
-- position column and a delete-then-insert to say the same thing — in a
-- database with no transactions to make the pair atomic.
--
-- Every value is a secret the user pasted in, so the document is sealed by
-- `flyco_api::crypto::TokenCipher` before it is written and the column is
-- named `*_enc` like every other sealed value in this schema. That also
-- keeps the values out of any query log: what a statement carries is
-- ciphertext.

CREATE TABLE session_env (
    session_id      TEXT    PRIMARY KEY REFERENCES sessions(id),
    entries_enc     TEXT    NOT NULL,
    updated_at_unix INTEGER NOT NULL
);
