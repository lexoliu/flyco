/**
 * A fixture control plane for looking at the UI.
 *
 * `bun run preview:api` serves it on :8788 and `bun run dev:preview` points
 * the Vite dev server at it, so every page renders with a realistic account
 * — sessions in every state, a transcript with tools and an approval, linked
 * agents with usage, linked compute — without one request reaching the
 * deployed control plane. Sign-in is `/auth/complete#token=preview` on the
 * dev server; any bearer is accepted.
 *
 * Every fixture is typed against the generated OpenAPI schema, so a field
 * the API renames fails `bun run typecheck` here rather than rendering a
 * blank card. Routes this does not know answer 404 and are printed, which
 * is how a page's new request is noticed.
 */
import type { components } from "../src/api/schema.d.ts";

type S = components["schemas"];

const PORT = Number(process.env["PREVIEW_API_PORT"] ?? 8788);
/** One dollar, in the API's micro-dollar unit. */
const USD = 1_000_000;
const NOW = Math.floor(Date.now() / 1000);
const MINUTE = 60;
const HOUR = 3600;
const DAY = 86_400;

// ── Identity ──────────────────────────────────────────────────────────

const ME: S["CurrentUser"] = { id: "8c1f1a2e-6c6f-4a8f-9a1a-1c2d3e4f5a6b", login: "lexoliu", session_cap: 5 };

const AZURE_ACCOUNT = "3b7a9c1d-2e4f-4a6b-8c0d-1e2f3a4b5c6d";
const CODESPACES_ACCOUNT = "9f8e7d6c-5b4a-4392-8170-6f5e4d3c2b1a";
const HOST_ACCOUNT = "0a1b2c3d-4e5f-4061-8283-94a5b6c7d8e9";
const HOST_ID = "5e6f7a8b-9c0d-4e1f-a2b3-c4d5e6f7a8b9";

const PROVIDERS: S["ProviderAccountView"][] = [
  { id: AZURE_ACCOUNT, kind: "azure", label: "Azure for Students", linked_at_unix: NOW - 12 * DAY, host_id: null },
  { id: CODESPACES_ACCOUNT, kind: "codespaces", label: "GitHub Codespaces", linked_at_unix: NOW - 30 * DAY, host_id: null },
  { id: HOST_ACCOUNT, kind: "host", label: "mbp-m3", linked_at_unix: NOW - 3 * DAY, host_id: HOST_ID },
];

// The list Claude Code itself serves, verbatim: five rows, and the
// default named for the model it resolves to rather than for itself. A
// three-row fixture fitted under the composer whatever the room, which
// is exactly the case the picker's placement does not have to get right.
const CLAUDE_MODELS: S["ModelOption"][] = [
  { id: "default", label: "Default (recommended)", description: "Sonnet 5 · Efficient for routine tasks", is_default: true, efforts: ["low", "medium", "high"], default_effort: "medium" },
  { id: "sonnet", label: "Sonnet", description: "Sonnet 5 · Efficient for routine tasks", is_default: false, efforts: ["low", "medium", "high"], default_effort: "medium" },
  { id: "fable", label: "Fable", description: "Fable 5 · Most capable for your hardest and longest-running tasks", is_default: false, efforts: ["low", "medium", "high", "xhigh"], default_effort: "high" },
  { id: "opus", label: "Opus", description: "Opus 5 · Best for everyday, complex tasks", is_default: false, efforts: ["low", "medium", "high"], default_effort: "medium" },
  { id: "haiku", label: "Haiku", description: "Haiku 4.5 · Fastest for quick answers", is_default: false, efforts: [], default_effort: null },
];

const HARNESS_ACCOUNTS: S["HarnessAccountView"][] = [
  {
    id: "c0ffee00-1111-4222-8333-444455556666",
    harness: "claude_code",
    label: "me@lexo.cool",
    linked_at_unix: NOW - 13 * DAY,
    expires_at_unix: NOW + 5 * DAY,
    models: CLAUDE_MODELS,
    usage: {
      state: "windows",
      windows: [
        { label: "5-hour", used_percent: 43, resets_at_unix: NOW + 2 * HOUR + 10 * MINUTE, window_minutes: 300 },
        { label: "Weekly", used_percent: 78, resets_at_unix: NOW + 3 * DAY, window_minutes: 10_080 },
        { label: "Weekly (Opus)", used_percent: 100, resets_at_unix: NOW + 3 * DAY, window_minutes: 10_080 },
      ],
    },
  },
  {
    id: "de71b000-1111-4222-8333-444455556666",
    harness: "devin",
    label: "Lexo Liu",
    linked_at_unix: NOW - DAY,
    expires_at_unix: null,
    models: [],
    usage: { state: "unmetered" },
  },
];

// ── Machines ──────────────────────────────────────────────────────────

const AZURE_B2S: S["MachineCatalogEntry"] = {
  provider: "azure",
  machine_type: "Standard_B2s",
  region: "eastus",
  os: "linux",
  runtime: "vm",
  pricing: { kind: "metered", on_demand_hourly: 0.0416 * USD, spot_hourly: 0.0083 * USD, storage: { kind: "per_gib_hourly", rate: 0.00011 * USD }, minimum: null },
  capacity: { vcpus: 2, memory_mib: 4096 },
  location: { latitude: 37.37, longitude: -79.82 },
  account: AZURE_ACCOUNT,
  free_grant: null,
  lineage: null,
};

const AZURE_D4: S["MachineCatalogEntry"] = {
  ...AZURE_B2S,
  machine_type: "Standard_D4s_v5",
  pricing: { kind: "metered", on_demand_hourly: 0.192 * USD, spot_hourly: 0.031 * USD, storage: { kind: "per_gib_hourly", rate: 0.00011 * USD }, minimum: null },
  capacity: { vcpus: 4, memory_mib: 16_384 },
};

const CODESPACE_4: S["MachineCatalogEntry"] = {
  provider: "codespaces",
  machine_type: "standardLinux32gb",
  region: "WestUs2",
  os: "linux",
  runtime: "container",
  pricing: { kind: "metered", on_demand_hourly: 0.36 * USD, spot_hourly: null, storage: { kind: "per_gib_hourly", rate: 0.0001 * USD }, minimum: null },
  capacity: { vcpus: 4, memory_mib: 16_384 },
  location: { latitude: 47.6, longitude: -122.3 },
  account: CODESPACES_ACCOUNT,
  free_grant: { vcpu_seconds_per_month: 120 * HOUR, gib_seconds_per_month: 15 * 30 * DAY },
  lineage: null,
};

