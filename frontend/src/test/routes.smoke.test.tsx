import { describe, expect, it, vi } from "vitest";
import { render } from "@solidjs/testing-library";
import { MemoryRouter, Navigate, Route, createMemoryHistory } from "@solidjs/router";
import AppShell from "../components/AppShell";
import Login from "../routes/Login";
import AuthComplete from "../routes/AuthComplete";
import Home from "../routes/Home";
import Welcome from "../routes/Welcome";
import ConnectHarness from "../routes/connect/ConnectHarness";
import ConnectCompute from "../routes/connect/ConnectCompute";
import SessionDetail from "../routes/SessionDetail";
import SettingsLayout from "../routes/settings/SettingsLayout";
import AgentsSection from "../routes/settings/AgentsSection";
import ComputeSection from "../routes/settings/ComputeSection";
import ToolsSection from "../routes/settings/ToolsSection";
import InstructionsSection from "../routes/settings/InstructionsSection";
import AccountSection from "../routes/settings/AccountSection";
import NotFound from "../routes/NotFound";
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
        <Route path="/" component={() => <Navigate href="/settings/agents" />} />
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
    expect(queryByRole("heading", { level: 1 })?.textContent).not.toContain("What should we build");
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
        { status: 401, headers: { "content-type": "application/problem+json" } },
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
    expect(await findByRole("button", { name: "Connect an agent first" })).toBeDisabled();
  });

  it("sends a first visitor with nothing linked to the welcome flow", async () => {
    const { findByRole } = renderAt("/", true, false);
    expect(await findByRole("heading", { level: 1, name: "Meet flyco" })).toBeInTheDocument();
  });

  it("renders /welcome as the three-screen card", async () => {
    const { findByRole, getByRole } = renderAt("/welcome");
    expect(await findByRole("heading", { level: 1, name: "Meet flyco" })).toBeInTheDocument();
    expect(getByRole("button", { name: "Next" })).toBeInTheDocument();
  });

  it("renders /connect/harness as a working link page", async () => {
    const { findByRole } = renderAt("/connect/harness");
    expect(
      await findByRole("heading", { level: 1, name: "Connect an agent" }),
    ).toBeInTheDocument();
  });

  it("renders /connect/compute as a working link page", async () => {
    const { findByRole } = renderAt("/connect/compute");
    expect(await findByRole("heading", { level: 1, name: "Connect compute" })).toBeInTheDocument();
  });

  it("renders /sessions/:id as a header, a transcript and a composer", async () => {
    const { findByText, getByLabelText } = renderAt("/sessions/abc-123");

    // The title, not the id: a session is identified by what it is for.
    expect(await findByText("Audit the relay for dropped frames")).toBeInTheDocument();
    expect(getByLabelText("Message the agent")).toBeInTheDocument();
    // A session with no events yet says what to do about it rather than
    // showing an empty box.
    expect(
      await findByText("Nothing has happened yet. Send a message to get the agent started."),
    ).toBeInTheDocument();
  });

  it("keeps the session's side panels behind the collapsed drawer", async () => {
    const { findByText, queryByLabelText, getByLabelText } = renderAt("/sessions/abc-123");
    await findByText("Audit the relay for dropped frames");

    // docs/ux.md §9.4: the drawer starts closed, so the transcript gets
    // the width and the terminal is one click away rather than always on.
    expect(queryByLabelText("Session panels")).not.toBeInTheDocument();
    getByLabelText("Show the panel").click();
    expect(getByLabelText("Session panels")).toBeInTheDocument();
  });

  it("renders /settings, redirecting to Agents", async () => {
    const { findByRole } = renderAt("/settings");
    expect(await findByRole("heading", { level: 2, name: "Agents" })).toBeInTheDocument();
  });

  it("offers all five sections in the settings navigation", async () => {
    const { findByRole, getByRole } = renderAt("/settings/agents");
    await findByRole("heading", { level: 2, name: "Agents" });
    for (const label of ["Agents", "Compute", "Tools", "Instructions", "Account"]) {
      expect(getByRole("link", { name: label })).toBeInTheDocument();
    }
  });

  it("renders /settings/agents with a card for each harness", async () => {
    const { findByRole, getAllByText, getByText } = renderAt("/settings/agents");
    expect(await findByRole("heading", { level: 2, name: "Agents" })).toBeInTheDocument();
    // Both harnesses are named twice: once on their card, once in the
    // capability matrix under the disclosure.
    expect(getAllByText("Claude Code").length).toBeGreaterThan(0);
    expect(getAllByText("Codex").length).toBeGreaterThan(0);
    expect(getByText("What works on each harness")).toBeInTheDocument();
    expect(getAllByText("Not linked")).toHaveLength(2);
  });

  it("renders /settings/compute as an empty state with its one action", async () => {
    const { findByRole, getByRole } = renderAt("/settings/compute");
    expect(await findByRole("heading", { level: 2, name: "Compute" })).toBeInTheDocument();
    expect(getByRole("link", { name: "Add compute" })).toBeInTheDocument();
  });

  it("renders /settings/tools with the skill drop zone", async () => {
    const { findByRole, getByRole } = renderAt("/settings/tools");
    expect(await findByRole("heading", { level: 2, name: "Tools" })).toBeInTheDocument();
    expect(getByRole("group", { name: "Which harness gets the skill" })).toBeInTheDocument();
  });

  it("renders /settings/instructions with the AGENTS.md editor", async () => {
    const { findByRole, findByLabelText } = renderAt("/settings/instructions");
    expect(await findByRole("heading", { level: 2, name: "Instructions" })).toBeInTheDocument();
    expect(await findByLabelText("Content")).toBeInTheDocument();
  });

  it("renders /settings/account, handling an unconfigured VAPID key calmly", async () => {
    const { findByRole, findByText, getByRole } = renderAt("/settings/account");
    expect(await findByRole("heading", { level: 2, name: "Account" })).toBeInTheDocument();
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
