-- Production compute and persistent-storage metering.
--
-- Each cursor is advanced only after the corresponding uniquely-keyed ledger
-- event exists. A scheduled invocation retried after an interruption can
-- therefore repeat a window without charging it twice.

ALTER TABLE machines ADD COLUMN storage_hourly_micros INTEGER;
ALTER TABLE machines ADD COLUMN compute_meter_started_at_unix INTEGER;
ALTER TABLE machines ADD COLUMN compute_metered_at_unix INTEGER;
ALTER TABLE machines ADD COLUMN storage_meter_started_at_unix INTEGER;
ALTER TABLE machines ADD COLUMN storage_metered_at_unix INTEGER;

ALTER TABLE spend_events ADD COLUMN meter_key TEXT;
CREATE UNIQUE INDEX spend_events_by_meter_key
    ON spend_events (meter_key) WHERE meter_key IS NOT NULL;

CREATE TABLE budget_signals (
    id              TEXT    PRIMARY KEY,
    budget_id       TEXT    NOT NULL REFERENCES budgets(id),
    session_id      TEXT    NOT NULL REFERENCES sessions(id),
    signal          TEXT    NOT NULL
        CHECK (signal IN ('notice50', 'warn80', 'final_warn90', 'pause')),
    ordinal         INTEGER NOT NULL CHECK (ordinal BETWEEN 1 AND 4),
    delivered       INTEGER NOT NULL DEFAULT 0 CHECK (delivered IN (0, 1)),
    created_at_unix INTEGER NOT NULL,
    UNIQUE (budget_id, signal)
);

CREATE INDEX budget_signals_pending
    ON budget_signals (delivered, created_at_unix, ordinal, id);