const OWN_HOST: S["MachineCatalogEntry"] = {
  provider: "host",
  machine_type: "mbp-m3",
  region: "home",
  os: "mac_os",
  runtime: "container",
  pricing: { kind: "user_owned" },
  capacity: { vcpus: 12, memory_mib: 36_864 },
  location: null,
  account: HOST_ACCOUNT,
  free_grant: null,
  lineage: null,
};

const CATALOG: S["MachineCatalog"] = { entries: [AZURE_B2S, AZURE_D4, CODESPACE_4, OWN_HOST], pending_accounts: [] };

const DEFAULT_MACHINE: S["MachineDefault"] = {
  choice: { provider_account: AZURE_ACCOUNT, machine_type: "Standard_B2s", region: "eastus", runtime: "vm", spot: true, disk_gib: 64 },
  entry: AZURE_B2S,
  pending_accounts: [],
};

/**
 * What `GET /v1/machines/default?account=` answers for one account.
 *
 * Per account, because that is what the control plane does: every compute
 * card asks for its own account's machine, and one shared answer put an
 * Azure VM SKU and an Azure region under GitHub Codespaces and hid the
 * free grant that only a codespace has.
 */
function defaultMachine(account: string | null): S["MachineDefault"] {
  const entry = CATALOG.entries.find((candidate) => candidate.account === account);
  if (account === null || entry === undefined) {
    return DEFAULT_MACHINE;
  }
  return {
    choice: {
      provider_account: account,
      machine_type: entry.machine_type,
      region: entry.region,
      runtime: entry.runtime,
      spot: entry.pricing.kind === "metered" && entry.pricing.spot_hourly !== null,
      disk_gib: 64,
    },
    entry,
    pending_accounts: [],
  };
}

// ── Repositories ──────────────────────────────────────────────────────

const REPOS: S["RepoSummary"][] = [
  { slug: "lexoliu/flyco", default_branch: "dev", description: "Agentic coding on the web, with flexible cloud computing.", private: false, pushed_at_unix: NOW - 40 * MINUTE },
  { slug: "lexoliu/helios", default_branch: "dev", description: "A kernel for machines that boot in a blink.", private: true, pushed_at_unix: NOW - 5 * HOUR },
  { slug: "water-rs/waterui", default_branch: "main", description: "Declarative UI in Rust, one tree for every backend.", private: false, pushed_at_unix: NOW - 2 * DAY },
  { slug: "zen-rs/skyzen", default_branch: "main", description: "Rust web framework for Cloudflare Workers.", private: false, pushed_at_unix: NOW - 9 * DAY },
];

// ── Sessions ──────────────────────────────────────────────────────────

const SESSION_IDS = {
  budgets: "1a2b3c4d-0001-4000-8000-000000000001",
  relay: "1a2b3c4d-0002-4000-8000-000000000002",
  compositor: "1a2b3c4d-0003-4000-8000-000000000003",
  serial: "1a2b3c4d-0004-4000-8000-000000000004",
  gtk: "1a2b3c4d-0005-4000-8000-000000000005",
  notes: "1a2b3c4d-0006-4000-8000-000000000006",
  bench: "1a2b3c4d-0007-4000-8000-000000000007",
  hi1: "1a2b3c4d-0008-4000-8000-000000000008",
  hi2: "1a2b3c4d-0009-4000-8000-000000000009",
  napping: "1a2b3c4d-0010-4000-8000-000000000010",
} as const;

function session(
  id: string,
  title: string,
  repo: { slug: string; branch: string },
  state: S["SessionState"],
  activity: S["SessionActivity"],
  ago: number,
  extra: Partial<S["SessionDetail"]> = {},
): S["SessionDetail"] {
  return {
    id,
    title,
    repos: [{ slug: repo.slug, branch: repo.branch, dir: repo.slug.split("/")[1] ?? repo.slug, added_by: "user" }],
    harness: "claude_code",
    model: { model: "default", effort: "high" },
    permission_mode: "auto",
    state,
    activity,
    computer_use: false,
    machine_origin: "auto",
    created_at_unix: NOW - ago - HOUR,
    last_active_unix: NOW - ago,
    interrupted_reason: null,
    paused_reason: null,
    budget: { limit: 10 * USD, spent: 1.2 * USD, remaining: 8.8 * USD, stage: "ok" },
    failure: null,
    ...extra,
  };
}

const SESSIONS: S["SessionDetail"][] = [
  session(SESSION_IDS.budgets, "Add per-principal request budgets to the control plane", { slug: "lexoliu/flyco", branch: "fix/worker-usage-caps" }, "active", "working", 2 * MINUTE, {
    budget: { limit: 10 * USD, spent: 1.84 * USD, remaining: 8.16 * USD, stage: "ok" },
  }),
  session(SESSION_IDS.relay, "Fix the flaky relay attach test", { slug: "lexoliu/flyco", branch: "dev" }, "active", "needs_input", 25 * MINUTE),
  session(SESSION_IDS.notes, "Draft the 0.4 release notes", { slug: "lexoliu/flyco", branch: "dev" }, "interrupted", "idle", 6 * HOUR, {
    interrupted_reason: "spot_reclaimed",
  }),
  session(SESSION_IDS.compositor, "Port the compositor wake path to the new timer", { slug: "lexoliu/helios", branch: "compositor-wake" }, "active", "idle", 3 * HOUR),
  session(SESSION_IDS.napping, "Trim the boot log's serial chatter", { slug: "lexoliu/helios", branch: "dev" }, "interrupted", "idle", 5 * HOUR, {
    interrupted_reason: "suspended",
  }),
  session(SESSION_IDS.serial, "Investigate serial console garbage on boot", { slug: "lexoliu/helios", branch: "dev" }, "paused", "idle", 8 * HOUR, {
    paused_reason: "budget",
    budget: { limit: 5 * USD, spent: 5 * USD, remaining: 0, stage: "exhausted" },
  }),
  session(SESSION_IDS.bench, "Benchmark the allocator on the bench lane", { slug: "lexoliu/helios", branch: "dev" }, "provisioning", "working", 30, {
    budget: { limit: 20 * USD, spent: 0, remaining: 20 * USD, stage: "ok" },
  }),
  session(SESSION_IDS.gtk, "Pin the GTK backend's text foreground colour", { slug: "water-rs/waterui", branch: "gtk-pin-text-foreground" }, "failed", "idle", 2 * DAY, {
    failure: "provider rejected the request: the subscription's lowPriorityCores quota (4) is spent; release a machine or pick a region with room.",
  }),
  session(SESSION_IDS.hi1, "Hi", { slug: "water-rs/waterui", branch: "main" }, "archived", "idle", 4 * DAY),
  session(SESSION_IDS.hi2, "Say hello and print the output of uname -a", { slug: "water-rs/waterui", branch: "main" }, "archived", "idle", 5 * DAY),
];

