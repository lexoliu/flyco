-- Flyco control plane, milestone M?: GitHub grants that renew themselves.
--
-- An OAuth app that expires user tokens (8h) answers the code exchange with
-- a refresh_token beside the access token, and until now flyco sealed only
-- the access half — so every grant died overnight and every morning's first
-- call surfaced a "reconnect GitHub" the user should never have been asked
-- for. The renewal credential and the grant's end are stored beside the
-- token so `users::github_token` can refresh the grant before handing it
-- out.
--
-- Both columns stay NULL for grants GitHub does not expire — and for every
-- row written before this migration, which is the same thing from the
-- caller's side: a grant with no refresh credential is used until GitHub
-- refuses it, and the next sign-in stores a renewable one.

ALTER TABLE users ADD COLUMN github_refresh_token_enc TEXT;
ALTER TABLE users ADD COLUMN github_token_expires_at_unix INTEGER;
