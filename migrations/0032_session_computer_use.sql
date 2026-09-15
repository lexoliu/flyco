-- Flyco control plane, issue #262: whether a session may have a screen.
--
-- Computer use is a session property rather than a machine's: the flag
-- decides what the provisioned `flycod` configuration says — the
-- `[computer]` table — and what the daemon starts when a `SetComputerUse`
-- command reaches a session already running. A session that gains a screen
-- while live gets it without a reboot, which is why the row, and not the
-- machine's configuration alone, is the answer a provisioning job reads.
--
-- NOT NULL DEFAULT 0 rather than NULL-with-resolution: unlike `model` and
-- `permission_mode`, a row written before this column existed has no legacy
-- meaning to preserve — it was simply a session without a screen, and 0
-- says exactly that.
ALTER TABLE sessions ADD COLUMN computer_use INTEGER NOT NULL DEFAULT 0
    CHECK (computer_use IN (0, 1));