function summary(detail: S["SessionDetail"]): S["SessionSummary"] {
  const { budget: _budget, failure: _failure, ...rest } = detail;
  return rest;
}

// ── The transcript of the working session ─────────────────────────────

const T0 = NOW - 34 * MINUTE;

function stored(events: { at: number; event: unknown }[]): S["StoredEvent"][] {
  return events.map((entry, index) => ({ seq: index + 1, at_unix: entry.at, event: entry.event }));
}

const TURN_1 = "turn-0001";
const TURN_2 = "turn-0002";

const BUDGET_EVENTS = stored([
  { at: T0, event: { type: "provisioning_stage", stage: "reserving", at_unix: T0 } },
  { at: T0 + 18, event: { type: "provisioning_stage", stage: "booting", at_unix: T0 + 18 } },
  { at: T0 + 61, event: { type: "provisioning_stage", stage: "cloning", at_unix: T0 + 61 } },
  { at: T0 + 74, event: { type: "provisioning_stage", stage: "ready", at_unix: T0 + 74 } },
  { at: T0 + 75, event: { type: "machine_connection", connected: true } },
  { at: T0 + 75, event: { type: "started", harness_session_id: "careful-vibraphone" } },
  { at: T0 + 76, event: { type: "models", models: CLAUDE_MODELS } },
  {
    at: T0 + 80,
    event: {
      type: "user_message",
      origin: "user",
      text: "One client looping at line rate keeps spending the whole account's daily Worker quota (#342). Add a per-principal request budget to crates/api: a per-minute bound before auth and a daily ceiling per user, session daemon and host, refused with 429 + Retry-After. Keep D1 writes off the request path.",
    },
  },
  { at: T0 + 82, event: { type: "harness", event: { type: "turn_started", turn_id: TURN_1 } } },
  {
    at: T0 + 90,
    event: {
      type: "harness",
      event: { type: "assistant_delta", turn_id: TURN_1, text: "I'll start from how requests are authenticated today, then add the budget as one middleware outside the routes so a refused request costs nothing further.\n\n" },
    },
  },
  {
    at: T0 + 92,
    event: {
      type: "harness",
      event: { type: "tool_started", turn_id: TURN_1, call_id: "call-1", tool: "Read", input: { file_path: "crates/api/src/authenticator.rs" } },
    },
  },
  { at: T0 + 94, event: { type: "harness", event: { type: "tool_completed", turn_id: TURN_1, call_id: "call-1", ok: true } } },
  {
    at: T0 + 95,
    event: {
      type: "harness",
      event: { type: "tool_started", turn_id: TURN_1, call_id: "call-2", tool: "Grep", input: { pattern: "fn bearer_token", path: "crates/api/src", description: "Find where the bearer is read off a request" } },
    },
  },
  { at: T0 + 96, event: { type: "harness", event: { type: "tool_completed", turn_id: TURN_1, call_id: "call-2", ok: true } } },
  {
    at: T0 + 120,
    event: {
      type: "harness",
      event: {
        type: "assistant_delta",
        turn_id: TURN_1,
        text: "The credential is resolved inside each route's extractor, so the principal is only known after the route ran. That settles the shape:\n\n- **Per-minute bound** before any credential lookup, keyed by the SHA-256 of the bearer (or the connecting address), through a Cloudflare Rate Limiting binding per class.\n- **Daily ceiling** charged after the route, to the principal it resolved to, from a per-isolate ledger flushed to D1 in batches.\n\n```rust\npub struct Limits {\n    pub user: Limit,\n    pub daemon: Limit,\n    pub host: Limit,\n    pub public: Limit,\n}\n\nimpl Limits {\n    pub const PRODUCTION: Self = Self {\n        user: Limit { per_minute: 300, per_day: 20_000 },\n        daemon: Limit { per_minute: 240, per_day: 25_000 },\n        host: Limit { per_minute: 60, per_day: 5_000 },\n        public: Limit { per_minute: 60, per_day: 2_000 },\n    };\n}\n```\n\nA principal at its ceiling has its credentials put on the isolate's block list until UTC midnight, so the refusal costs no storage read.",
      },
    },
  },
  {
    at: T0 + 130,
    event: {
      type: "harness",
      event: { type: "tool_started", turn_id: TURN_1, call_id: "call-3", tool: "Write", input: { file_path: "crates/api/src/request_budget.rs", content: "//! Per-principal request budgets (issue #342).\n" } },
    },
  },
  { at: T0 + 131, event: { type: "harness", event: { type: "tool_completed", turn_id: TURN_1, call_id: "call-3", ok: true } } },
  {
    at: T0 + 132,
    event: {
      type: "harness",
      event: {
        type: "tool_started",
        turn_id: TURN_1,
        call_id: "call-4",
        tool: "Edit",
        input: { file_path: "crates/api/src/app.rs", old_string: "with_rooms(Route::new((routes(), frontend())))", new_string: "with_rooms(Route::new((routes(), frontend())))\n    .with(request_budget::RequestBudget::new(limiter, limits))" },
      },
    },
  },
  { at: T0 + 133, event: { type: "harness", event: { type: "tool_completed", turn_id: TURN_1, call_id: "call-4", ok: true } } },
  {
    at: T0 + 140,
    event: {
      type: "harness",
      event: { type: "tool_started", turn_id: TURN_1, call_id: "call-5", tool: "Bash", input: { command: "cargo nextest run -p flyco-api", description: "Run the API crate's tests" } },
    },
  },
  { at: T0 + 296, event: { type: "harness", event: { type: "tool_completed", turn_id: TURN_1, call_id: "call-5", ok: false } } },
  {
    at: T0 + 300,
    event: {
      type: "harness",
      event: { type: "assistant_delta", turn_id: TURN_1, text: "Two failures: the router tests build the app without the new `limits` argument, and the per-minute test trips the public daily ceiling first. Fixing both." },
    },
  },
  {
    at: T0 + 302,
    event: {
      type: "harness",
      event: { type: "tool_started", turn_id: TURN_1, call_id: "call-6", tool: "Edit", input: { file_path: "crates/api/src/tests/webhooks.rs", old_string: "router(config, github, turnstile, vendors, clouds, codespaces, db, queue)", new_string: "router(config, github, turnstile, vendors, clouds, codespaces, Limits::PRODUCTION, db, queue)" } },
    },
  },
  { at: T0 + 303, event: { type: "harness", event: { type: "tool_completed", turn_id: TURN_1, call_id: "call-6", ok: true } } },
  {
    at: T0 + 305,
    event: {
      type: "harness",
      event: { type: "tool_started", turn_id: TURN_1, call_id: "call-7", tool: "Bash", input: { command: "cargo nextest run -p flyco-api", description: "Run the API crate's tests again" } },
    },
  },
  { at: T0 + 470, event: { type: "harness", event: { type: "tool_completed", turn_id: TURN_1, call_id: "call-7", ok: true } } },
  {
    at: T0 + 480,
    event: {
      type: "harness",
      event: { type: "assistant_delta", turn_id: TURN_1, text: "628 tests pass. The middleware is in, with a migration for the `request_budgets` table and a test that pins the production limits to `Skyzen.toml`. Next I'd wire the daemon to honour `Retry-After`; say the word." },
    },
  },
  {
    at: T0 + 481,
    event: {
      type: "harness",
      event: { type: "turn_completed", turn_id: TURN_1, usage: { input_tokens: 184_220, output_tokens: 9_812, estimated_cost: 0.62 * USD, context: { size_tokens: 1_000_000, used_tokens: 212_400 } } },
    },
  },
  { at: T0 + 482, event: { type: "usage", usage: { input_tokens: 184_220, output_tokens: 9_812, estimated_cost: 0.62 * USD, context: { size_tokens: 1_000_000, used_tokens: 212_400 } } } },
  { at: T0 + 600, event: { type: "shell_command", run: "run-1", command: "git status --short" } },
  { at: T0 + 601, event: { type: "shell_output", run: "run-1", stream: "stdout", data: " M crates/api/src/app.rs\n M crates/api/src/error.rs\n?? crates/api/src/request_budget.rs\n?? migrations/0033_request_budgets.sql\n" } },
  { at: T0 + 601, event: { type: "shell_exited", run: "run-1", outcome: { kind: "exited", code: 0 }, truncated: false } },
  { at: T0 + 700, event: { type: "model_changed", model: { model: "fable", effort: "high" } } },
  {
    at: T0 + 900,
    event: { type: "user_message", origin: "user", text: "Yes. Also coalesce the daemon's frame batches over half a second so a busy terminal is bounded to two requests a second." },
  },
  { at: T0 + 902, event: { type: "harness", event: { type: "turn_started", turn_id: TURN_2 } } },
  {
    at: T0 + 910,
    event: {
      type: "harness",
      event: { type: "tool_started", turn_id: TURN_2, call_id: "call-8", tool: "Read", input: { file_path: "crates/daemon/src/control/wire.rs" } },
    },
  },
  { at: T0 + 912, event: { type: "harness", event: { type: "tool_completed", turn_id: TURN_2, call_id: "call-8", ok: true } } },
  {
    at: T0 + 930,
    event: {
      type: "approval_pending",
      id: "a0000000-0000-4000-8000-000000000001",
      payload: { kind: "tool_use", tool: "Bash", input: { command: "git push -u origin fix/worker-usage-caps", description: "Push the branch so a PR can be opened" } },
    },
  },
  {
    at: T0 + 931,
    event: {
      type: "harness",
      event: { type: "assistant_delta", turn_id: TURN_2, text: "The relay's backoff ladder already climbs on a stream that dies within `ATTACH_STABLE`; I'm adding the `Retry-After` floor to it and a `COALESCE` window at the head of the pump loop." },
    },
  },
]);

