import { describe, expect, it, onTestFinished, vi } from "vitest";
import { render } from "@solidjs/testing-library";
import {
  MemoryRouter,
  Navigate,
  Route,
  createMemoryHistory,
} from "@solidjs/router";
import AppShell from "../components/AppShell";
import Login from "../routes/Login";
import AuthComplete from "../routes/AuthComplete";
import Home from "../routes/Home";
import Welcome from "../routes/Welcome";
import { ConnectCompute, ConnectHarness } from "../routes/connect/Connect";
import SessionDetail from "../routes/SessionDetail";
import SettingsLayout from "../routes/settings/SettingsLayout";
import AgentsSection from "../routes/settings/AgentsSection";
import ComputeSection from "../routes/settings/ComputeSection";
import ToolsSection from "../routes/settings/ToolsSection";
import InstructionsSection from "../routes/settings/InstructionsSection";
import AccountSection from "../routes/settings/AccountSection";
import NotFound from "../routes/NotFound";
import { HOST_ENROLL_COMMAND } from "./setup";
import { consumePostLoginPath } from "../lib/postLoginPath";
import { clearSessionToken, setSessionToken } from "../lib/session";
import { dismissWelcome } from "../lib/localPreferences";

/**
 * Renders the same route tree as src/main.tsx, starting at a given path.
 *
 * Uses `MemoryRouter` with a fresh, per-call history instead of the
 * browser-history-backed `Router`: jsdom exposes a single real
 * `window.location` shared by the whole test file, so a `Router url={...}`
 * prop only ever takes effect once per file. A dedicated in-memory history
 * per render keeps each test's navigation fully isolated.
 */
function renderAt(url: string, signedIn = true, seenWelcome = true) {
  clearSessionToken();
  if (signedIn) {
    setSessionToken("fs_route_test");
  }
  // Every route test but the welcome one starts from a browser that has
  // already been through the first run; otherwise `/` redirects there.
  if (seenWelcome) {
    dismissWelcome();
  }
  const history = createMemoryHistory();
  history.set({ value: url, replace: true, scroll: false });
  return render(() => (
    <MemoryRouter history={history} root={AppShell}>
      <Route path="/login" component={Login} />
      <Route path="/auth/complete" component={AuthComplete} />
      <Route path="/" component={Home} />
      <Route path="/welcome" component={Welcome} />
      <Route path="/connect/harness" component={ConnectHarness} />
      <Route path="/connect/compute" component={ConnectCompute} />
      <Route path="/sessions/:id" component={SessionDetail} />
      <Route path="/settings" component={SettingsLayout}>
        <Route
          path="/"
          component={() => <Navigate href="/settings/agents" />}
        />
        <Route path="/agents" component={AgentsSection} />
        <Route path="/compute" component={ComputeSection} />
        <Route path="/tools" component={ToolsSection} />
        <Route path="/instructions" component={InstructionsSection} />
        <Route path="/account" component={AccountSection} />
      </Route>
      <Route path="*404" component={NotFound} />
    </MemoryRouter>
  ));
}

