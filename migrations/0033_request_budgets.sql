-- Flyco control plane, issue #342: the per-principal request ledger.
--
-- One row per principal per UTC day, holding how many requests the control
-- plane has answered for it. The Worker charges it in batches from a
-- per-isolate tally — `flyco_api::request_budget` — so the ledger costs one
-- D1 write per few dozen requests rather than one per request, and refuses
-- a principal for the rest of the day once its ceiling is reached.
--
-- `principal` is the typed key the Worker builds: `user:<id>`,
-- `session:<id>`, `host:<id>` or `ip:<address>`. The cron drops rows older
-- than a week; nothing reads history.
CREATE TABLE request_budgets (
    principal TEXT    NOT NULL,
    day       INTEGER NOT NULL,
    requests  INTEGER NOT NULL,
    PRIMARY KEY (principal, day)
);
