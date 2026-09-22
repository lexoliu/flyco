import { describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@solidjs/testing-library";
import {
  MemoryRouter,
  Navigate,
  Route,
  createMemoryHistory,
} from "@solidjs/router";
import AppShell from "../components/AppShell";
import Login from "../routes/Login";
import AuthComplete from "../routes/AuthComplete";
import CliAuthorize from "../routes/CliAuthorize";
import Home from "../routes/Home";
import Welcome from "../routes/Welcome";
import { ConnectCompute, ConnectHarness } from "../routes/connect/Connect";
import ConnectReturn from "../routes/connect/Return";
import SessionDetail from "../routes/SessionDetail";
import SettingsLayout from "../routes/settings/SettingsLayout";
import AgentsSection from "../routes/settings/AgentsSection";
import ApiKeysSection from "../routes/settings/ApiKeysSection";
import PreferencesSection from "../routes/settings/PreferencesSection";
import ComputeSection from "../routes/settings/ComputeSection";
import ToolsSection from "../routes/settings/ToolsSection";
import McpCatalog from "../routes/settings/McpCatalog";
import SkillCatalog from "../routes/settings/SkillCatalog";
import InstructionsSection from "../routes/settings/InstructionsSection";
import AccountSection from "../routes/settings/AccountSection";
import NotFound from "../routes/NotFound";
import { HOST_ENROLL_COMMAND } from "./setup";
import { command, route, type } from "../components/flow/testSupport";
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
      <Route path="/cli/authorize" component={CliAuthorize} />
      <Route path="/" component={Home} />
      <Route path="/welcome" component={Welcome} />
      <Route path="/connect/harness" component={ConnectHarness} />
      <Route path="/connect/compute" component={ConnectCompute} />
      <Route path="/connect/return" component={ConnectReturn} />
      <Route path="/sessions/:id" component={SessionDetail} />
      <Route path="/settings" component={SettingsLayout}>
        <Route
          path="/"
          component={() => <Navigate href="/settings/agents" />}
        />
        <Route path="/agents" component={AgentsSection} />
        <Route path="/compute" component={ComputeSection} />
        <Route path="/tools" component={ToolsSection} />
        <Route path="/tools/mcp-catalog" component={McpCatalog} />
        <Route path="/tools/skill-catalog" component={SkillCatalog} />
        <Route path="/instructions" component={InstructionsSection} />
        <Route path="/api-keys" component={ApiKeysSection} />
        <Route path="/preferences" component={PreferencesSection} />
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
    // The one call allowed before anyone signs in is the public config
    // read that hands the login page its Turnstile sitekey.
    expect(vi.mocked(fetch).mock.calls.map(([input]) => String(input))).toEqual(
      [expect.stringContaining("/v1/config")],
    );
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

  it("sends a signed-out /cli/authorize visit through sign-in, and back", async () => {
    const { findByText } = renderAt("/cli/authorize?id=attempt-1", false);
    expect(await findByText("Sign in with GitHub")).toBeInTheDocument();
    // Approving a terminal belongs to an account, so the attempt id rides
    // the returnTo the shell builds, and sign-in lands back on the ask.
    // The one call the sign-in page makes is the public config read.
    expect(vi.mocked(fetch).mock.calls.map(([input]) => String(input))).toEqual(
      [expect.stringContaining("/v1/config")],
    );
    expect(consumePostLoginPath()).toBe("/cli/authorize?id=attempt-1");
  });

  it("asks on /cli/authorize, and approving posts the decision", async () => {
    route(
      (path, method) =>
        method === "POST" && path === "/v1/cli-sessions/attempt-1/approve",
      () => new Response(null, { status: 204 }),
    );
    const { findByRole, getByRole, findByText } = renderAt(
      "/cli/authorize?id=attempt-1",
    );
    await findByRole("heading", { level: 1, name: "Sign in the flyco CLI?" });

    fireEvent.click(getByRole("button", { name: "Approve sign-in" }));

    expect(await findByText(/Signed in/)).toBeInTheDocument();
    expect(
      vi.mocked(fetch).mock.calls.some(
        ([input, init]) =>
          String(input).endsWith("/v1/cli-sessions/attempt-1/approve") &&
          init?.method === "POST",
      ),
    ).toBe(true);
  });

  it("denies on /cli/authorize, posting the refusal", async () => {
    route(
      (path, method) =>
        method === "POST" && path === "/v1/cli-sessions/attempt-1/deny",
      () => new Response(null, { status: 204 }),
    );
    const { findByRole, getByRole, findByText } = renderAt(
      "/cli/authorize?id=attempt-1",
    );
    await findByRole("heading", { level: 1, name: "Sign in the flyco CLI?" });

    fireEvent.click(getByRole("button", { name: "Deny" }));

    expect(await findByText(/sign-in was refused/)).toBeInTheDocument();
    expect(
      vi.mocked(fetch).mock.calls.some(
        ([input, init]) =>
          String(input).endsWith("/v1/cli-sessions/attempt-1/deny") &&
          init?.method === "POST",
      ),
    ).toBe(true);
  });

  it("renders /cli/authorize without the session rail", async () => {
    const { findByRole, queryByRole } = renderAt("/cli/authorize?id=attempt-1");
    await findByRole("heading", { level: 1, name: "Sign in the flyco CLI?" });

    // A signed-in visitor approving a terminal is not yet inside the
    // product the rail navigates — it must not be there.
    expect(
      queryByRole("button", { name: "Open navigation" }),
    ).not.toBeInTheDocument();
    expect(queryByRole("navigation")).not.toBeInTheDocument();
  });

  it("says so on /cli/authorize when the URL names no attempt", async () => {
    const { findByRole, queryByRole } = renderAt("/cli/authorize");
    expect(await findByRole("alert")).toBeInTheDocument();
    // With nothing to name, neither answer exists to give.
    expect(
      queryByRole("button", { name: "Approve sign-in" }),
    ).not.toBeInTheDocument();
  });

  it("surfaces a refused approval rather than claiming it", async () => {
    route(
      (path, method) =>
        method === "POST" && path === "/v1/cli-sessions/attempt-1/approve",
      () =>
        new Response(
          JSON.stringify({
            type: "https://flyco.dev/problems/cli-session-gone",
            title: "Gone",
            status: 410,
            detail: "The sign-in attempt has expired.",
          }),
          {
            status: 410,
            headers: { "content-type": "application/problem+json" },
          },
        ),
    );
    const { findByRole, getByRole, findByText } = renderAt(
      "/cli/authorize?id=attempt-1",
    );
    await findByRole("heading", { level: 1, name: "Sign in the flyco CLI?" });

    fireEvent.click(getByRole("button", { name: "Approve sign-in" }));

    expect(await findByText("The sign-in attempt has expired.")).toBeInTheDocument();
    // The ask stays up: the refusal is a fact about the attempt, not a
    // reason to pretend the approval landed.
    expect(
      getByRole("heading", { level: 1, name: "Sign in the flyco CLI?" }),
    ).toBeInTheDocument();
  });

  it("renders / as the composer, with the session list in the rail beside it", async () => {
    const { getByLabelText, findByText } = renderAt("/");
    expect(getByLabelText("Describe a task")).toBeInTheDocument();
    // The list is the rail's, not the page's: home asks one question, and
    // repeating the list under it would be the same list one navigation
    // further from where it is used.
    expect(getByLabelText("Search sessions by title or repository")).toBeInTheDocument();
    expect(await findByText("No sessions yet.")).toBeInTheDocument();
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

  it("sends a signed-out visit to /welcome through sign-in, and back", async () => {
    const { findByText } = renderAt("/welcome", false);
    expect(await findByText("Sign in with GitHub")).toBeInTheDocument();
    // The first run belongs to an account: nothing of it was drawn, no
    // protected call went out — the sign-in page's public config read is
    // the one exception — and sign-in returns here.
    expect(vi.mocked(fetch).mock.calls.map(([input]) => String(input))).toEqual(
      [expect.stringContaining("/v1/config")],
    );
    expect(consumePostLoginPath()).toBe("/welcome");
  });

  it("paints nothing on /welcome until readiness has been read", async () => {
    vi.mocked(fetch).mockImplementation(() => new Promise(() => undefined));
    const { queryByRole } = renderAt("/welcome");
    await new Promise((resolve) => setTimeout(resolve, 50));
    // Not the first page: a flow that started now would start from
    // "nothing is linked" and never hear otherwise.
    expect(queryByRole("heading", { level: 1 })).not.toBeInTheDocument();
  });

  it("shows on /welcome what is linked already, as a refresh mid-flow finds it", async () => {
    route(
      (path, method) => method === "GET" && path === "/v1/harness-accounts",
      () =>
        new Response(
          JSON.stringify([
            {
              id: "harness-2",
              harness: "claude_code",
              label: "me@lexo.cool",
              linked_at_unix: 1_787_000_000,
              expires_at_unix: 1_787_028_800,
              models: [],
              usage: [],
            },
          ]),
          { status: 200, headers: { "content-type": "application/json" } },
        ),
    );
    const { findByRole, findByText, getByRole } = renderAt("/welcome");
    await findByRole("heading", { level: 1, name: "Meet flyco" });
    getByRole("button", { name: "Next" }).click();
    await findByRole("heading", { level: 1, name: "Link the agents you use" });
    expect(await findByText("Linked · me@lexo.cool")).toBeInTheDocument();
    expect(getByRole("button", { name: "Next" })).toBeEnabled();
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

  it("tells the tab a vendor sent back that flyco carries on elsewhere", async () => {
    const { findByRole, getByText, queryByRole } = renderAt(
      "/connect/return?provider=azure",
    );
    expect(
      await findByRole("heading", { level: 1, name: "Signed in with Azure" }),
    ).toBeInTheDocument();
    expect(getByText(/You can close this tab/)).toBeInTheDocument();
    // Nothing to press: this tab is not where the flow is.
    expect(queryByRole("button")).not.toBeInTheDocument();
  });

  it("explains a refused consent and carries one action back into the flow", async () => {
    const { findByRole, getByText, getByRole } = renderAt(
      "/connect/return?provider=gcp&problem=google-rejected&reason=access_denied%3A%20denied",
    );
    expect(
      await findByRole("heading", {
        level: 1,
        name: "Google Cloud did not complete the sign-in",
      }),
    ).toBeInTheDocument();
    expect(getByText(/Cloud Shell needs no approval/)).toBeInTheDocument();
    expect(getByText("access_denied: denied")).toBeInTheDocument();
    // The one action: back to the compute stage to pick another road.
    fireEvent.click(getByRole("button", { name: "Try another way" }));
    expect(
      await findByRole("heading", {
        level: 1,
        name: "Where should sessions run?",
      }),
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
    expect(await findByText(command(HOST_ENROLL_COMMAND))).toBeInTheDocument();
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

  it("holds the timeline's place over a prompt the queue has not answered yet", async () => {
    // The prompt reaches the room before the queue reaches the job, so
    // the first thing a new session's transcript holds is the user's own
    // message and nothing about the machine. The build is still the story
    // of that page, and it must not go blank between the two.
    const base = vi.mocked(fetch).getMockImplementation();
    vi.mocked(fetch).mockImplementation(async (input, init) => {
      const url = new URL(String(input instanceof Request ? input.url : input));
      if (!/^\/v1\/sessions\/[^/]+\/events$/.test(url.pathname)) {
        return base!(input, init);
      }
      return new Response(
        JSON.stringify({
          events: [
            {
              seq: 1,
              at_unix: 1_800_000_000,
              event: { type: "user_message", text: "Audit the relay for dropped frames" },
            },
          ],
          more: false,
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    });
    const { findByText, findByLabelText, queryByText } = renderAt("/sessions/abc-123");

    expect(await findByLabelText("Provisioning")).toBeInTheDocument();
    expect(await findByText("Reserving a machine on AWS")).toBeInTheDocument();
    expect(
      queryByText("Your task is queued and will start as soon as the machine is ready."),
    ).not.toBeInTheDocument();
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
    // to be on is a bill nobody is being sent (issue #135). The stream says
    // the session moved; the page asks the control plane what it moved to.
    //
    // The user stream is a module singleton shared across the whole file,
    // so it is driven by standing the `GET /v1/events` fetch up as an
    // enqueueable SSE body rather than by stubbing a socket class.
    const base = vi.mocked(fetch).getMockImplementation();
    const encoder = new TextEncoder();
    const feeds: ReadableStreamDefaultController<Uint8Array>[] = [];
    let reads = 0;
    vi.mocked(fetch).mockImplementation(async (input, init) => {
      const url = new URL(String(input instanceof Request ? input.url : input));
      if (url.pathname === "/v1/events") {
        return Promise.resolve(
          new Response(
            new ReadableStream<Uint8Array>({
              start(controller) {
                feeds.push(controller);
              },
            }),
            { status: 200, headers: { "content-type": "text/event-stream" } },
          ),
        );
      }
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

    await vi.waitFor(() => expect(feeds.length).toBeGreaterThan(0));
    const frame = `id: 1\ndata: ${JSON.stringify({
      session: "abc-123",
      seq: null,
      event: {
        type: "machine_changed",
        machine_type: "m7g.xlarge",
        hourly: 163_200,
        spot: true,
        restarted: true,
      },
    })}\n\n`;
    for (const feed of feeds) {
      feed.enqueue(encoder.encode(frame));
    }

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

    const { findByRole, findByText, getByRole, queryByLabelText } =
      renderAt("/sessions/abc-123");

    const state = await findByRole("region", { name: "Session state" });
    expect(state.textContent).toContain("Failed");
    expect(state.textContent).toContain("AWS refused the reservation");
    // And it stands where the composer would be, rather than above a box
    // that would refuse every message typed into it (issue #133).
    expect(queryByLabelText("Message the agent")).not.toBeInTheDocument();

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

    const { findByRole, getByRole, queryByLabelText } = renderAt("/sessions/abc-123");

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
    // The notice is the whole slot: a paused session has nowhere to type
    // until the budget is raised, and a box that says so is a box that
    // should not be there.
    expect(queryByLabelText("Message the agent")).not.toBeInTheDocument();
  });

  it("covers a suspended session's composer with the notice that resumes it", async () => {
    // The machine is off, and the one thing to do about it is on the
    // notice. The box stays on the page — it is where typing resumes —
    // but it is inert under the notice until the machine is back.
    const base = vi.mocked(fetch).getMockImplementation();
    vi.mocked(fetch).mockImplementation(async (input, init) => {
      const url = new URL(String(input instanceof Request ? input.url : input));
      const method = (init?.method ?? "GET").toUpperCase();
      if (method !== "GET") {
        return base!(input, init);
      }
      if (/^\/v1\/sessions\/[^/]+$/.test(url.pathname)) {
        const response = await base!(input, init);
        const session = (await response.json()) as Record<string, unknown>;
        return new Response(
          JSON.stringify({
            ...session,
            state: "interrupted",
            activity: "idle",
            interrupted_reason: "suspended",
          }),
          { status: 200, headers: { "content-type": "application/json" } },
        );
      }
      if (/^\/v1\/sessions\/[^/]+\/machine$/.test(url.pathname)) {
        const response = await base!(input, init);
        const machine = (await response.json()) as Record<string, unknown>;
        return new Response(JSON.stringify({ ...machine, state: "deallocated" }), {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      }
      return base!(input, init);
    });

    const { findByRole, getByLabelText, getByRole } = renderAt("/sessions/abc-123");

    const state = await findByRole("region", { name: "Session state" });
    expect(state.textContent).toContain("Interrupted · suspended");
    expect(state.textContent).toContain("Its disk is kept");
    expect(getByRole("button", { name: "Resume" })).toBeInTheDocument();

    const field = getByLabelText("Message the agent");
    expect(field.closest("[inert]")).not.toBeNull();
  });

  it("refuses a shell command while the suspended session's machine is off", async () => {
    // A `!` is delivered or nothing — it cannot be held the way a prompt
    // is, so the composer says so rather than sending it to die (§9.9).
    const base = vi.mocked(fetch).getMockImplementation();
    vi.mocked(fetch).mockImplementation(async (input, init) => {
      const url = new URL(String(input instanceof Request ? input.url : input));
      const method = (init?.method ?? "GET").toUpperCase();
      if (method !== "GET") {
        return base!(input, init);
      }
      if (/^\/v1\/sessions\/[^/]+$/.test(url.pathname)) {
        const response = await base!(input, init);
        const session = (await response.json()) as Record<string, unknown>;
        return new Response(
          JSON.stringify({
            ...session,
            state: "interrupted",
            activity: "idle",
            interrupted_reason: "suspended",
          }),
          { status: 200, headers: { "content-type": "application/json" } },
        );
      }
      if (/^\/v1\/sessions\/[^/]+\/machine$/.test(url.pathname)) {
        const response = await base!(input, init);
        const machine = (await response.json()) as Record<string, unknown>;
        return new Response(JSON.stringify({ ...machine, state: "deallocated" }), {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      }
      return base!(input, init);
    });

    const { findByRole, findByText, getByLabelText } = renderAt("/sessions/abc-123");
    await findByRole("region", { name: "Session state" });

    const field = getByLabelText("Message the agent") as HTMLTextAreaElement;
    type(field, "!cargo test");
    expect(
      await findByText("The machine is not connected — there is no bash to run it."),
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
              models: [],
              usage: [],
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
    const { findByRole, getAllByText } = renderAt("/settings/agents");
    expect(
      await findByRole("heading", { level: 2, name: "Agents" }),
    ).toBeInTheDocument();
    expect(getAllByText("Claude Code").length).toBeGreaterThan(0);
    expect(getAllByText("Codex").length).toBeGreaterThan(0);
    expect(getAllByText("Not linked")).toHaveLength(3);
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

  it("renders /settings/tools/mcp-catalog with the registry's servers", async () => {
    const { findByRole, getByRole } = renderAt("/settings/tools/mcp-catalog");
    expect(
      await findByRole("heading", { level: 2, name: "Add an MCP server" }),
    ).toBeInTheDocument();
    expect(getByRole("searchbox", { name: "Search the registry" })).toBeInTheDocument();
    expect(await findByRole("button", { name: /DeepWiki/ })).toBeInTheDocument();
    // One way back, above the title, and no primary until a server is chosen.
    expect(getByRole("link", { name: "Back to Tools" })).toHaveAttribute("href", "/settings/tools");
  });

  it("renders /settings/tools/skill-catalog with the marketplace's skills", async () => {
    const { findByRole, getByRole } = renderAt("/settings/tools/skill-catalog");
    expect(await findByRole("heading", { level: 2, name: "Add a skill" })).toBeInTheDocument();
    expect(getByRole("searchbox", { name: "Search skills" })).toBeInTheDocument();
    expect(await findByRole("button", { name: /xlsx/ })).toBeInTheDocument();
    // The marketplace it came from is listed, and the built-in one cannot be
    // removed, so the row carries no Remove button.
    expect(await findByRole("list", { name: "Marketplaces" })).toBeInTheDocument();
    expect(getByRole("link", { name: "Back to Tools" })).toHaveAttribute("href", "/settings/tools");
  });

  it("renders /settings/instructions with the AGENTS.md editor", async () => {
    const { findByRole, findByLabelText } = renderAt("/settings/instructions");
    expect(
      await findByRole("heading", { level: 2, name: "Instructions" }),
    ).toBeInTheDocument();
    // Labelled "AGENTS.md", not "Content": the box's own shape is not a
    // name, and the group heading already said what document this is.
    expect(await findByLabelText("AGENTS.md")).toBeInTheDocument();
  });

  it("renders /settings/account as the identity and the way out", async () => {
    const { findByRole, getByRole } = renderAt("/settings/account");
    expect(
      await findByRole("heading", { level: 2, name: "Account" }),
    ).toBeInTheDocument();
    expect(getByRole("button", { name: "Sign out" })).toBeInTheDocument();
  });

  it("renders /settings/preferences, handling an unconfigured VAPID key calmly", async () => {
    const { findByRole, findByText, getByRole } = renderAt("/settings/preferences");
    expect(
      await findByRole("heading", { level: 2, name: "Preferences" }),
    ).toBeInTheDocument();
    expect(getByRole("group", { name: "Theme" })).toBeInTheDocument();
    expect(getByRole("group", { name: "Interface size" })).toBeInTheDocument();
    expect(getByRole("group", { name: "Transcript font" })).toBeInTheDocument();
    // Push is unavailable in jsdom and the VAPID key is unconfigured; the
    // card says so instead of offering a button that cannot work.
    expect(await findByText("Unsupported here")).toBeInTheDocument();
  });

  it("renders /settings/api-keys as its own section", async () => {
    const { findByRole, getByRole } = renderAt("/settings/api-keys");
    expect(
      await findByRole("heading", { level: 2, name: "API keys" }),
    ).toBeInTheDocument();
    expect(getByRole("button", { name: /Create key/ })).toBeInTheDocument();
  });

  it("renders an unknown path as the 404 page", () => {
    const { getByText } = renderAt("/nope");
    expect(getByText("404")).toBeInTheDocument();
  });
});