const RELAY_EVENTS = stored([
  { at: NOW - 40 * MINUTE, event: { type: "provisioning_stage", stage: "ready", at_unix: NOW - 40 * MINUTE } },
  { at: NOW - 39 * MINUTE, event: { type: "user_message", origin: "user", text: "The attach test in crates/daemon flakes on CI about one run in five. Find the race and fix it." } },
  { at: NOW - 38 * MINUTE, event: { type: "harness", event: { type: "turn_started", turn_id: "turn-r1" } } },
  { at: NOW - 30 * MINUTE, event: { type: "harness", event: { type: "assistant_delta", turn_id: "turn-r1", text: "The room accepts the attach before its stream is registered, so a daemon that posts a frame within the gap gets `relay-epoch-stale`. The fix registers the stream first; the test needs the fixture's daemon to wait for `StreamOpened`. I'd like to merge `dev` in before pushing." } } },
  { at: NOW - 26 * MINUTE, event: { type: "harness", event: { type: "turn_completed", turn_id: "turn-r1", usage: { input_tokens: 62_100, output_tokens: 3_020, estimated_cost: 0.21 * USD, context: { size_tokens: 1_000_000, used_tokens: 71_000 } } } } },
  {
    at: NOW - 25 * MINUTE,
    event: { type: "approval_pending", id: "a0000000-0000-4000-8000-000000000002", payload: { kind: "merge", repo: "lexoliu/flyco", from_branch: "dev", into_branch: "fix/relay-attach-race" } },
  },
]);

