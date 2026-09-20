---
name: flyco
description: Drive Flyco cloud coding sessions through the `flyco` CLI. Use when offloading a task to a cloud-hosted Claude Code or Codex session — creating, messaging, watching, approving, and stopping sessions — or when a job needs a full remote machine rather than local edits. Covers the agent surface (`flyco run`, `flyco session …`); the interactive TUI path is for humans only.
---

# Flyco

Flyco runs Claude Code and Codex sessions on cloud machines. The `flyco` CLI is the whole surface: REST for commands, one SSE stream for push, JSON everywhere a pipe is involved.

## When to use flyco

- The task wants a dedicated machine — a full checkout, long-running builds, services, a real shell — rather than local edits.
- The work should outlive this agent's context: a session persists on disk and can be resumed later.
- Two harnesses exist: `--harness claude` (Claude Code) or `--harness codex` (Codex). Pick per task; both run the harness's real agent, not an emulation.
- **`flyco run` vs `flyco session`**: `run` is the one-shot — create, stream the turn's events as JSONL, exit when the turn ends. `session …` is the primitive set for anything multi-step: create once, then `send`, `exec`, `wait`, `approve` over the session's life. Prefer `run` for fire-and-forget tasks; drop to `session` when you need to inspect, steer mid-turn, or reuse the machine.
- The human path (`flyco claude`, `flyco codex`, `flyco resume`, bare `flyco`) opens a native TUI bridged to a terminal. It requires a TTY and is **not for agents** — do not invoke it.

## Auth

A credential is an `fk_` API key, minted in Flyco's Settings. Provide it one of two ways:

- `FLYCO_TOKEN=fk_…` in the environment — preferred for agents; no file is involved.
- `flyco login --token fk_…` — validates and stores the key at `~/.config/flyco/credentials.json`.

`FLYCO_API_URL` points the CLI at a non-production control plane (default `https://flyco.dev`). `flyco auth status --json` reports who the credential authenticates as; every command exits `3` when no credential resolves.

## Output contract

- `--json` is a global flag; **stdout not being a TTY forces JSON anyway**, so on a pipe you always get documents, never tables.
- Single commands emit one JSON document per line; `run` and `events --follow` emit JSONL streams.
- Failures write to stderr and carry the server's `application/problem+json` body verbatim — parse it for `type`/`title` rather than matching message text.

## Commands

All agent commands share the session-spec flags where noted: `--repo owner/name` (required), `--branch`, `-p/--prompt` or `--prompt-file` (a prompt is required — a session is born with a goal; `-` or piped stdin reads it), `--budget DOLLARS` (required), `--machine/--account/--region` (all three together, or omit for automatic placement), `--spot`/`--on-demand`, `--disk GIB`, `--model`, `--effort`, `--permission-mode`, `--env KEY=VALUE` (repeatable), `--idempotency-key KEY`.

### flyco run — one prompt, one session, one turn

```sh
flyco run --harness claude --repo owner/name --budget 5.00 \
  -p "Fix the flaky test in crates/api" --stop --timeout 30m
```

Streams the session's events as JSONL, then emits one synthesized closing line:

```json
{"type":"run_result","outcome":"idle","session_id":"ses_…"}
```

`outcome` is the wait condition that ended the run: `idle`, `approval`, `paused`, `failed`, `stopped`. Flags: `--detach` (print the session document and return immediately), `--stop` (release the machine when the turn ends), `--archive`, `--timeout`. On `--idempotency-key`, a retried invocation within 24h returns the same session instead of billing a second machine.

### flyco session …

