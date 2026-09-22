# Flyco repository notes

## Product invariant: browser-only users

Assume the user has **only a browser** — no local terminal, no CLI, nothing
installed on their machine. Every user-facing flow must be completable from
the PWA alone:

- Account linking is paste-a-key or an OAuth/device-code page, never "run
  `x login` locally and paste the result back". A vendor whose OAuth
  client admits only localhost redirects gets its CLI's manual-code flow
  (the page shows the code, the user pastes the code), never a dead
  `127.0.0.1` redirect the user copies out of the browser's error page.
- No flow may ask the user to execute a command in their own terminal and
  copy output into flyco.
- Session inspection, approvals, diffs, files, env, terminal — all through
  the web UI; the `flyco` CLI and `flycod` are agent/daemon surfaces, not
  user prerequisites.

## Platform invariant: the control plane runs on a free-plan quota

The control plane (`crates/api`) is one Cloudflare Worker on the free plan.
Its ceilings are **account-wide and daily**: 100k Worker requests, 100k
Durable Object requests, 5M Durable Object row reads, 100k D1 row writes,
100k KV reads. When one is spent, every session of every user is down
until UTC midnight. Four incidents have done exactly that, each through
one client loop running at line rate: a catalog computed inside the
request (#174), a CLI gap-fill re-reading a page every 400 ms (#288), two
daemons ping-ponging `relay/attach` at 3 req/s (#336), and a Devin session
that spent the day's Worker usage (#342).

The control plane now defends itself — `crates/api/src/request_budget.rs`
charges every request to a principal (user, session daemon, host, or
address), refuses a principal over its per-minute limit before any storage
is read, and refuses one over its daily ceiling until midnight, always with
`429` and a `Retry-After` header; `row_budget.rs` does the same for
Durable Object row reads. Those are circuit breakers for the bug we have
not written yet. The rules below are what keep us from writing it.

### Rules for code that talks to the control plane

Every client — `flycod` (`crates/daemon`), the `flyco` CLI (`crates/cli`),
the PWA (`frontend/`), and anything new — obeys these, and a PR that adds
or changes a loop says in its description which line satisfies each:

1. **Every reconnect or retry loop backs off**: capped exponential with
   full jitter (`crates/daemon/src/control/wire.rs::backoff` is the
   reference), and the ladder resets only after a connection *held* for
   `ATTACH_STABLE`, never on the open — a stream that opens and dies in a
   second must climb the ladder, not stay at its floor.
2. **`429` is a wait, not a transport error.** Read `Retry-After` and
   sleep at least that long before the next request to that route. A
   `request-budget-exhausted` problem names a wait of hours: stop, surface
   it to the user, do not switch credentials or hosts to get around it.
3. **Every pagination walk has a page cap and stops when the cursor does
   not advance.** A server answering `more: true` for ever must cost a
   bounded number of requests.
4. **Nothing polls the control plane on a timer without an end.** A poll
   states its interval (never under two seconds) and the condition or
   deadline that stops it. Liveness is the server's SSE `ping`, never a
   client request.
5. **One connection per stream, one daemon per session.** A second
   `flycod` on a machine exits (the lock file); a second `--follow` on the
   same session is a bug.
6. **Frames and events are batched**, never one request per item.

### Rules for the control plane itself

1. A new route's doc comment states what it costs per call: D1 statements
   (reads and writes), KV operations, Durable Object calls, R2 operations,
   subrequests. A cost that scales with data size says so.
2. **No unconditional write per request.** A stamp that only needs to be
   right to the hour is written once an hour (`api_keys::mark_used` is the
   pattern).
3. A Durable Object read that can touch many rows bills
   `row_budget::charge_reads`; a one-row keyed lookup does not.
4. Work that is periodic runs in the minute cron under a CPU cap
   (`metering::MAX_WINDOWS_PER_SWEEP` is the pattern), never inside a
   request, and never fans out one request into one call per row.
5. The ceilings live in one place — `request_budget::Limits::PRODUCTION`,
   pinned to `Skyzen.toml` by a test — and a PR that raises one carries
   the arithmetic: what a client at the new limit spends in a day, against
   the account's 100k.

### Rules for an agent driving the deployed control plane

`dev.flyco.dev` is production for quota purposes. An agent — Devin,
Claude, Codex, any of them — that uses it during a task:

1. Drives sessions through the `flyco` CLI (`skills/flyco/SKILL.md`),
   never a shell loop of `curl`, `flyco session get`, or the events route.
   Waiting is `flyco session wait --timeout` or `flyco run --timeout`, one
   process per session, and every wait names its timeout.
2. Never runs a load test, a soak test, a benchmark, or an end-to-end loop
   against the deployed control plane. Behaviour is tested against the
   in-process router (`cargo nextest run -p flyco-api`) or a local
   `workerd`; a live check is one session, one prompt, one wait.
3. On a `429`, stops. Reads `Retry-After`, reports the problem type and
   the wait, and does not retry, rotate credentials, or open another
   session to continue.
4. Never starts a second daemon on a session's machine and never leaves a
   `--follow`, a `wait`, or a `run` running after its task ends.
5. When traffic looks wrong — a session answering slowly, a `502`, a burst
   in `wrangler tail` — stops the client first (stop the session, destroy
   the machine, kill the process) and diagnoses second. Workers Logs keep
   the evidence; a loop left running to "observe it" spends the day.

## Interface invariant: the design teaches, the copy does not

A good screen is understood at a glance. Explanatory sentences under a
control are the design failing and then apologising, so they are not
written:

- No sentence that explains what a control does when the control can say
  it itself. `Spot capacity — cheaper, and flyco handles eviction`,
  `A container your provider gives away this month, which flyco spends
  before it spends money.`, `Choosing a machine yourself is remembered
  with the session, and the agent is told you picked it.` are all banned,
  in that wording and in any other.
- No pile of facts joined by `·` standing in for a layout. Each fact gets
  its own place in the row or the line, in the order it is read.
- A collapsed control shows only what the choice turns on — for a machine
  that is cores and memory, not price. Secondary facts appear when it is
  open, laid out, not concatenated.
- No label that repeats the heading above it, and no hint that repeats the
  label beside it.

The test is the first-time user: if they could not work the screen without
the sentence, fix the screen.