const EVENTS: Record<string, S["StoredEvent"][]> = {
  [SESSION_IDS.budgets]: BUDGET_EVENTS,
  [SESSION_IDS.relay]: RELAY_EVENTS,
  [SESSION_IDS.gtk]: stored([
    { at: NOW - 2 * DAY, event: { type: "provisioning_stage", stage: "reserving", at_unix: NOW - 2 * DAY } },
    { at: NOW - 2 * DAY + 40, event: { type: "session_state_changed", state: "failed" } },
  ]),
  [SESSION_IDS.bench]: stored([
    { at: NOW - 30, event: { type: "provisioning_stage", stage: "reserving", at_unix: NOW - 30 } },
    { at: NOW - 12, event: { type: "provisioning_stage", stage: "booting", at_unix: NOW - 12 } },
  ]),
};

const APPROVALS: S["ApprovalView"][] = [
  { id: "a0000000-0000-4000-8000-000000000001", session: SESSION_IDS.budgets, payload: { kind: "tool_use", tool: "Bash", input: { command: "git push -u origin fix/worker-usage-caps" } }, state: "pending", created_at_unix: T0 + 930 },
  { id: "a0000000-0000-4000-8000-000000000002", session: SESSION_IDS.relay, payload: { kind: "merge", repo: "lexoliu/flyco", from_branch: "dev", into_branch: "fix/relay-attach-race" }, state: "pending", created_at_unix: NOW - 25 * MINUTE },
];

function machineOf(id: string): S["MachineView"] {
  const spot = id !== SESSION_IDS.gtk;
  return {
    id: `0000${id.slice(4)}`,
    session: id,
    spec: { provider: "azure", machine_type: "Standard_B2s", region: "eastus", runtime: "vm", spot, disk_gib: 64 },
    state:
      id === SESSION_IDS.bench
        ? "provisioning"
        : id === SESSION_IDS.gtk
          ? "destroyed"
          : id === SESSION_IDS.napping
            ? "deallocated"
            : "running",
    spot,
    region: "eastus",
    hourly: 0.0083 * USD,
    storage_hourly: 0.007 * USD,
    created_at_unix: NOW - HOUR,
  };
}

// ── Settings ──────────────────────────────────────────────────────────

const API_KEYS: S["ApiKeySummary"][] = [
  { id: "k0000000-0000-4000-8000-000000000001", label: "flyco CLI on mbp-m3", created_at_unix: NOW - 20 * DAY, last_used_unix: NOW - 3 * MINUTE },
  { id: "k0000000-0000-4000-8000-000000000002", label: "Devin", created_at_unix: NOW - 2 * DAY, last_used_unix: NOW - 5 * HOUR },
];

const MCP_SERVERS: S["McpServerView"][] = [
  { id: "s0000000-0000-4000-8000-000000000001", name: "context7", enabled: true, updated_at_unix: NOW - 9 * DAY, config: { transport: "http", url: "https://mcp.context7.com/mcp", headers: [] } },
  { id: "s0000000-0000-4000-8000-000000000002", name: "acpsub", enabled: false, updated_at_unix: NOW - 2 * DAY, config: { transport: "stdio", command: "acpsub", args: ["serve"], env: [] } },
];

/** Registry entries as the control plane translates them, one of each shape. */
const CATALOG_MCP: S["CatalogMcpServer"][] = [
  {
    name: "com.devin/deepwiki",
    title: "DeepWiki",
    description: "Ask questions about any public GitHub repository and read its generated documentation.",
    version: "1.0.0",
    repository_url: "https://github.com/cognition-ai/deepwiki",
    website_url: "https://deepwiki.com",
    suggested_name: "deepwiki",
    installs: [{ kind: "remote", label: "Remote · mcp.deepwiki.com", inputs: [] }],
  },
  {
    name: "ai.smithery/Hint-Services-obsidian-github-mcp",
    title: null,
    description: "Connect AI assistants to your GitHub-hosted Obsidian vault to seamlessly access, search, and analyze your knowledge base.",
    version: "0.4.0",
    repository_url: "https://github.com/Hint-Services/obsidian-github-mcp",
    website_url: null,
    suggested_name: "Hint-Services-obsidian-github-mcp",
    installs: [
      {
        kind: "remote",
        label: "Remote · server.smithery.ai",
        inputs: [
          { key: "var:smithery_api_key", label: "smithery_api_key", description: "Bearer token for Smithery authentication", required: true, secret: true, default: null },
        ],
      },
    ],
  },
  {
    name: "io.github.bytedance/mcp-server-filesystem",
    title: "Filesystem",
    description: "Read, write and search files under the directories you allow.",
    version: "latest",
    repository_url: "https://github.com/bytedance/UI-TARS-desktop",
    website_url: null,
    suggested_name: "mcp-server-filesystem",
    installs: [
      {
        kind: "npm",
        label: "npx @agent-infra/mcp-server-filesystem",
        inputs: [
          { key: "arg:allowed-directories", label: "allowed-directories", description: "Comma-separated list of allowed directories for file operations", required: true, secret: false, default: null },
        ],
      },
    ],
  },
  {
    name: "io.github.0nork/0nMCP",
    title: "0nMCP",
    description: "One MCP server that fronts 700+ integrations, hosted or run locally.",
    version: "2.4.1",
    repository_url: "https://github.com/0nork/0nMCP",
    website_url: "https://0n.network",
    suggested_name: "0nMCP",
    installs: [
      { kind: "remote", label: "Remote · mcp.0n.network", inputs: [] },
      { kind: "npm", label: "npx 0nmcp", inputs: [] },
    ],
  },
  {
    name: "io.github.upstash/context7",
    title: "Context7",
    description: "Up-to-date documentation for any library, straight into the context window.",
    version: "1.0.14",
    repository_url: "https://github.com/upstash/context7",
    website_url: "https://context7.com",
    suggested_name: "context7",
    installs: [{ kind: "remote", label: "Remote · mcp.context7.com", inputs: [] }],
  },
];

const SKILLS: S["SkillView"][] = [
  { id: "sk000000-0000-4000-8000-000000000001", name: "flyco", scope: "claude", size_bytes: 18_432, uploaded_at_unix: NOW - 6 * DAY },
  { id: "sk000000-0000-4000-8000-000000000002", name: "fusion-delegation", scope: "claude", size_bytes: 9_120, uploaded_at_unix: NOW - DAY },
];

