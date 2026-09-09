-- Flyco control plane, issue #228: the model a session runs on becomes a
-- session property instead of whatever the CLI happens to default to.
--
-- Two columns rather than one, because a model and an effort are two
-- answers: a model the harness offers with no effort levels at all (Claude's
-- Haiku row) can never carry one, and a session that chose a model without
-- choosing an effort is leaving the harness's own default in place rather
-- than picking a level called "default". Packing the pair into one string
-- would make that distinction a parsing convention.
--
-- Nullable, and NULL is a real answer rather than a gap to fill in: every
-- session opened before this migration ran on the harness's own default, so
-- `model IS NULL` reads back as
-- `ModelChoice::default_of(builtin_models(harness))` — today's default of
-- the harness that session runs, resolved where the row is read. Backfilling
-- a literal would freeze one afternoon's default into rows nobody chose it
-- for, and the harness that names its own default is the only authority on
-- what those sessions were actually running.
--
-- `harness_accounts.models_json` is the whole list, as the account's last
-- session's harness answered it, serialized as a JSON array of
-- flyco_core::ModelOption. A column rather than a table because nothing
-- queries it: it is read whole, replaced whole, and is a cache of a fact
-- the harness owns. NULL means no session of this account has reported yet,
-- and the built-in list stands.
ALTER TABLE sessions ADD COLUMN model TEXT;
ALTER TABLE sessions ADD COLUMN effort TEXT;
ALTER TABLE harness_accounts ADD COLUMN models_json TEXT;
