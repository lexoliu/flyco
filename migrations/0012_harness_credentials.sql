-- Harness credentials are tagged values rather than an untyped OAuth token.
-- Naming the encrypted column after what it contains prevents every reader
-- from silently assuming a credential mode that the database does not encode.

ALTER TABLE harness_accounts RENAME COLUMN token_enc TO credential_enc;

CREATE UNIQUE INDEX one_harness_account_per_user
ON harness_accounts (user_id, harness);
