-- Flyco control plane, milestone M6: what flyco has observed of a harness
-- account's usage.
--
-- This table exists because neither Anthropic nor OpenAI publishes a
-- remaining-quota API. There is nothing to read, so there is nothing to
-- cache; the only honest panel is one built out of things that actually
-- happened, which means recording them as they happen. Every row is one
-- observation a session's daemon made — the cost the harness reported for a
-- turn, or the moment it hit its account limit — and `GET /v1/usage/llm` is
-- the sum of them, never a claim about what is left.
--
-- `harness_account_id` is derived by the control plane from the session's
-- own user and harness rather than named by the caller: a daemon token
-- resolves to a session, and a session cannot post observations against
-- somebody else's account if it never names one.
--
-- Both measurements are nullable because a row carries whichever of them
-- the daemon saw, and the CHECK is what keeps a row that carries neither out
-- of the table: an observation of nothing is not an observation.
--
-- `resets_at_unix` is only ever set alongside `rate_limited_at_unix`, which
-- the DTO enforces by shape — a reset time lives inside the rate-limit
-- observation, so there is nowhere to put one otherwise.
--
-- Costs are integer microdollars like every other amount in this schema.

CREATE TABLE harness_observations (
    id                   TEXT    PRIMARY KEY,
    user_id              TEXT    NOT NULL REFERENCES users(id),
    harness_account_id   TEXT    NOT NULL REFERENCES harness_accounts(id),
    session_id           TEXT    NOT NULL REFERENCES sessions(id),
    observed_cost_micros INTEGER,
    rate_limited_at_unix INTEGER,
    resets_at_unix       INTEGER,
    at_unix              INTEGER NOT NULL,
    CHECK (observed_cost_micros IS NOT NULL OR rate_limited_at_unix IS NOT NULL)
);

-- The panel reads one account's window, and separately its newest rate
-- limit; both are this index walked from the recent end.
CREATE INDEX harness_observations_by_account ON harness_observations (harness_account_id, at_unix);
