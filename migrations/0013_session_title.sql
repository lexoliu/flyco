-- A session is named and remembers who chose its machine.
--
-- `title` opens as the excerpt of the session's first prompt and is
-- editable through `PATCH /v1/sessions/{id}`. The empty-string default
-- exists only because SQLite requires one to add a NOT NULL column to a
-- table that already has rows; every row written after this migration
-- carries a real title.
ALTER TABLE sessions ADD COLUMN title TEXT NOT NULL DEFAULT '';

-- `machine_origin` records whether flyco picked the machine or the user
-- did. The two are not interchangeable afterwards — a machine the user
-- chose is a decision flyco must not quietly undo — and the fact is
-- unrecoverable from anywhere else once the request that carried it is
-- gone. The CHECK lists exactly the tokens flyco_core::MachineOrigin
-- serializes to, so the schema and the domain model cannot drift apart.
ALTER TABLE sessions ADD COLUMN machine_origin TEXT NOT NULL DEFAULT 'auto'
    CHECK (machine_origin IN ('auto', 'user'));