```sh
flyco session list --json                        # all sessions, newest first
flyco session list --state active
flyco session get <id>
flyco session create --harness codex --repo o/n --budget 2.50 -p "…"
flyco session send <id> -m "also update the README"   # or -f file, --stdin / piped
flyco session exec <id> -- cargo test -p api          # exit code = remote status
flyco session events <id> [--after N] [--follow]
flyco session wait <id> --for idle,approval,failed [--timeout 10m]
flyco session set <id> --budget 10 --permission-mode accept-edits
flyco session approvals <id> --pending
flyco session approve <id> <approval> --allow|--deny
flyco session diff <id>            # working tree vs base branch
flyco session files <id> [dir]     # list the checkout
flyco session read <id> <path>     # print one file
flyco session env <id> [--set K=V …]
flyco session interrupt <id>       # stop the turn, keep the machine
flyco session stop <id>            # release the machine, keep the disk
flyco session resume <id>          # back onto a machine
flyco session archive <id> [--discard-uncommitted]
```

`wait --for` conditions: `active`, `idle`, `approval`, `paused`, `stopped`, `failed`, `archived`. The first to occur wins; the matched name is the JSON answer and the exit code carries it too (see below).

Discovery: `flyco catalog`, `flyco repos`, `flyco branches <o/n>`, `flyco harnesses` — all JSON-capable.

## Event stream

`flyco run` and `flyco session events --follow` emit JSONL, one event per line. Two document shapes appear:

- **Recorded history** (catch-up pages): `{"seq":N,"event":{…},"at_unix":…}` — a `StoredEvent`; `event` is untyped so variants newer than this build still pass through.
- **Live** (the SSE tail): `{"session":"ses_…","seq":N|null,"at_unix":…,"event":{…}}` — a `SessionEvent` envelope; `seq` is null for live-only events that are never recorded (terminal output, machine connection changes).

Every consumer reads `.event`; the `type` field inside it discriminates. The schema source of truth is `ClientEvent` in `flyco_core::wire` — treat unknown variants as pass-through, never as errors.

## Lifecycle etiquette

Machines bill while they live. Always:

- Set `--budget` — it is required, and it is the hard stop when a turn runs away.
- Prefer `run --stop` (or `--archive`) for one-shot tasks so the machine releases itself.
- For `session` flows, `flyco session stop <id>` when done; `interrupt` only pauses the turn — the machine is still up and still billing.
- `wait` and `run` returning `approval` (exit 7) means the agent is blocked on a decision — list with `session approvals --pending`, decide with `session approve … --allow|--deny`, then `send` to continue.
- `paused` (exit 8) means budget exhausted or machine reclaimed — resolve the pause (`session set --budget`, then `session resume`) rather than retrying the same call.

## Quota etiquette

The control plane is one Cloudflare Worker on a free-plan daily quota shared by every user, and it charges every request to the caller: a session daemon, a user, or an address that loops at line rate is refused with `429` for the rest of the UTC day, and a loop that outruns the account's ceiling takes every session down with it. So:

- Every wait names its timeout: `flyco run --timeout`, `flyco session wait --timeout`. Never poll `session get` or `session events` in a shell loop — one `wait` or one `--follow` per session, and never two `--follow` on the same session.
- Exit 5 with a `429` problem is a wait, not an error: read `Retry-After` from the problem output and do nothing to that session until it has passed. `request-budget-exhausted` names hours — stop and report it; do not retry, switch keys, or open another session to continue.
- Stop what you start: `session stop` or `run --stop`, and no `wait`, `--follow` or `run` left running after the task ends.
- No load tests, soak tests or end-to-end loops against `dev.flyco.dev`; a live check is one session, one prompt, one wait.

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | Success; `wait`/`run` matched `active`, `idle`, or `archived` |
| 2 | Usage — a flag was missing or malformed |
| 3 | Auth — no credential, refused credential, denied login approval |
| 4 | Not found or conflict (404/409/410) |
| 5 | The control plane answered a problem document |
| 6 | Transport — no answer arrived (connection, timeout, dropped stream) |
| 7 | An approval is pending |
| 8 | The session is paused |
| 9 | The session failed or stopped |
| 10 | The caller's `--timeout` elapsed |
| other | A remote command's own status — `session exec` and the TUI bridge pass it through verbatim, `ssh`-style |

Retry guidance: 6 is always retryable; 5 retries only on what the problem document says (429/5xx honored `Retry-After` upstream); 2/3/4 are never retryable unchanged; 7/8/9/10 are outcomes — branch on them, don't retry them.
