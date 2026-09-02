-- What the machine a session runs on actually is, recorded on the row.
--
-- The agent's `machine_status` tool answers four questions about the
-- machine under it: what it is called, what it costs, how big it is, and
-- whether booting it already committed the user to a licence minimum. The
-- first two were already columns; the last two lived only in the provider's
-- catalog, and reading that catalog to answer a status call would be several
-- HTTPS round trips on every question — and would answer *nothing* for a
-- type curation has since dropped, or one the provider has stopped selling,
-- even though the machine is still running and still being billed.
--
-- So they are recorded when the machine is built and rewritten when it is
-- resized, alongside the price they belong with. All four are nullable
-- because all four are genuinely unknown for hardware the user registered
-- over SSH: flyco has not measured that machine and does not meter it.
ALTER TABLE machines ADD COLUMN vcpus INTEGER;
ALTER TABLE machines ADD COLUMN memory_mib INTEGER;

-- The floor the provider bills the moment the machine boots, when the type
-- imposes one — EC2 Mac's 24 hours under the Apple licence. Both halves,
-- because only one of them answers the question a person is asking: `24` is
-- a fact about a licence, and the charge is what pressing the button cost.
ALTER TABLE machines ADD COLUMN minimum_hours INTEGER;
ALTER TABLE machines ADD COLUMN minimum_charge_micros INTEGER;
