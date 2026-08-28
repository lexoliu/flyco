# Flyco architecture

The product-level spec is [proposal.md](proposal.md). This document records the technical design and the facts it rests on.

## Two planes

- **Control plane** — `crates/flyco-api`, a [skyzen](https://crates.io/crates/skyzen) app on Cloudflare Workers (wasm32). REST API + auth, session store, budget accounting, approvals, skills/MCP registry, provisioning orchestration. Services: D1 (relational metadata), KV (caches, auth tokens), R2 (transcripts, repo snapshots, skill zips), Queues (provisioning jobs, GitHub webhooks), Durable Objects (live session rooms).
- **Execution plane** — `flycod` (`crates/flyco-daemon`), a native Rust daemon installed on every session VM. It supervises the harness process, enforces takeovers, serves the terminal, streams events to the control plane, and handles spot-eviction notices and budget pauses.

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

One Durable Object per session (`session:{id}`). The daemon holds one outbound hibernating WebSocket tagged `daemon`; browsers connect tagged `client`; the DO forwards frames between tags, batches transcript writes to R2, and owns the live budget counter (single writer — D1 has no transactions). D1 keeps durable metadata and the append-only spend ledger.

The wire protocol is `flyco_core::wire` (`DaemonToControl` / `ControlToDaemon`), versioned by `WIRE_PROTOCOL_VERSION`; a version mismatch closes the connection (fast fail).

## Harness drivers

- **Claude Code**: a TypeScript sidecar under Bun using `@anthropic-ai/claude-agent-sdk`, spoken to by flycod over stdio. The SDK is the only supported route to `SessionStore` (cross-host session resume — flyco's History feature, backed by R2) and to `canUseTool` approvals. Feature detection via `system/init.capabilities`, never version strings; SIGINT (never SIGTERM) ends a turn.
- **Codex**: pure Rust JSON-RPC 2.0 client to `codex app-server` (`thread/start`, `turn/start`, `turn/interrupt`, streaming `item/*` notifications).

Both are normalized to `flyco_core::harness::HarnessEvent`. Feature parity is tracked in [feature-matrix.md](feature-matrix.md) — "ALL features" is a matrix with per-release gaps, not an untracked promise.

### Takeover enforcement (strongest first)

Claude Code: `/etc/claude-code/managed-settings.json` + `managed-mcp.json` (root-owned, outrank everything) with `allowManagedPermissionRulesOnly`, `allowManagedMcpServersOnly`, `disableSideloadFlags`, `autoMemoryEnabled: false`, absolute-path deny rules (bind even in bypass mode); `PreToolUse` hook exit-2 backstop; managed policy CLAUDE.md carries the shared AGENTS.md. Codex: MCP allowlist (id + identity) + root-owned `config.toml` + hooks. Both: config dirs unwritable by the agent user. Sanctioned mutations go through flycod's local stdio MCP server: `memory_*` (tree), `skill_upload`, `agentsmd_change_request` (→ user approval), `machine_resize`, `budget_status`.

## Budget engine

`flyco_core::budget`, pure and exhaustively tested. Thresholds: notice 50%, warn 80%, final warn 90%, pause 100%; budgets under $5 signal only at 90%+pause. Budgets cover compute + storage, never LLM tokens. Money is integer microdollars (`Usd`) — floats never enter the domain. A scheduled Worker cron prices machine time from provider catalogs and posts spend to each live session's DO.

## Providers

`flyco_provider::CloudProvider`: `catalog` (typed pricing, including minimum-billing flags like EC2 Mac's 24-hour Apple-license minimum), `provision`, `resize` (disk-preserving), `deallocate`, `destroy`. All implementations are plain signed HTTP (zenwave) so they run in the Worker; `aws-sigv4` is verified to compile on wasm32. Spot by default; eviction notices are watched by flycod on the VM metadata endpoint (Azure Scheduled Events `Preempt`, EC2 IMDSv2).

## Version pins and workarounds (skyzen 0.1.2)

Designed against the published `v0.1.2` tag (the skyzen repo's dev branch is far ahead — its docs describe unreleased features). Consequences, dropped when 0.2.0 releases:

- SPA embedded via `EmbeddedStaticDir` (no CF Assets support at 0.1.2).
- `openapi.json` exported by a native debug run (`linkme` collection is debug+native only); the TS client is generated from that artifact.
- D1 migrations via `wrangler d1 migrations`; KV/D1 resource IDs provisioned with wrangler and pasted into `Skyzen.toml`.
- `WebSocketUpgrade::on_upgrade` on wasm (no `.ws()` shorthand); DO relay uses `HibernationWebSocketUpgrade` + tags.
- No built-in sessions/OAuth/JWT on wasm: GitHub OAuth code flow via zenwave, opaque tokens in KV behind a custom `Authenticator`.

## Auth

- Users: GitHub OAuth only; opaque session tokens in KV; API keys (hashed) for the REST API.
- Harness accounts: flyco redirects to the provider's own auth page (the flow the official CLIs wrap); tokens stored encrypted, provisioned per-session to VMs. Anthropic's third-party-login policy approval is a hosted-launch prerequisite.
- Usage panels are reactive: no remaining-quota API exists for either harness; flyco observes rate-limit events and OTLP cost telemetry.
