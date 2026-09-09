-- Flyco control plane, issue #235: a machine is a virtual machine or a
-- managed container, and the difference is whether its filesystem survives
-- a stop.
--
-- `runtime` is on `machines` rather than derived from `provider`, because
-- the same subscription sells both: an Azure account offers `Standard_D4s_v6`
-- as a VM and `aca-4x8` as a Container Apps job, and the driver that reaches
-- them is the same driver. It is the fact `flyco_provider::flycod` writes
-- into the daemon's configuration, and the one that decides what the daemon
-- does with the platform's SIGTERM: a VM shuts down onto a disk that is
-- still there, and a container has about thirty seconds to get the working
-- tree off the machine as a `workdir-patch`.
--
-- `DEFAULT 'vm'` backfills every existing row, and that is a statement of
-- fact rather than a convenience: before this migration flyco provisioned
-- virtual machines and nothing else. NOT NULL because every machine is one
-- or the other and "we do not know" is not a state a provisioner can be in.
ALTER TABLE machines ADD COLUMN runtime TEXT NOT NULL DEFAULT 'vm'
    CHECK (runtime IN ('vm', 'container'));

-- When this machine's own daemon said it was going, and what made it go.
--
-- Written by `POST /v1/sessions/{id}/stopping`, which a container session's
-- flycod files after it has flushed the transcript and stored the working
-- tree — so a row carrying this instant is one whose session is safe to
-- resume elsewhere. Cleared the moment the machine is acted on again,
-- because a machine that has just been started is no longer mid-stop.
--
-- Two columns rather than one: an instant answers "is this execution on its
-- way out", which is what a container driver checks before it treats a
-- vanished execution as a failure, and the reason answers "why", which is
-- what tells a platform stop from a run that reached its timeout. Both
-- nullable, and NULL is the ordinary state: a machine that has never been
-- asked to stop has no instant and no reason, and a placeholder would be an
-- explanation for something that did not happen.
ALTER TABLE machines ADD COLUMN stopping_since_unix INTEGER;
-- `IN` with no `IS NULL` beside it: a CHECK passes when its expression is
-- NULL, which is exactly the ordinary state here.
ALTER TABLE machines ADD COLUMN stopping_reason TEXT
    CHECK (stopping_reason IN ('sigterm'));
