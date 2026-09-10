-- Flyco control plane, issue #244: a session pauses itself when the harness
-- plan's window is spent and continues itself when the window turns over.
--
-- `paused_reason` is why a session is `state = 'paused'`. The state alone
-- says it is stopped on purpose and says nothing about what would start it
-- again, and the two answers send a reader to completely different places:
-- a spent budget is a number only the user can raise, and a spent plan
-- window is a wait flyco ends by itself (docs/ux.md §6). NULL means the
-- session is not paused at all, and — for the handful of rows this migration
-- runs over — that it was paused by a build older than this column, which is
-- always a budget pause, because it was the only kind there was.
--
-- The four `usage_limit_*` columns are that wait, and they mean nothing for
-- any other reason:
--
--   * `usage_limit_window` is the *label* of the window that struck —
--     `5-hour`, `Weekly (Opus)`. The label and not the whole reading: the
--     percentages behind the rings belong to the account and go on moving
--     while this session waits, so a copy frozen here would be a second
--     answer going stale. Which window is a name.
--   * `usage_limit_resets_at_unix` is when it turns over, which is when the
--     session is continued.
--   * `usage_limit_resume_at_unix` is when flyco starts the machine again,
--     ten minutes before that. NULL means the machine was never stopped —
--     every reset less than half an hour out keeps it, because stopping and
--     starting costs minutes at both ends and saves almost nothing — so
--     this one column answers both "when does it come back" and "is it
--     costing anything", and the two cannot disagree.
--   * `usage_limit_queued_message` is what the user typed into the composer
--     while the session was waiting. It is sent as the continuation instead
--     of the canned nudge: they had something to say, and saying it is a
--     better continuation than `usage limit reset, please continue`.
--
-- All four outlive `state = 'paused'`. The wake runs through
-- `provisioning`, the same state a brand-new session sits in, and these are
-- the only thing that tells that provisioning apart from a first one —
-- exactly the job `interrupted_reason` does for a spot reclamation. They
-- are cleared together when the continuation is sent.
--
-- Nullable with no default: a session that has never been paused has no
-- reason and no window, and a placeholder would explain something that did
-- not happen. The CHECK lists exactly the tokens
-- flyco_core::PausedReason serializes to, so the schema and the domain
-- model cannot drift apart.
ALTER TABLE sessions ADD COLUMN paused_reason TEXT
    CHECK (paused_reason IN ('budget', 'usage_limit'));
ALTER TABLE sessions ADD COLUMN usage_limit_window TEXT;
ALTER TABLE sessions ADD COLUMN usage_limit_resets_at_unix INTEGER;
ALTER TABLE sessions ADD COLUMN usage_limit_resume_at_unix INTEGER;
ALTER TABLE sessions ADD COLUMN usage_limit_queued_message TEXT;

-- The minute cron asks three questions of this table every minute — which
-- paused sessions still hold a machine, which are due to be woken, and
-- which are due to be continued — and every one of them is keyed on the
-- reason. Without an index each is a scan of every session flyco has ever
-- opened, sixty times an hour, for the sake of the handful that are
-- waiting. Partial, because the answer is only ever about those rows.
CREATE INDEX IF NOT EXISTS idx_sessions_usage_limit
    ON sessions (usage_limit_resets_at_unix)
    WHERE paused_reason = 'usage_limit';
