-- Harness-native session identity, recorded when the daemon announces
-- itself so a later resume can reopen the same conversation on a new
-- machine.

ALTER TABLE sessions ADD COLUMN harness_session_id TEXT;
