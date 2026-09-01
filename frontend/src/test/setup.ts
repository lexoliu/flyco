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
        repo: "octocat/hello-world",
        harness: "claude_code",
        state: "active",
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
  if (method === "GET" && path === "/v1/api-keys") {
    return Promise.resolve(jsonResponse([]));
  }
  if (method === "GET" && path === "/v1/github/repos") {
    return Promise.resolve(jsonResponse([]));
  }
  if (method === "GET" && path === "/v1/machines/catalog") {
    return Promise.resolve(jsonResponse([]));
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
  if (method === "GET" && /^\/v1\/sessions\/[^/]+\/env$/.test(path)) {
    return Promise.resolve(
      jsonResponse({ entries: [], warning: "Agents can read this file but not edit it." }),
    );
  }
  if (method === "GET" && path === "/v1/harness-accounts") {
    return Promise.resolve(jsonResponse([]));
  }
  if (method === "GET" && path === "/v1/memory") {
    return Promise.resolve(jsonResponse([]));
  }
  if (method === "GET" && path === "/v1/agents-md") {
    return Promise.resolve(jsonResponse({ content: "", updated_at_unix: 0 }));
  }

  return Promise.resolve(NOT_IMPLEMENTED());
}

beforeEach(() => {
  vi.stubGlobal("fetch", vi.fn(mockFetch));
});
