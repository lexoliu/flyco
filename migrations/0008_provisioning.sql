-- Flyco control plane, milestone M4: a provision that failed says so.
--
-- Until now a session that could not get a machine simply stayed in
-- `provisioning` for ever, because there was no other state to put it in and
-- nowhere to record what went wrong. `sessions.state` carries no CHECK — it
-- never has — so the new `failed` token needs no constraint change here; what
-- it needs is somewhere to keep the reason.
--
-- Nullable, and NULL for every state but `failed`: a reason attached to a
-- running session would be a stale sentence from a provisioning attempt that
-- was later retried and succeeded. The queue consumer clears it on every new
-- attempt for exactly that reason.

ALTER TABLE sessions ADD COLUMN failure_reason TEXT;