const MARKETPLACES: S["MarketplaceView"][] = [
  { id: null, repo: "anthropics/skills", git_ref: null, built_in: true, added_at_unix: null },
  { id: "m0000000-0000-4000-8000-000000000001", repo: "lexoliu/flyco-skills", git_ref: "main", built_in: false, added_at_unix: NOW - 2 * DAY },
  { id: "m0000000-0000-4000-8000-000000000002", repo: "acme/missing-manifest", git_ref: null, built_in: false, added_at_unix: NOW - DAY },
];

/** A marketplace flyco has not finished reading, and one it could not read. */
const PENDING_MARKETPLACES = new Set<string>();
const FAILED_MARKETPLACE: S["MarketplaceProblem"] = { marketplace: "acme/missing-manifest", detail: "no .claude-plugin/marketplace.json at the repository root" };

const CATALOG_SKILLS: S["CatalogSkill"][] = [
  { marketplace: "anthropics/skills", plugin: "document-skills", name: "xlsx", description: "Read and write Excel workbooks, including formulas and formatting." },
  { marketplace: "anthropics/skills", plugin: "document-skills", name: "pdf", description: "Fill, split and read PDF documents." },
  { marketplace: "anthropics/skills", plugin: "artifacts-builder", name: "artifacts-builder", description: "Build a single-page artifact with the house design system." },
  { marketplace: "lexoliu/flyco-skills", plugin: "flyco", name: "fusion-delegation", description: "Run a persistent worker agent across a sequence of related tasks." },
];

const MEMORY: S["MemoryNode"][] = [
  { id: "n0000000-0000-4000-8000-000000000001", parent: null, repo: null, title: "Toolchain", content: "bun over npm, uv over pip, ast-grep before grep.", updated_at_unix: NOW - 10 * DAY },
  { id: "n0000000-0000-4000-8000-000000000002", parent: "n0000000-0000-4000-8000-000000000001", repo: null, title: "Rust builds", content: "One builder per target dir; never two cargo commands in parallel.", updated_at_unix: NOW - 4 * DAY },
  { id: "n0000000-0000-4000-8000-000000000003", parent: null, repo: "lexoliu/flyco", title: "Definition of done", content: "Merged to dev via PR and deployed to dev.flyco.dev.", updated_at_unix: NOW - DAY },
];

const AGENTS_MD: S["AgentsDocument"] = {
  content: "# Flyco repository notes\n\n## Product invariant: browser-only users\n\nAssume the user has only a browser. Every user-facing flow must be completable from the PWA alone.\n",
  updated_at_unix: NOW - 3 * HOUR,
};

const LLM_USAGE: S["LlmUsageView"][] = [
  { account: HARNESS_ACCOUNTS[0]?.id ?? "", harness: "claude_code", label: "me@lexo.cool", observed_cost: 4.31 * USD, period_start_unix: NOW - DAY, rate_limited_at_unix: NOW - 11 * HOUR, resets_at_unix: NOW + 2 * HOUR },
];

const FEATURES: S["HarnessFeature"][] = [
  { feature: "usage_display", claude_code: "supported", codex: "supported", devin: "not_applicable" },
  { feature: "context_window_display", claude_code: "supported", codex: "planned", devin: "not_applicable" },
  { feature: "auto_mode", claude_code: "supported", codex: "harness_limitation", devin: "not_applicable" },
  { feature: "compact", claude_code: "supported", codex: "supported", devin: "not_applicable" },
  { feature: "skills", claude_code: "supported", codex: "supported", devin: "disabled" },
  { feature: "mcp", claude_code: "supported", codex: "supported", devin: "planned" },
  { feature: "computer_control", claude_code: "takeover", codex: "phase2", devin: "not_applicable" },
];

const HOSTS: S["HostView"][] = [
  {
    id: HOST_ID,
    label: "mbp-m3",
    state: "online",
    created_at_unix: NOW - 3 * DAY,
    last_seen_unix: NOW - 20,
    facts: { architecture: "arm64", disk_free_gib: 412, hostname: "mbp-m3.local", kernel: "Darwin 25.6.0", memory_mib: 36_864, podman_version: "5.4.1", vcpus: 12 },
  },
];

// ── Serving ───────────────────────────────────────────────────────────

function json(body: unknown, status = 200, headers: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json", ...headers } });
}

function problem(status: number, slug: string, detail: string): Response {
  return new Response(
    JSON.stringify({ type: `https://flyco.dev/problems/${slug}`, title: slug, status, detail }),
    { status, headers: { "content-type": "application/problem+json" } },
  );
}

/** The per-user stream: a ping every ten seconds, nothing else. */
function eventStream(): Response {
  let timer: ReturnType<typeof setInterval> | undefined;
  const stream = new ReadableStream<Uint8Array>({
    start(controller) {
      const encoder = new TextEncoder();
      controller.enqueue(encoder.encode(": ping\n\n"));
      timer = setInterval(() => controller.enqueue(encoder.encode(": ping\n\n")), 10_000);
    },
    cancel() {
      clearInterval(timer);
    },
  });
  return new Response(stream, { headers: { "content-type": "text/event-stream", "cache-control": "no-cache" } });
}