describe("route smoke tests", () => {
  it("renders /login with the sign-in call to action", () => {
    const { getByText } = renderAt("/login", false);
    expect(getByText("Sign in with GitHub")).toBeInTheDocument();
  });

  it("redirects a signed-out first visit to GitHub sign-in without calling protected APIs", async () => {
    const { findByText, queryByRole } = renderAt("/", false);

    expect(await findByText("Sign in with GitHub")).toBeInTheDocument();
    expect(queryByRole("heading", { level: 1 })?.textContent).not.toContain(
      "What should we build",
    );
    expect(fetch).not.toHaveBeenCalled();
  });

  it("restores the protected destination after sign-in", async () => {
    const { findByText } = renderAt("/sessions/abc-123?panel=terminal", false);

    expect(await findByText("Sign in with GitHub")).toBeInTheDocument();
    expect(consumePostLoginPath()).toBe("/sessions/abc-123?panel=terminal");
  });

  it("redirects a signed-in visitor away from /login", async () => {
    const { findByText } = renderAt("/login");

    expect(await findByText("Welcome back, octocat")).toBeInTheDocument();
  });

  it("clears a rejected session and returns to sign-in", async () => {
    vi.mocked(fetch).mockResolvedValue(
      new Response(
        JSON.stringify({
          type: "https://flyco.dev/problems/invalid-credential",
          title: "Unauthorized",
          status: 401,
          detail: "the session expired",
        }),
        {
          status: 401,
          headers: { "content-type": "application/problem+json" },
        },
      ),
    );
    const { findByText } = renderAt("/");

    expect(await findByText("Sign in with GitHub")).toBeInTheDocument();
    expect(localStorage.getItem("flyco.session_token")).toBeNull();
  });

  it("renders /auth/complete as an explicit error state without a token", () => {
    const { getByRole } = renderAt("/auth/complete", false);
    expect(getByRole("alert")).toBeInTheDocument();
  });

  it("renders / as the composer over an empty session list", async () => {
    const { getByLabelText, findByText } = renderAt("/");
    expect(getByLabelText("Describe a task")).toBeInTheDocument();
    expect(
      await findByText("No sessions yet. Describe a task above to start one."),
    ).toBeInTheDocument();
  });

  it("refuses to send until every prerequisite is present", async () => {
    const { findByRole } = renderAt("/");
    // Nothing is linked in the fixture, so the button says which of the
    // three prerequisites is missing rather than failing on submit.
    expect(
      await findByRole("button", { name: "Connect an agent first" }),
    ).toBeDisabled();
  });

  it("sends a first visitor with nothing linked to the welcome flow", async () => {
    const { findByRole } = renderAt("/", true, false);
    expect(
      await findByRole("heading", { level: 1, name: "Meet flyco" }),
    ).toBeInTheDocument();
  });

  it("renders /welcome as the first page of the linear flow", async () => {
    const { findByRole, getByRole } = renderAt("/welcome");
    expect(
      await findByRole("heading", { level: 1, name: "Meet flyco" }),
    ).toBeInTheDocument();
    expect(getByRole("button", { name: "Next" })).toBeInTheDocument();
  });

  it("lists every agent next, never asking which one, with nothing to skip", async () => {
    const { findByRole, getByRole, queryByRole } = renderAt("/welcome");
    await findByRole("heading", { level: 1, name: "Meet flyco" });
    expect(
      queryByRole("button", { name: "Skip for now" }),
    ).not.toBeInTheDocument();
    getByRole("button", { name: "Next" }).click();

    // The agent is chosen per task in the composer, so the first run only
    // links agents: one page listing them all, nothing to skip and no way
    // past it with nothing linked.
    expect(
      await findByRole("heading", {
        level: 1,
        name: "Link the agents you use",
      }),
    ).toBeInTheDocument();
    expect(getByRole("radio", { name: /^Claude Code/ })).toBeInTheDocument();
    expect(getByRole("radio", { name: /^Codex/ })).toBeInTheDocument();
    expect(getByRole("button", { name: "Next" })).toBeDisabled();
    expect(
      queryByRole("button", { name: "Skip for now" }),
    ).not.toBeInTheDocument();
  });

  it("renders /connect/harness as stage B on its own", async () => {
    const { findByRole, getByRole } = renderAt("/connect/harness");
    expect(
      await findByRole("heading", {
        level: 1,
        name: "Link the agents you use",
      }),
    ).toBeInTheDocument();
    // Opened from somewhere else, so Back on the first page leads back there.
    expect(getByRole("button", { name: "Back" })).toBeInTheDocument();
  });

  it("walks only the named agent's page on /connect/harness?agent=", async () => {
    const { findByRole } = renderAt("/connect/harness?agent=codex");
    expect(
      await findByRole("heading", { level: 1, name: "Link Codex" }),
    ).toBeInTheDocument();
  });

  it("renders /connect/compute as stage C on its own", async () => {
    const { findByRole } = renderAt("/connect/compute");
    expect(
      await findByRole("heading", {
        level: 1,
        name: "Where should sessions run?",
      }),
    ).toBeInTheDocument();
  });

  it("enrolls a machine on /connect/compute, command and all", async () => {
    const { findByRole, findByText, getByRole, getByText } =
      renderAt("/connect/compute");
    await findByRole("heading", {
      level: 1,
      name: "Where should sessions run?",
    });

    getByRole("radio", { name: /Your own machine/ }).click();
    getByRole("button", { name: "Next" }).click();

    // The command comes from the control plane, which is the only thing
    // that knows this deployment's own origin.
    expect(await findByText(HOST_ENROLL_COMMAND)).toBeInTheDocument();
    expect(getByRole("button", { name: "Copy" })).toBeInTheDocument();
    expect(getByText(/needs Podman/)).toBeInTheDocument();
    expect(getByRole("status")).toHaveTextContent("Waiting for the machine…");
    expect(getByRole("button", { name: "Next" })).toHaveAttribute(
      "title",
      "Run the command on the machine to continue",
    );
  });

  it("renders /sessions/:id as a header, a transcript and a composer", async () => {
    const { findByText, getByLabelText } = renderAt("/sessions/abc-123");

    // The title, not the id: a session is identified by what it is for.
    expect(
      await findByText("Audit the relay for dropped frames"),
    ).toBeInTheDocument();
    expect(getByLabelText("Message the agent")).toBeInTheDocument();
    // The prompt already went out with `POST /v1/sessions` and the machine
    // is being built, so the empty transcript is the build itself — and
    // the one sentence a new user needs, which is that nothing is being
    // asked of them.
    expect(
      await findByText(
        "Your task is queued and will start as soon as the machine is ready.",
      ),
    ).toBeInTheDocument();
    expect(getByLabelText("Provisioning")).toBeInTheDocument();
    expect(await findByText("Reserving a machine on AWS")).toBeInTheDocument();
  });

  it("asks a running session with no events for a message", async () => {
    const base = vi.mocked(fetch).getMockImplementation();
    vi.mocked(fetch).mockImplementation(async (input, init) => {
      const url = new URL(String(input instanceof Request ? input.url : input));
      const response = await base!(input, init);
      if (!/^\/v1\/sessions\/[^/]+$/.test(url.pathname)) {
        return response;
      }
      // The same session, but with a machine that is up and an agent that
      // has nothing to do until someone speaks.
      const session = (await response.json()) as Record<string, unknown>;
      return new Response(
        JSON.stringify({ ...session, state: "active", activity: "idle" }),
        {
          status: 200,
          headers: { "content-type": "application/json" },
        },
      );
    });
    const { findByText, queryByLabelText } = renderAt("/sessions/abc-123");

    expect(
      await findByText(
        "Nothing has happened yet. Send a message to get the agent started.",
      ),
    ).toBeInTheDocument();
    expect(queryByLabelText("Provisioning")).not.toBeInTheDocument();
  });

  it("renders a session that does not exist as a problem with a way back", async () => {
    // A 404 on the session is definitive: the relay stops instead of
    // backing off forever behind a `Reconnecting…` pill that will never
    // become anything (issue #137).
    const base = vi.mocked(fetch).getMockImplementation();
    vi.mocked(fetch).mockImplementation(async (input, init) => {
      const url = new URL(String(input instanceof Request ? input.url : input));
      if (!url.pathname.startsWith("/v1/sessions/abc-123")) {
        return base!(input, init);
      }
      return new Response(
        JSON.stringify({
          type: "https://flyco.dev/problems/not-found",
          title: "Not Found",
          status: 404,
          detail: "no session with that id",
        }),
        {
          status: 404,
          headers: { "content-type": "application/problem+json" },
        },
      );
    });
    const { findByRole, getByRole, queryByText } =
      renderAt("/sessions/abc-123");

    const notice = await findByRole("alert");
    expect(notice).toHaveTextContent("no session with that id");
    expect(
      getByRole("button", { name: "Back to sessions" }),
    ).toBeInTheDocument();
    expect(queryByText("Reconnecting…")).not.toBeInTheDocument();
    expect(queryByText("Loading")).not.toBeInTheDocument();
  });

  it("re-reads the machine when the room says the session moved onto another one", async () => {
    // The header quotes a rate, and a rate for the machine the session used
    // to be on is a bill nobody is being sent (issue #135). The relay says
    // the session moved; the page asks the control plane what it moved to.
    const original = globalThis.WebSocket;
    const sockets: EventTarget[] = [];
    class DrivableSocket extends EventTarget {
      readyState = 1;
      constructor(readonly url: string) {
        super();
        sockets.push(this);
        queueMicrotask(() => this.dispatchEvent(new Event("open")));
      }
      send(): void {
        /* nothing in this test speaks to the room */
      }
      close(): void {
        this.readyState = 3;
      }
    }
    vi.stubGlobal("WebSocket", DrivableSocket);
    onTestFinished(() => {
      vi.stubGlobal("WebSocket", original);
    });

    const base = vi.mocked(fetch).getMockImplementation();
    let reads = 0;
    vi.mocked(fetch).mockImplementation(async (input, init) => {
      const url = new URL(String(input instanceof Request ? input.url : input));
      if (!/^\/v1\/sessions\/[^/]+\/machine$/.test(url.pathname)) {
        return base!(input, init);
      }
      reads += 1;
      const response = await base!(input, init);
      const machine = (await response.json()) as Record<string, unknown>;
      // The first read is the machine the session started on; every read
      // after the move answers with the one it moved to.
      return new Response(
        JSON.stringify(
          reads === 1
            ? machine
            : {
                ...machine,
                hourly: 163_200,
                spec: {
                  ...(machine["spec"] as object),
                  machine_type: "m7g.xlarge",
                },
              },
        ),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    });

    const { findByText } = renderAt("/sessions/abc-123");
    expect(await findByText("t3.large · $0.04/hr · spot")).toBeInTheDocument();

    await vi.waitFor(() => expect(sockets.length).toBeGreaterThan(0));
    const socket = sockets[0];
    socket?.dispatchEvent(
      new MessageEvent("message", {
        data: JSON.stringify({
          type: "machine_changed",
          machine_type: "m7g.xlarge",
          hourly: 163_200,
          spot: true,
          restarted: true,
        }),
      }),
    );

    expect(
      await findByText("m7g.xlarge · $0.16/hr · spot"),
    ).toBeInTheDocument();
  });

  it("offers a failed session a resume, and takes it back to provisioning", async () => {
    // The page used to state the provider's error and offer nothing: no
    // way on, and a composer that refused without saying why (issue #133).
    const base = vi.mocked(fetch).getMockImplementation();
    let resumed = false;
    vi.mocked(fetch).mockImplementation(async (input, init) => {
      const url = new URL(String(input instanceof Request ? input.url : input));
      const method = (init?.method ?? "GET").toUpperCase();
      if (
        method === "POST" &&
        /^\/v1\/sessions\/[^/]+\/resume$/.test(url.pathname)
      ) {
        resumed = true;
        const response = await base!(
          new URL(url.href.replace("/resume", "")).href,
          {},
        );
        return response;
      }
      if (method !== "GET" || !/^\/v1\/sessions\/[^/]+$/.test(url.pathname)) {
        return base!(input, init);
      }
      const response = await base!(input, init);
      const session = (await response.json()) as Record<string, unknown>;
      return new Response(
        JSON.stringify(
          resumed
            ? session
            : {
                ...session,
                state: "failed",
                activity: "idle",
                failure:
                  "AWS refused the reservation: InsufficientInstanceCapacity.",
              },
        ),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    });

    const { findByRole, findByText, getByRole, getByText } =
      renderAt("/sessions/abc-123");

    const state = await findByRole("region", { name: "Session state" });
    expect(state.textContent).toContain("Failed");
    expect(state.textContent).toContain("AWS refused the reservation");
    // And the composer says why it is not taking a message.
    expect(
      getByText(
        "This session failed. Resume it to pick the conversation back up.",
      ),
    ).toBeInTheDocument();

    getByRole("button", { name: "Resume" }).click();

    // The answer is the session provisioning again, so the page comes back
    // as the build it now is rather than as the dead end it was.
    expect(
      await findByText(
        "Your task is queued and will start as soon as the machine is ready.",
      ),
    ).toBeInTheDocument();
    expect(resumed).toBe(true);
  });

  it("carries the budget raise on the paused session's own notice", async () => {
    // One notice per state, and a paused session's way out is the picker
    // rather than a second block beside it (#133, #134).
    const base = vi.mocked(fetch).getMockImplementation();
    vi.mocked(fetch).mockImplementation(async (input, init) => {
      const url = new URL(String(input instanceof Request ? input.url : input));
      if (
        (init?.method ?? "GET").toUpperCase() !== "GET" ||
        !/^\/v1\/sessions\/[^/]+$/.test(url.pathname)
      ) {
        return base!(input, init);
      }
      const response = await base!(input, init);
      const session = (await response.json()) as Record<string, unknown>;
      return new Response(
        JSON.stringify({
          ...session,
          state: "paused",
          activity: "idle",
          budget: {
            limit: 10_000_000,
            spent: 10_000_000,
            remaining: 0,
            stage: "exhausted",
          },
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    });

    const { findByRole, getByRole, getByText } = renderAt("/sessions/abc-123");

    const state = await findByRole("region", { name: "Session state" });
    expect(state.textContent).toContain("Paused · budget exhausted");
    expect(state.textContent).toContain(
      "The $10.00 budget is spent. Raise it to continue.",
    );

    // The action is the picker itself, and its floor is the first whole
    // dollar above the spend.
    getByRole("button", { name: "Raise budget" }).click();
    expect(
      (
        getByRole("slider", {
          name: "Session budget in dollars",
        }) as HTMLInputElement
      ).min,
    ).toBe("11");
    expect(
      getByText(
        "This session is paused: its budget is spent. Raise it to continue.",
      ),
    ).toBeInTheDocument();
  });

  it("keeps the session's side panels behind the collapsed drawer", async () => {
    const { findByText, queryByLabelText, getByLabelText } =
      renderAt("/sessions/abc-123");
    await findByText("Audit the relay for dropped frames");

    // docs/ux.md §9.4: the drawer starts closed, so the transcript gets
    // the width and the terminal is one click away rather than always on.
    expect(queryByLabelText("Session panels")).not.toBeInTheDocument();
    getByLabelText("Show the panel").click();
    expect(getByLabelText("Session panels")).toBeInTheDocument();
  });

  it("asks before unlinking the account every session's agent runs on", async () => {
    // `Unlink` used to fire `DELETE /v1/harness-accounts/{id}` on the first
    // click, with nothing said about what it costs (issue #139).
    const base = vi.mocked(fetch).getMockImplementation();
    const deletes: string[] = [];
    /** What the control plane answers the unlink with, per the test's turn. */
    let refuse = false;
    vi.mocked(fetch).mockImplementation(async (input, init) => {
      const url = new URL(String(input instanceof Request ? input.url : input));
      const method = (init?.method ?? "GET").toUpperCase();
      if (
        method === "DELETE" &&
        url.pathname.startsWith("/v1/harness-accounts/")
      ) {
        deletes.push(url.pathname);
        if (refuse) {
          return new Response(
            JSON.stringify({
              type: "https://flyco.dev/problems/harness-account-in-use",
              title: "Conflict",
              status: 409,
              detail:
                "2 session(s) still run on this account; archive them before unlinking",
              active_sessions: 2,
            }),
            {
              status: 409,
              headers: { "content-type": "application/problem+json" },
            },
          );
        }
        return new Response(null, { status: 204 });
      }
      if (method === "GET" && url.pathname === "/v1/harness-accounts") {
        return new Response(
          JSON.stringify([
            {
              id: "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d",
              harness: "claude_code",
              label: "lexo@lexo.cool",
              linked_at_unix: 1_787_000_000,
              expires_at_unix: null,
            },
          ]),
          { status: 200, headers: { "content-type": "application/json" } },
        );
      }
      return base!(input, init);
    });

    const { findByRole, findByText, getByRole, queryByRole } =
      renderAt("/settings/agents");

    (await findByRole("button", { name: "Unlink" })).click();
    const dialog = await findByRole("alertdialog");
    expect(dialog).toHaveTextContent("Unlink lexo@lexo.cool?");
    expect(dialog).toHaveTextContent("Signing in again links it back");
    expect(deletes).toEqual([]);

    getByRole("button", { name: "Keep it" }).click();
    expect(deletes).toEqual([]);

    // And when the control plane refuses because sessions still run on the
    // harness (issue #152), the dialog says how many and points at them
    // rather than offering the button again — there is no forcing this one.
    refuse = true;
    (await findByRole("button", { name: "Unlink" })).click();
    getByRole("button", { name: "Unlink" }).click();

    expect(
      await findByText(/2 sessions are still running there/),
    ).toBeInTheDocument();
    expect(queryByRole("button", { name: "Unlink" })).toBeNull();
    expect(getByRole("link", { name: "Go to Sessions" })).toHaveAttribute(
      "href",
      "/",
    );
  });

  it("renders /settings, redirecting to Agents", async () => {
    const { findByRole } = renderAt("/settings");
    expect(
      await findByRole("heading", { level: 2, name: "Agents" }),
    ).toBeInTheDocument();
  });

  it("offers all five sections in the settings navigation", async () => {
    const { findByRole, getByRole } = renderAt("/settings/agents");
    await findByRole("heading", { level: 2, name: "Agents" });
    for (const label of [
      "Agents",
      "Compute",
      "Tools",
      "Instructions",
      "Account",
    ]) {
      expect(getByRole("link", { name: label })).toBeInTheDocument();
    }
  });

  it("renders /settings/agents with a card for each harness", async () => {
    const { findByRole, getAllByText, getByText } =
      renderAt("/settings/agents");
    expect(
      await findByRole("heading", { level: 2, name: "Agents" }),
    ).toBeInTheDocument();
    // Both harnesses are named twice: once on their card, once in the
    // capability matrix under the disclosure.
    expect(getAllByText("Claude Code").length).toBeGreaterThan(0);
    expect(getAllByText("Codex").length).toBeGreaterThan(0);
    expect(getByText("What works on each harness")).toBeInTheDocument();
    expect(getAllByText("Not linked")).toHaveLength(2);
  });

  it("renders /settings/compute as an empty state with its one action", async () => {
    const { findByRole, getByRole } = renderAt("/settings/compute");
    expect(
      await findByRole("heading", { level: 2, name: "Compute" }),
    ).toBeInTheDocument();
    expect(getByRole("link", { name: "Add compute" })).toBeInTheDocument();
  });

  it("lists a machine the user owns beside the cloud accounts", async () => {
    // A host is a provider account like any other, so it is one more card in
    // the same run rather than a section of its own — and the facts on it
    // come from `GET /v1/hosts`, which is where a host's state lives.
    const base = vi.mocked(fetch).getMockImplementation();
    vi.mocked(fetch).mockImplementation((input, init) => {
      const url = new URL(String(input instanceof Request ? input.url : input));
      if (url.pathname === "/v1/hosts") {
        return Promise.resolve(
          new Response(
            JSON.stringify([
              {
                id: "8d1a6f30-4b7c-4e21-b0f5-9c2d6a7e4b11",
                label: "mercury",
                state: "online",
                facts: {
                  architecture: "x86_64",
                  vcpus: 16,
                  memory_mib: 65_536,
                  disk_free_gib: 812,
                  podman_version: "5.4.0",
                  kernel: "6.8.0-45-generic",
                  hostname: "mercury",
                },
                last_seen_unix: 1_790_000_600,
                created_at_unix: 1_789_991_000,
              },
            ]),
            { status: 200, headers: { "content-type": "application/json" } },
          ),
        );
      }
      if (url.pathname === "/v1/providers") {
        return Promise.resolve(
          new Response(
            JSON.stringify([
              {
                id: "5a2c8e11-9b3d-4f60-8a71-2c4e6d8b0f39",
                kind: "host",
                label: "mercury",
                linked_at_unix: 1_789_991_000,
                // The account *is* the machine above, which is how the page
                // knows this row is already on screen as a host card rather
                // than owed a cloud one.
                host_id: "8d1a6f30-4b7c-4e21-b0f5-9c2d6a7e4b11",
              },
            ]),
            { status: 200, headers: { "content-type": "application/json" } },
          ),
        );
      }
      return base?.(input, init) ?? Promise.reject(new Error("no fixture"));
    });

    const { findByText, getByText, getByRole } = renderAt("/settings/compute");

    expect(await findByText("mercury")).toBeInTheDocument();
    expect(getByText("Online")).toBeInTheDocument();
    expect(getByText("your hardware")).toBeInTheDocument();
    expect(getByRole("button", { name: "Rename" })).toBeInTheDocument();
    expect(getByRole("button", { name: "Remove" })).toBeInTheDocument();
  });

  it("renders /settings/tools with the skill drop zone", async () => {
    const { findByRole, getByRole } = renderAt("/settings/tools");
    expect(
      await findByRole("heading", { level: 2, name: "Tools" }),
    ).toBeInTheDocument();
    expect(
      getByRole("group", { name: "Which harness gets the skill" }),
    ).toBeInTheDocument();
  });

  it("renders /settings/instructions with the AGENTS.md editor", async () => {
    const { findByRole, findByLabelText } = renderAt("/settings/instructions");
    expect(
      await findByRole("heading", { level: 2, name: "Instructions" }),
    ).toBeInTheDocument();
    expect(await findByLabelText("Content")).toBeInTheDocument();
  });

  it("renders /settings/account, handling an unconfigured VAPID key calmly", async () => {
    const { findByRole, findByText, getByRole } = renderAt("/settings/account");
    expect(
      await findByRole("heading", { level: 2, name: "Account" }),
    ).toBeInTheDocument();
    expect(getByRole("group", { name: "Theme" })).toBeInTheDocument();
    expect(getByRole("button", { name: "Sign out" })).toBeInTheDocument();
    // Push is unavailable in jsdom and the VAPID key is unconfigured; the
    // card says so instead of offering a button that cannot work.
    expect(await findByText("Unsupported here")).toBeInTheDocument();
  });

  it("renders an unknown path as the 404 page", () => {
    const { getByText } = renderAt("/nope");
    expect(getByText("404")).toBeInTheDocument();
  });
});
