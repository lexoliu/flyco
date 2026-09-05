import "@testing-library/jest-dom/vitest";
import { afterEach, beforeEach, vi } from "vitest";
import { cleanup } from "@solidjs/testing-library";

afterEach(() => {
  cleanup();
  localStorage.clear();
  sessionStorage.clear();
});

// jsdom doesn't implement scrollTo; the router calls it after navigation
// (e.g. following a hash) and logs a noisy "not implemented" error otherwise.
window.scrollTo = () => {
  /* no-op in tests */
};

// Nor does jsdom implement window.open, which both sign-ins use to put the
// vendor's own page in a tab of its own.
window.open = () => null;

// vite-plugin-pwa's virtual module only exists inside a real Vite build;
// components that call registerSW() need a stand-in for it under Vitest.
vi.mock("virtual:pwa-register", () => ({
  registerSW: vi.fn(() => async () => {
    /* no-op in tests */
  }),
}));

/**
 * jsdom has no `WebSocket` global. Every component that opens the session
 * relay (see src/api/relay.ts) needs *something* to construct without
 * throwing; this stub never fires `open`, so a mounted component sees a
 * socket stuck at "connecting" rather than a crash — good enough for
 * rendering assertions that don't depend on the socket going live.
 */
class StubWebSocket extends EventTarget {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;
  readonly url: string;
  readyState = StubWebSocket.CONNECTING;

  constructor(url: string) {
    super();
    this.url = url;
  }

  send(): void {
    throw new Error("StubWebSocket never reaches OPEN; nothing should call send() on it in tests");
  }

  close(): void {
    this.readyState = StubWebSocket.CLOSED;
  }
}

vi.stubGlobal("WebSocket", StubWebSocket);

/**
 * A minimal, in-memory stand-in for the control plane, so every route's
 * smoke test can mount without reaching the real network. Endpoints not
 * listed here answer like an unimplemented one does today: `501` with a
 * `.../not-implemented` problem document (see src/api/problem.ts) — the
 * same "not built yet" shape most of this API actually returns.
 */
function problemResponse(status: number, type: string, title: string): Response {
  return new Response(JSON.stringify({ type, title, status, detail: title }), {
    status,
    headers: { "content-type": "application/problem+json" },
  });
}

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

/** The one line the host wizard shows, as the control plane renders it. */
export const HOST_ENROLL_COMMAND =
  "curl -fsSL https://dev.flyco.dev/install/flycod.sh | sudo sh -s -- host enroll fh_2Qv8xLmR4pT7nWzKcYbA";

const NOT_IMPLEMENTED = () =>
  problemResponse(501, "https://flyco.dev/problems/not-implemented", "Not implemented");

