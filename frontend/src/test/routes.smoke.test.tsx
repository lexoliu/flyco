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
import McpServersTab from "../routes/settings/McpServersTab";
import SkillsTab from "../routes/settings/SkillsTab";
import CloudProvidersTab from "../routes/settings/CloudProvidersTab";
import ApiKeysTab from "../routes/settings/ApiKeysTab";
import HarnessAccountsTab from "../routes/settings/HarnessAccountsTab";
import FeaturesTab from "../routes/settings/FeaturesTab";
import MemoryTab from "../routes/settings/MemoryTab";
import AgentsMdTab from "../routes/settings/AgentsMdTab";
import NotificationsTab from "../routes/settings/NotificationsTab";
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
        <Route path="/" component={() => <Navigate href="/settings/mcp" />} />
        <Route path="/mcp" component={McpServersTab} />
        <Route path="/skills" component={SkillsTab} />
        <Route path="/providers" component={CloudProvidersTab} />
        <Route path="/harness-accounts" component={HarnessAccountsTab} />
        <Route path="/features" component={FeaturesTab} />
        <Route path="/memory" component={MemoryTab} />
        <Route path="/agents-md" component={AgentsMdTab} />
        <Route path="/notifications" component={NotificationsTab} />
        <Route path="/api-keys" component={ApiKeysTab} />
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

  it("renders /sessions/:id as the session shell", () => {
    const { getByText } = renderAt("/sessions/abc-123");
    expect(getByText("abc-123")).toBeInTheDocument();
    expect(getByText("Compact context")).toBeInTheDocument();
  });

  it("renders /settings, redirecting to the MCP tab", async () => {
    const { findByRole } = renderAt("/settings");
    expect(
      await findByRole("heading", { level: 2, name: "MCP servers" }),
    ).toBeInTheDocument();
  });

  it("renders /settings/harness-accounts", async () => {
    const { findByRole } = renderAt("/settings/harness-accounts");
    expect(await findByRole("heading", { level: 2, name: "Harness accounts" })).toBeInTheDocument();
  });

  it("renders /settings/features", async () => {
    const { findByRole } = renderAt("/settings/features");
    expect(await findByRole("heading", { level: 2, name: "Harness features" })).toBeInTheDocument();
  });

  it("renders /settings/memory", () => {
    const { getByRole } = renderAt("/settings/memory");
    expect(getByRole("heading", { level: 2, name: "Memory" })).toBeInTheDocument();
  });

  it("renders /settings/agents-md", () => {
    const { getByRole } = renderAt("/settings/agents-md");
    expect(getByRole("heading", { level: 2, name: "AGENTS.md" })).toBeInTheDocument();
  });

  it("renders /settings/notifications, handling an unconfigured VAPID key calmly", async () => {
    const { getByRole, findByRole } = renderAt("/settings/notifications");
    expect(getByRole("heading", { level: 2, name: "Notifications" })).toBeInTheDocument();
    expect(await findByRole("status")).toHaveTextContent("Not built yet");
  });

  it("renders an unknown path as the 404 page", () => {
    const { getByText } = renderAt("/nope");
    expect(getByText("404")).toBeInTheDocument();
  });
});