function route(method: string, path: string, url: URL, body: string): Response {
  const seg = path.split("/").filter((part) => part.length > 0);
  const sessionId = seg[0] === "v1" && seg[1] === "sessions" ? seg[2] : undefined;

  if (method === "GET") {
    if (path === "/v1/config") return json({ turnstile_sitekey: "1x00000000000000000000AA" });
    if (path === "/v1/me") return json(ME);
    if (path === "/v1/sessions") return json(SESSIONS.map(summary));
    if (path === "/v1/events") return eventStream();
    if (path === "/v1/harness-accounts") return json(HARNESS_ACCOUNTS);
    if (path === "/v1/harness-features") return json(FEATURES);
    if (path === "/v1/providers") return json(PROVIDERS);
    if (path === "/v1/hosts") return json(HOSTS);
    // The wizard polls this while it waits for a machine that never comes.
    if (seg[1] === "hosts" && seg[2] === "enrollment-tokens" && seg.length === 4) {
      return json({ status: "pending" } satisfies S["Enrollment"]);
    }
    if (path === "/v1/machines/catalog") return json(CATALOG);
    if (path === "/v1/machines/default") return json(defaultMachine(url.searchParams.get("account")));
    if (path === "/v1/github/repos") {
      const q = (url.searchParams.get("q") ?? "").toLowerCase();
      return json(REPOS.filter((repo) => repo.slug.toLowerCase().includes(q)));
    }
    if (/^\/v1\/github\/repos\/[^/]+\/[^/]+\/branches$/.test(path)) {
      return json({ branches: [{ name: "dev", is_default: true }, { name: "main", is_default: false }, { name: "fix/worker-usage-caps", is_default: false }], next_cursor: null });
    }
    if (path === "/v1/approvals") {
      const wanted = url.searchParams.get("session");
      return json(APPROVALS.filter((approval) => wanted === null || approval.session === wanted));
    }
    if (path === "/v1/api-keys") return json(API_KEYS);
    if (path === "/v1/mcp-servers") return json(MCP_SERVERS);
    if (path === "/v1/catalog/mcp-servers") {
      const q = (url.searchParams.get("search") ?? "").toLowerCase();
      return json({ servers: CATALOG_MCP.filter((server) => server.name.toLowerCase().includes(q)), next_cursor: null } satisfies S["McpCatalogPage"]);
    }
    if (path === "/v1/skills") return json(SKILLS);
    if (path === "/v1/marketplaces") return json(MARKETPLACES);
    if (path === "/v1/catalog/skills") {
      // A marketplace added in this preview is read on the second look, the
      // way the queue reads a real one a moment after it is added.
      const pending = [...PENDING_MARKETPLACES];
      const answer = json({ skills: [...CATALOG_SKILLS], pending, failed: [FAILED_MARKETPLACE] } satisfies S["SkillCatalog"]);
      // What the queue read lands on the next look, not this one.
      PENDING_MARKETPLACES.clear();
      for (const repo of pending) {
        CATALOG_SKILLS.push({ marketplace: repo, plugin: repo.split("/")[1] ?? repo, name: "example-skill", description: `What ${repo} publishes, read once the queue had it.` });
      }
      return answer;
    }
    if (path === "/v1/memory") return json(MEMORY);
    if (path === "/v1/agents-md") return json(AGENTS_MD);
    if (path === "/v1/usage/llm") return json(LLM_USAGE);
    if (path === "/v1/usage/cloud") return json([]);
    if (path === "/v1/push/vapid-public-key") return json({ key: "BPreviewKeyPreviewKeyPreviewKeyPreviewKeyPreviewKeyPreviewKeyPreviewKeyPreviewKeyPreviewKey" });
    if (sessionId !== undefined) {
      const found = SESSIONS.find((candidate) => candidate.id === sessionId);
      if (found === undefined) return problem(404, "not-found", "no such session");
      const rest = seg.slice(3).join("/");
      if (rest === "") return json(found);
      if (rest === "events") return json({ events: EVENTS[sessionId] ?? [], more: false });
      if (rest === "machine") return json(machineOf(sessionId));
      if (rest === "budget") return json(found.budget);
      if (rest === "repo-status") return json({ checkouts: found.repos.map((repo) => ({ dir: repo.dir, dirty: sessionId === SESSION_IDS.budgets, summary: sessionId === SESSION_IDS.budgets ? " M crates/api/src/app.rs\n?? crates/api/src/request_budget.rs" : "" })) });
      if (rest === "env") return json({ entries: [{ key: "RUST_LOG", value: "info" }], warning: "Agents can read this file but not edit it." });
      if (rest === "files") {
        return json({
          path: url.searchParams.get("path") ?? "",
          truncated: false,
          entries: [
            { name: "crates", path: "crates", kind: "directory", size_bytes: null, ignored: false },
            { name: "frontend", path: "frontend", kind: "directory", size_bytes: null, ignored: false },
            { name: "target", path: "target", kind: "directory", size_bytes: null, ignored: true },
            { name: "AGENTS.md", path: "AGENTS.md", kind: "file", size_bytes: 6_120, ignored: false },
            { name: "Cargo.toml", path: "Cargo.toml", kind: "file", size_bytes: 1_204, ignored: false },
          ],
        });
      }
      if (rest === "files/content") return json({ path: url.searchParams.get("path") ?? "AGENTS.md", text: AGENTS_MD.content, bytes: AGENTS_MD.content.length });
      if (rest === "diff") {
        return json({
          base: "origin/dev",
          added_lines: 212,
          removed_lines: 9,
          truncated: false,
          files: [
            { path: "crates/api/src/request_budget.rs", change: "added", added_lines: 180, removed_lines: 0, patch: "@@ -0,0 +1,4 @@\n+//! Per-principal request budgets (issue #342).\n+\n+use std::collections::HashMap;\n+" },
            { path: "crates/api/src/app.rs", change: "modified", added_lines: 32, removed_lines: 9, patch: "@@ -120,7 +120,8 @@\n-    with_rooms(Route::new((routes(), frontend())))\n+    with_rooms(Route::new((routes(), frontend())))\n+        .with(request_budget::RequestBudget::new(limiter, limits))" },
          ],
        });
      }
      if (rest === "turns") return json({ turns: [], next_cursor: null });
      if (rest === "harness-session") return json({ harness_session_id: "careful-vibraphone" });
    }
    console.log(`404 ${method} ${path}${url.search}`);
    return problem(404, "not-found", `preview: no fixture for GET ${path}`);
  }

  if (method === "POST" && path === "/v1/sessions") {
    const input = JSON.parse(body || "{}") as { prompt?: string };
    const created = session("1a2b3c4d-00ff-4000-8000-0000000000ff", (input.prompt ?? "New session").slice(0, 60), { slug: "lexoliu/flyco", branch: "dev" }, "provisioning", "working", 0);
    SESSIONS.unshift(created);
    EVENTS[created.id] = stored([{ at: NOW, event: { type: "user_message", origin: "user", text: input.prompt ?? "" } }]);
    return json(created, 201);
  }
  if (method === "POST" && path === "/v1/hosts/enrollment-tokens") {
    const token: S["EnrollmentToken"] = {
      id: "e0000000-0000-4000-8000-0000000000ff",
      command: "curl -fsSL https://dev.flyco.dev/enroll.sh | sh -s -- fh_preview_7Kd2mQx9vRt4nWpLzYb3cA8eF1gH",
      expires_at_unix: NOW + 15 * 60,
      token: "fh_preview_7Kd2mQx9vRt4nWpLzYb3cA8eF1gH",
    };
    return json(token, 201);
  }
  if (method === "POST" && path === "/v1/catalog/mcp-servers") {
    const input = JSON.parse(body || "{}") as S["InstallCatalogMcpServer"];
    const entry = CATALOG_MCP.find((server) => server.name === input.server);
    const install = entry?.installs.find((candidate) => candidate.kind === input.kind);
    if (entry === undefined || install === undefined) return problem(404, "catalog-server-not-found", `the catalog no longer lists ${input.server}`);
    const missing = install.inputs.find((field) => field.required && !(input.values?.[field.key] ?? field.default));
    if (missing !== undefined) return problem(422, "catalog-input-missing", `the catalog entry needs a value for ${missing.key}`);
    const name = input.name ?? entry.suggested_name;
    if (MCP_SERVERS.some((server) => server.name === name)) return problem(409, "mcp-server-name-taken", `you already registered an MCP server called ${name}`);
    const config: S["McpServerConfig"] = install.kind === "remote"
      ? { transport: "http", url: `https://${install.label.replace("Remote · ", "")}/mcp`, headers: install.inputs.map((field) => ({ name: "Authorization", value: `Bearer ${input.values?.[field.key] ?? ""}` })) }
      : { transport: "stdio", command: install.kind === "npm" ? "npx" : "uvx", args: install.label.split(" ").slice(1), env: [] };
    const created: S["McpServerView"] = { id: `s0000000-0000-4000-8000-${String(MCP_SERVERS.length + 1).padStart(12, "0")}`, name, enabled: true, updated_at_unix: NOW, config };
    MCP_SERVERS.push(created);
    return json(created, 201);
  }
  if (method === "POST" && path === "/v1/marketplaces") {
    const input = JSON.parse(body || "{}") as S["AddMarketplace"];
    const repo = input.repo.trim();
    if (!/^[^/\s]+\/[^/\s]+$/.test(repo)) return problem(422, "invalid-marketplace", "a marketplace is a GitHub `owner/name`");
    if (MARKETPLACES.some((entry) => entry.repo === repo)) return problem(409, "marketplace-already-added", `you already added ${repo}`);
    const added: S["MarketplaceView"] = { id: `m0000000-0000-4000-8000-${String(MARKETPLACES.length + 1).padStart(12, "0")}`, repo, git_ref: input.git_ref ?? null, built_in: false, added_at_unix: NOW };
    MARKETPLACES.push(added);
    PENDING_MARKETPLACES.add(repo);
    return json(added, 201);
  }
  if (method === "POST" && path === "/v1/catalog/skills") {
    const input = JSON.parse(body || "{}") as S["InstallCatalogSkill"];
    const entry = CATALOG_SKILLS.find((skill) => skill.marketplace === input.marketplace && skill.plugin === input.plugin && skill.name === input.name);
    if (entry === undefined) return problem(404, "catalog-skill-not-found", `${input.marketplace} no longer offers ${input.name}`);
    if (input.scopes.length === 0) return problem(422, "no-skill-scope", "a skill is installed for at least one harness");
    const installed = input.scopes.map((scope, index): S["SkillView"] => ({ id: `sk000000-0000-4000-8000-${String(SKILLS.length + index + 1).padStart(12, "0")}`, name: entry.name, scope, size_bytes: 12_288, uploaded_at_unix: NOW }));
    SKILLS.push(...installed);
    return json(installed, 201);
  }
  if (method === "PUT" && /^\/v1\/sessions\/[^/]+\/awake$/.test(path)) {
    const id = path.split("/")[3] ?? "";
    const input = JSON.parse(body || "{}") as S["KeepAwake"];
    const target = SESSIONS.find((entry) => entry.id === id);
    if (target === undefined) return problem(404, "session-not-found", "preview: no such session");
    // The live clock, not the fixture's `NOW`: a hold is read back as the
    // time it has left, and a constant captured at startup would make a
    // fresh 4h hold read as the server's uptime short of four hours.
    target.awake_until_unix =
      input.minutes === undefined || input.minutes === null
        ? null
        : Math.floor(Date.now() / 1000) + input.minutes * MINUTE;
    return json(target);
  }
  if (method === "POST" && path === "/v1/api-keys") {
    return json({ id: "k0000000-0000-4000-8000-0000000000ff", label: "Preview key", created_at_unix: NOW, last_used_unix: null, key: "fk_preview_2Qv8xLmR4pT7nWzKcYbA9sD3fG6h" }, 201);
  }
  // Every other write is accepted and forgotten: the preview is for looking.
  console.log(`204 ${method} ${path}`);
  return new Response(null, { status: 204 });
}

function cors(request: Request, response: Response): Response {
  const headers = new Headers(response.headers);
  headers.set("access-control-allow-origin", request.headers.get("origin") ?? "*");
  headers.set("access-control-allow-credentials", "true");
  headers.set("access-control-allow-headers", "authorization, content-type, idempotency-key, last-event-id, x-flyco-schema");
  headers.set("access-control-allow-methods", "GET, POST, PUT, PATCH, DELETE, OPTIONS");
  headers.set("access-control-expose-headers", "retry-after, x-flyco-transcript-batches");
  return new Response(response.body, { status: response.status, headers });
}

Bun.serve({
  port: PORT,
  async fetch(request) {
    if (request.method === "OPTIONS") {
      return cors(request, new Response(null, { status: 204 }));
    }
    const url = new URL(request.url);
    const body = request.method === "GET" ? "" : await request.text();
    return cors(request, route(request.method, url.pathname, url, body));
  },
});

console.log(`preview control plane on http://localhost:${PORT} — sign in at the dev server's /auth/complete#token=preview`);