function mockFetch(input: string | URL | Request, init?: RequestInit): Promise<Response> {
  const method = (init?.method ?? "GET").toUpperCase();
  const url = new URL(typeof input === "string" ? input : input instanceof URL ? input : input.url);
  const path = url.pathname;

  if (method === "GET" && path === "/v1/me") {
    return Promise.resolve(jsonResponse({ id: "user-1", login: "octocat", session_cap: 5 }));
  }
  if (method === "GET" && path === "/v1/sessions") {
    return Promise.resolve(jsonResponse([]));
  }
  if (method === "GET" && /^\/v1\/sessions\/[^/]+$/.test(path)) {
    const id = path.split("/").pop() ?? "";
    return Promise.resolve(
      jsonResponse({
        id,
        title: "Audit the relay for dropped frames",
        repo: "octocat/hello-world",
        harness: "claude_code",
        // Freshly opened, machine still being built: what a new user sees
        // first, and the state whose empty transcript is easiest to get wrong.
        state: "provisioning",
        machine_origin: "auto",
        created_at_unix: 0,
        last_active_unix: 0,
        budget: { limit: 10_000_000, spent: 0, remaining: 10_000_000, stage: "ok" },
      }),
    );
  }
  if (method === "GET" && /^\/v1\/sessions\/[^/]+\/events$/.test(path)) {
    return Promise.resolve(jsonResponse({ events: [], more: false }));
  }
  if (method === "POST" && /^\/v1\/sessions\/[^/]+\/relay-ticket$/.test(path)) {
    return Promise.resolve(jsonResponse({ ticket: "frt_test", expires_at_unix: 0 }));
  }
  if (method === "GET" && path === "/v1/mcp-servers") {
    return Promise.resolve(jsonResponse([]));
  }
  if (method === "GET" && path === "/v1/skills") {
    return Promise.resolve(jsonResponse([]));
  }
  if (method === "GET" && path === "/v1/providers") {
    return Promise.resolve(jsonResponse([]));
  }
  if (method === "GET" && path === "/v1/hosts") {
    return Promise.resolve(jsonResponse([]));
  }
  if (method === "POST" && path === "/v1/hosts/enrollment-tokens") {
    return Promise.resolve(
      jsonResponse(
        {
          id: "3f2b1c9d-6a4e-4d8b-9f21-7c5a0e3b8d14",
          token: "fh_2Qv8xLmR4pT7nWzKcYbA",
          // Ten minutes out from whatever "now" the run happens at, so the
          // command is live rather than stale the moment it renders.
          expires_at_unix: Math.floor(Date.now() / 1000) + 600,
          command: HOST_ENROLL_COMMAND,
        },
        201,
      ),
    );
  }
  // The default is a machine nobody has run the command on yet, so a wizard
  // that opens and waits is the ordinary case; a test that wants the other
  // outcome routes this path itself.
  if (method === "GET" && /^\/v1\/hosts\/enrollment-tokens\/[^/]+$/.test(path)) {
    return Promise.resolve(jsonResponse({ status: "pending" }));
  }
  if (method === "GET" && path === "/v1/api-keys") {
    return Promise.resolve(jsonResponse([]));
  }
  if (method === "GET" && path === "/v1/github/repos") {
    return Promise.resolve(jsonResponse([]));
  }
  if (method === "GET" && /^\/v1\/github\/repos\/[^/]+\/[^/]+\/branches$/.test(path)) {
    return Promise.resolve(
      jsonResponse({
        branches: [
          { name: "main", is_default: true },
          { name: "dev", is_default: false },
        ],
        next_cursor: null,
      }),
    );
  }
  if (method === "GET" && path === "/v1/machines/catalog") {
    // Read, and offering nothing: a test that wants the other answer — an
    // account flyco has not finished reading — routes this path itself.
    return Promise.resolve(jsonResponse({ entries: [], pending_accounts: [] }));
  }
  if (method === "GET" && /^\/v1\/sessions\/[^/]+\/machine$/.test(path)) {
    const id = path.split("/")[3] ?? "";
    return Promise.resolve(
      jsonResponse({
        id: "machine-1",
        session: id,
        spec: { provider: "aws", machine_type: "t3.large", region: "us-east-1", spot: true, disk_gib: 40 },
        state: "running",
        spot: true,
        region: "us-east-1",
        hourly: 45_000,
        created_at_unix: 0,
      }),
    );
  }
  if (method === "GET" && /^\/v1\/sessions\/[^/]+\/repo-status$/.test(path)) {
    return Promise.resolve(jsonResponse({ dirty: false, summary: "" }));
  }
  if (method === "GET" && /^\/v1\/sessions\/[^/]+\/files$/.test(path)) {
    return Promise.resolve(
      jsonResponse({
        path: url.searchParams.get("path") ?? "",
        truncated: false,
        entries: [
          { name: "src", path: "src", kind: "directory", size_bytes: null, ignored: false },
          { name: "README.md", path: "README.md", kind: "file", size_bytes: 812, ignored: false },
        ],
      }),
    );
  }
  if (method === "GET" && /^\/v1\/sessions\/[^/]+\/files\/content$/.test(path)) {
    return Promise.resolve(
      jsonResponse({
        path: url.searchParams.get("path") ?? "README.md",
        text: "# flyco\n",
        bytes: 8,
      }),
    );
  }
  if (method === "GET" && /^\/v1\/sessions\/[^/]+\/diff$/.test(path)) {
    return Promise.resolve(
      jsonResponse({
        base: "origin/main",
        files: [],
        added_lines: 0,
        removed_lines: 0,
        truncated: false,
      }),
    );
  }
  if (method === "GET" && /^\/v1\/sessions\/[^/]+\/env$/.test(path)) {
    return Promise.resolve(
      jsonResponse({ entries: [], warning: "Agents can read this file but not edit it." }),
    );
  }
  if (method === "GET" && path === "/v1/harness-accounts") {
    return Promise.resolve(jsonResponse([]));
  }
  if (method === "POST" && path === "/v1/harness-accounts") {
    return Promise.resolve(
      jsonResponse(
        {
          id: "harness-1",
          harness: "codex",
          label: "OpenAI API key",
          linked_at_unix: 1_787_000_000,
          expires_at_unix: null,
        },
        201,
      ),
    );
  }
  if (method === "POST" && path === "/v1/harness-accounts/claude/oauth/start") {
    return Promise.resolve(
      jsonResponse({
        attempt_id: "11111111-2222-4333-8444-555555555555",
        authorize_url:
          "https://claude.ai/oauth/authorize?code=true&client_id=test&state=the-state",
      }),
    );
  }
  if (method === "POST" && path === "/v1/harness-accounts/claude/oauth/complete") {
    return Promise.resolve(
      jsonResponse(
        {
          id: "harness-2",
          harness: "claude_code",
          label: "me@lexo.cool",
          linked_at_unix: 1_787_000_000,
          expires_at_unix: 1_787_028_800,
        },
        201,
      ),
    );
  }
  if (method === "POST" && path === "/v1/harness-accounts/codex/oauth/start") {
    return Promise.resolve(
      jsonResponse({
        attempt_id: "99999999-8888-4777-8666-555555555555",
        user_code: "FLYC-8QK2",
        verification_url: "https://auth.openai.com/codex/device",
        interval_seconds: 1,
      }),
    );
  }
  // The default is a sign-in nobody has approved yet, so a card that opens
  // and waits is the ordinary case; a test that wants the other outcomes
  // routes this path itself.
  if (method === "GET" && /^\/v1\/harness-accounts\/codex\/oauth\/[^/]+$/.test(path)) {
    return Promise.resolve(jsonResponse({ state: "pending" }));
  }
  if (method === "GET" && path === "/v1/memory") {
    return Promise.resolve(jsonResponse([]));
  }
  if (method === "GET" && path === "/v1/agents-md") {
    return Promise.resolve(jsonResponse({ content: "", updated_at_unix: 0 }));
  }
  if (method === "GET" && path === "/v1/approvals") {
    return Promise.resolve(jsonResponse([]));
  }
  if (method === "GET" && path === "/v1/harness-features") {
    return Promise.resolve(jsonResponse([]));
  }
  if (method === "GET" && (path === "/v1/usage/llm" || path === "/v1/usage/cloud")) {
    return Promise.resolve(jsonResponse([]));
  }

  return Promise.resolve(NOT_IMPLEMENTED());
}

beforeEach(() => {
  vi.stubGlobal("fetch", vi.fn(mockFetch));
});
