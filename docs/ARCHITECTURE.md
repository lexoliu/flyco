# Flyco architecture

The product-level spec is [proposal.md](proposal.md). This document records the technical design and the facts it rests on.

## Two planes

- **Control plane** — `crates/api`, a [skyzen](https://crates.io/crates/skyzen) app on Cloudflare Workers (wasm32). REST API + auth, session store, budget accounting, approvals, skills/MCP registry, provisioning orchestration. Services: D1 (relational metadata), KV (caches, auth tokens), R2 (transcripts, repo snapshots, skill zips), Queues (provisioning jobs, GitHub webhooks), Durable Objects (live session rooms).
- **Execution plane** — `flycod` (`crates/daemon`), a native Rust daemon installed on every session VM. It supervises the harness process, enforces takeovers, serves the terminal, streams events to the control plane, and handles spot-eviction notices and budget pauses.

The Worker is wasm32: no processes, no listening sockets, no tokio. Anything long-running or process-shaped lives in the daemon.

## Crates

| Crate | Runs on | Purpose |
|---|---|---|
| `flyco-core` | wasm32 + native | Domain types, DTOs, wire protocol, budget engine. Pure logic, no I/O. |
| `flyco-api` | Cloudflare Workers | Control plane. |
| `flyco-provider` | wasm32 + native | `CloudProvider` trait + implementations (byo-ssh, Azure, AWS, GCP) over signed HTTP via zenwave. |
| `flyco-daemon` | Linux VMs (native) | `flycod`: harness drivers, terminal, local MCP server, enforcement. |

`frontend/` is the SolidJS PWA (Bun + Vite), embedded into the Worker via `EmbeddedStaticDir`.

## Session relay

One Durable Object per session, addressed by the session id (`SESSION_ROOMS.get_by_name(session)`), so a session and its room are one identity with no mapping table to go stale. The daemon holds one outbound hibernating WebSocket tagged `daemon`; browsers connect tagged `client`; the room forwards frames between tags and appends the conversation — the harness stream, plus every user message, whichever way it arrived — to its own SQLite so a reconnecting browser can catch up. That table is the only normalized, harness-neutral record of a session, so `GET /v1/sessions/{id}/turns` folds the turn history out of it (`flyco_api::turns`) rather than out of R2, which holds the *harness's own* store: opaque Agent SDK entries for Claude Code and nothing at all for Codex. The room also keeps the last working tree the daemon reported, which is what `repo-status` serves; no report is a 404, because an unexamined tree is not a clean one. D1 keeps durable metadata and the append-only spend ledger.

The wire protocol is `flyco_core::wire`, versioned by `WIRE_PROTOCOL_VERSION`; a mismatch closes the connection (fast fail). Three enums, one per direction: `DaemonToControl`, `ControlToDaemon`, and `ClientEvent` (what browsers receive). **Every variant is a struct variant, including single-payload ones** — an internally-tagged newtype variant wrapping another internally-tagged enum emits `type` twice, which `serde_json` writes happily and then refuses to read back.

A Durable Object cannot reach D1 or the Worker's KV, so every check that needs them happens in the Worker before it addresses a room: credential validity, session ownership, and the durable half of any state change. What arrives at the room is already authenticated and says so in internal headers (`x-flyco-internal`, `x-flyco-session`, `x-flyco-role`) that only a same-Worker call can set. A socket's role and session id live in its accept-time tags, so a `Hello` can be validated after the room has hibernated without an I/O round trip per frame; the handshake itself is remembered in the connection's attachment.

**How each side authenticates.** The daemon presents `Authorization: Bearer fd_…`, a per-session token minted by `POST /v1/sessions/{id}/daemon-token` and stored as a SHA-256 in `sessions.daemon_token_hash`. It resolves to a *session*, never to a user, and is checked against the session id in the path — so the scope check is the lookup itself. A browser cannot set headers on a WebSocket handshake, so it exchanges its session token for a single-use 60-second relay ticket (`POST /v1/sessions/{id}/relay-ticket`) and opens `wss://…/relay/client?ticket=frt_…`; the ticket is consumed by being *presented*, not by being accepted.

**Transcripts never ride the relay.** Cloudflare caps a WebSocket frame at 1 MiB and a transcript is unbounded, so `flycod` writes batches over ordinary authenticated REST to R2 at `transcripts/{session}/{stream}/{seq:08}.jsonl`. The zero-padded sequence makes lexicographic order equal numeric order, so a read is a prefix list with no index to keep consistent, and batches are immutable — rewriting one would silently reorder the transcript, so it is a 409.

## Harness drivers

- **Claude Code**: a TypeScript sidecar under Bun using `@anthropic-ai/claude-agent-sdk`, spoken to by flycod over stdio. The SDK is the only supported route to `SessionStore` (cross-host session resume — flyco's History feature, backed by R2) and to `canUseTool` approvals. Feature detection via `system/init.capabilities`, never version strings; SIGINT (never SIGTERM) ends a turn.
- **Codex**: pure Rust JSON-RPC 2.0 client to `codex app-server` (`thread/start`, `turn/start`, `turn/interrupt`, streaming `item/*` notifications).

Both are normalized to `flyco_core::harness::HarnessEvent`. Feature parity is tracked in [feature-matrix.md](feature-matrix.md) — "ALL features" is a matrix with per-release gaps, not an untracked promise.

### The Claude Code sidecar

`crates/daemon/sidecar/` is a Bun TypeScript project embedded in the `flycod` binary (`rust-embed`, excluding the in-tree `node_modules/`) and materialized to a working directory at startup, where `bun install --frozen-lockfile` runs once. flycod speaks a line protocol to it over stdio — `crates/daemon/src/harness/claude/protocol.rs` and `sidecar/protocol.ts` (zod), pinned against each other by `crates/daemon/fixtures/protocol/`, one canonical JSON document per message variant that both test suites decode, re-encode, and compare byte for byte.

The sidecar owns the SDK session and nothing else: it forwards every SDK message out verbatim, parks `canUseTool` and `SessionStore` calls on flycod, and ends turns through `interrupt()`. Every interpretation happens in Rust.

A session announces itself (`started`) as soon as its SDK query is constructed — the CLI is spawned and handshaking by then and the sidecar picks the session UUID itself, so flyco has a warm, identified session before the user types. **Capabilities are turn-derived and therefore arrive late**: the Agent SDK names them only on the `system/init` stream frame, which the CLI emits at the start of a turn (not at boot, not on the `initialize` control response, and not on `reinitialize()` — verified against CLI 2.1.250). They travel as their own `capabilities` event, newest set wins, and every consumer must tolerate "unknown yet" rather than reading pre-first-turn silence as "this build supports nothing". Normalization is documented-drop: message types flyco does not model are logged at `debug` and dropped, because the SDK adds them on Anthropic's release schedule. Assistant text comes only from partial-message deltas (`includePartialMessages`), and the turn id is flyco's own — the Agent SDK has no turn identity, only a session, messages, and a terminal `result`.

Auth has two shapes, and they are one decision: `inherit` (no `CLAUDE_CONFIG_DIR`, no credential injection — the developer-machine mode, which reads the host's own `~/.claude`), or an injected credential that always carries its own isolated config tree and project key.

`flycod run` picks its driver from the configuration and logs which it chose. With a `[control_plane]` section it opens the relay and keeps its transcript in R2; without one it drives the session from a line-oriented stdin REPL, printing `SessionOutput` values as JSON lines, and keeps the transcript in `transcript_dir`. The REPL stays as the dev tool for reproducing a harness bug without provisioning a VM.

### The relay client

Two tasks and one bounded queue. The collector owns the harness's output stream and turns each `SessionOutput` into a wire frame; the connection owns the socket and the harness's control handle, draining the queue outward and dispatching commands inward. They are separate because the socket is not always there, and the queue is what absorbs a reconnect — bounded at 1024 frames, because a queue that grows without limit trades a visible outage for an OOM kill. **Overflow is fatal**, never a silent drop: losing part of a session's transcript is worse than stopping. A frame that leaves the queue but fails to write is handed back and retried on the next connection, making delivery at-least-once; exactly-once needs an ack the protocol does not carry yet.

Reconnection is capped exponential backoff with full jitter (1s→60s) and re-sends `Hello`, because a room that hibernated has forgotten the handshake. Nothing is pumped before `Welcome`.

A budget threshold below 100% is *told* to the agent, as a `[flyco budget notice] …` message in the conversation — the agent decides how to spend its budget, so a threshold is information rather than a limit. `Pause` is the exception: it interrupts the turn immediately and the daemon stops accepting work.

An approval is recorded over REST **before** its frame is announced, so the id a browser sees is one the API can settle, and a decision that arrives while the socket is down still finds a pending row.

### Takeover enforcement (strongest first)

Claude Code: `/etc/claude-code/managed-settings.json` + `managed-mcp.json` (root-owned, outrank everything) with `allowManagedPermissionRulesOnly`, `allowManagedMcpServersOnly`, `disableSideloadFlags`, `autoMemoryEnabled: false`, absolute-path deny rules (bind even in bypass mode); `PreToolUse` hook exit-2 backstop; managed policy CLAUDE.md carries the shared AGENTS.md. Codex: MCP allowlist (id + identity) + root-owned `config.toml` + hooks. Both: config dirs unwritable by the agent user. Sanctioned mutations go through flycod's local stdio MCP server: `memory_*` (tree), `skill_upload`, `agentsmd_change_request` (→ user approval), `machine_resize`, `budget_status`.

## Budget engine

`flyco_core::budget`, pure and exhaustively tested. Thresholds: notice 50%, warn 80%, final warn 90%, pause 100%; budgets under $5 signal only at 90%+pause. Budgets cover compute + storage, never LLM tokens. Money is integer microdollars (`Usd`) — floats never enter the domain. A scheduled Worker cron prices machine time from provider catalogs and posts spend to each live session's DO.

## Providers

`flyco_provider::CloudProvider`: `catalog` (typed pricing, including minimum-billing flags like EC2 Mac's 24-hour Apple-license minimum), `provision`, `resize` (disk-preserving), `deallocate`, `start`, `destroy`. Every method takes `&mut self`, because a driver caches state it must be able to replace — an Azure access token, refreshed at 80% of its lifetime and on any 401 — and modelling that with `&self` would mean interior mutability, which on a `Send` future means a lock.

No driver touches `zenwave` directly. They build a `flyco_provider::http::HttpRequest` and hand it to an `HttpTransport`, which is `ZenwaveTransport` in production (Fetch on the Worker, hyper natively) and a table of recorded exchanges under test. That indirection is what makes the wire assertable: `crates/provider/fixtures/` holds the recorded provider responses, and the tests pin the exact URL, `api-version`, headers and JSON body each operation produces. Spot by default; eviction notices are watched by flycod on the VM metadata endpoint (Azure Scheduled Events `Preempt`, EC2 IMDSv2).

**Every provisioned machine boots the same `flycod` configuration**, rendered once by `flyco_provider::flycod` as a serde document — not a template, because the daemon's loader is `deny_unknown_fields` and escaping and layout have to be the serializer's problem. `crates/daemon/tests/provisioned_config.rs` renders it and parses it back with the daemon's own loader, so a field renamed on either side fails a test rather than producing a machine that boots and never phones home.

### byo-ssh, and the plan/execute split

byo-ssh is the provider that needs no cloud account, so it exists first: everything downstream of provisioning is exercisable against a laptop. A "machine" is a **Podman container** on the registered host — created from the flyco image, handed the session's `flycod` config through an env-file on the remote shell's stdin (never a command-line argument, which a `ps` would show), stopped to deallocate, removed to destroy. `resize` is `ProviderError::Unsupported`, never a silent no-op: the host has the hardware it has, and a caller that believed a resize happened would bill and schedule against a machine that did not change. `catalog()` reports one entry — the host itself — with `MachinePricing::UserOwned` and no capacity, because flyco does not know either number and inventing a `$0.00` would tell a budget the session can run forever.

SSH is a TCP transport and the Worker has no sockets, so the driver is in two halves, and the split is enforced by the crate graph rather than by convention:

- `byo_ssh::ByoSsh` compiles on wasm32, performs no I/O, and *plans*: it turns a `MachineOperation` into a serializable `ContainerJob` that the control plane enqueues.
- `byo_ssh::SshExecutor` exists only behind the native `ssh` feature (russh) and is the only thing that implements `CloudProvider` for byo-ssh. A Worker build contains no SSH client because the feature that provides one is not enabled for it.

The feature is off by default and CI enables it explicitly (`--features flyco-provider/ssh`) so the executor is still linted and tested. Host keys are pinned: `ProviderCredentials::ByoSsh` carries a required `host_fingerprint` and there is no trust-on-first-use path, because linking the account is the moment flyco starts handing that address live session credentials.

## Version pins and workarounds (skyzen 0.1.2)

Designed against the published `v0.1.2` tag (the skyzen repo's dev branch is far ahead — its docs describe unreleased features). Consequences, dropped when 0.2.0 releases:

- SPA embedded via `EmbeddedStaticDir` (no CF Assets support at 0.1.2).
- `openapi.json` exported by a native debug run — `cargo run -p flyco-api --bin openapi > openapi.json` (`linkme` collection is debug+native only). The result is checked in at the repo root and CI fails on a diff; the TS client is generated from that artifact. `#[skyzen::openapi]` cannot be applied to a generic handler (it emits module-level items naming every argument type), so `auth/github/callback` exports its path without parameter schemas.
- **Response bodies are declared, not derived** (upstream [skyzen#18](https://github.com/zen-rs/skyzen/issues/18)). `openapi::maybe_schema_of::<T>()` is a generic function with no `T: ToSchema` bound, so its specialization probe never fires and `Json<T>::openapi()` reports a schema-less response; the only path to real content is the `#[skyzen::openapi]` branch that syntactically recognises a bare `Json<T>` return type, which nothing wrapped in `problem::Outcome` matches. `flyco_api::responses` therefore carries one table of operation id → success status + payload, writes it into the generated document, and registers every returned DTO into `components.schemas`. `crates/api/src/tests/responses.rs` parses the crate's own sources, derives each annotated handler's payload from its return type, and fails if the table disagrees — so a changed return type cannot silently ship an untyped client. Status codes are derivable too, which is why `respond::Created<T>` is a type rather than a status field: `Created<Json<T>>` in a signature *is* the `201`. The six routed operations with no `#[skyzen::openapi]` annotation (the two relay upgrades, the three daemon-scoped routes, the generic OAuth callback) are named in `responses::UNDECLARED` with the reason each has none.
- D1 migrations via `wrangler d1 migrations`; KV/D1 resource IDs provisioned with wrangler and pasted into `crates/api/Skyzen.toml` (the manifest must sit next to the crate's `Cargo.toml` — that is where `#[skyzen::main]` looks for it).
- `[[database]]` is **not** declared: skyzen 0.1.2's database codegen expands to `if <bool> { WithMiddleware<E, Db> } else { E }`, whose arms have different types, so any declared database fails to compile. `flyco_api::database` opens D1 (Worker) or SQLite (native) by hand and the router injects it. The KV namespace *is* a `[[service]]`, which works.
- `[native.service.auth_kv] backend = "memory"` resolves to `skyzen_test::mock::InMemoryKv`, so `skyzen-test` is a dependency of flyco-api's native target — the Worker build never sees it.
- `skyzen-cloudflare` pulls the official `worker` crate, which pulls `tokio` into the wasm dependency graph. Nothing in flyco uses it.
- `Kv` has no TTL parameter at 0.1.2, so `flyco_api::expiring` stores an explicit deadline alongside every value and treats an expired read as a miss. Cloudflare KV's native TTL is not relied on.
- `WebSocketUpgrade::on_upgrade` on wasm (no `.ws()` shorthand); DO relay uses `HibernationWebSocketUpgrade` + tags.
- **A `101` response drops its headers.** `convert_response` (`src/runtime/wasm.rs`) and `create_websocket_response` (`src/websocket/ffi.rs`) both build a fresh `ResponseInit` carrying only `status` and `webSocket`, so `Sec-WebSocket-Protocol` cannot be echoed. RFC 6455 §4.1 makes a client that offered a subprotocol fail the connection when the server echoes none, so the usual browser trick — smuggling the credential through `Sec-WebSocket-Protocol` — is unbuildable here. Hence the relay ticket above; restore the subprotocol handshake when skyzen can set headers on a `101`.
- **The native Durable Object simulator serves `fetch` but never `websocket`.** `NativeDurableConnections` answers every `all()`/`by_tag()` with an empty set and nothing calls `DurableObject::websocket`, and `HibernationWebSocketUpgrade` implements `Responder` on wasm32 only. The relay routes therefore answer `501` on native (`problems/relay-unavailable`) rather than accepting a socket that would never receive a frame; the room's HTTP routes work natively and are what `skyzen dev` and the tests drive.
- `skyzen_test::mock::InMemoryDurableDb` records the SQL it is handed and returns no rows, so it cannot exercise a DO's tables. `crates/api/src/tests/room.rs` carries a ~40-line `DurableDbBackend` over in-memory SQLite instead, and drives the real `SessionRoom::websocket` through the public `WebSocketConnection::new` / `DurableConnections::new` / `DurableContext::new` constructors.
- JavaScript handles (`worker::Request`, `js_sys::Promise`, `JsFuture`) are `!Send`, but skyzen's `Handler` requires a handler's future to be `Send`. Worker→room calls wrap their whole body in `worker::send::SendFuture`, which is sound because Workers are single-threaded and is what `skyzen-cloudflare` does internally.
- `#[skyzen::durable_object]` on a struct `T` exports the JS class as `TObject`, so `Skyzen.toml` binds `class_name = "SessionRoomObject"` for `struct SessionRoom`.
- No built-in sessions/OAuth/JWT on wasm: GitHub OAuth code flow via zenwave, opaque tokens in KV behind a custom `Authenticator` (there is no `AuthUser<T>` extractor at 0.1.2 — the user arrives as `State<CurrentUser>`).
- `flyco_api::middleware::RequireAuth` replaces skyzen's `AuthMiddleware`: the stock one propagates the error to the runtime, which renders `{"error": …}` and cannot set `WWW-Authenticate`. Flyco's own errors likewise never propagate — handlers return `problem::Outcome`, which renders the RFC 9457 document itself — and an `ErrorHandlingMiddleware` at the router root converts framework errors to the same media type.

## Auth

- **`Authorization: Bearer` only — no cookies.** Two token kinds share one credential channel: `fs_` browser session tokens (opaque, KV, 30 days) and `fk_` API keys (D1, SHA-256 hashed). The prefix tells the authenticator which store owns the token, so a lookup never probes both. No cookie means no ambient authority and nothing for a cross-site request to ride on.
- The OAuth callback hands the token to the SPA through the redirect's **URL fragment** (`/auth/complete#token=fs_…`, resolved against the configured redirect URI's origin) — a fragment never reaches the server, so it stays out of access logs and `Referer` headers.
- Users: GitHub OAuth only.
- Standards: 401s carry `WWW-Authenticate: Bearer` (RFC 6750), with `error="invalid_token"` when a credential was presented and rejected. Every error response is an RFC 9457 problem document (`application/problem+json`); flyco's own failures name a type under `https://flyco.dev/problems/`, framework-level failures use `about:blank`. Server-side detail is logged, never returned.
- Harness accounts: flyco redirects to the provider's own auth page (the flow the official CLIs wrap); tokens stored encrypted, provisioned per-session to VMs. Anthropic's third-party-login policy approval is a hosted-launch prerequisite.
- Usage panels are reactive: no remaining-quota API exists for either harness; flyco observes rate-limit events and OTLP cost telemetry.
