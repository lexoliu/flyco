import { describe, expect, it } from "vitest";
import { render } from "@solidjs/testing-library";
import { MemoryRouter, Navigate, Route, createMemoryHistory } from "@solidjs/router";
import AppShell from "../components/AppShell";
import Login from "../routes/Login";
import AuthComplete from "../routes/AuthComplete";
import Sessions from "../routes/Sessions";
import SessionDetail from "../routes/SessionDetail";
import SettingsLayout from "../routes/settings/SettingsLayout";
import McpServersTab from "../routes/settings/McpServersTab";
import SkillsTab from "../routes/settings/SkillsTab";
import CloudProvidersTab from "../routes/settings/CloudProvidersTab";
import ApiKeysTab from "../routes/settings/ApiKeysTab";
import EnvTab from "../routes/settings/EnvTab";
import NotFound from "../routes/NotFound";

/**
 * Renders the same route tree as src/main.tsx, starting at a given path.
 *
 * Uses `MemoryRouter` with a fresh, per-call history instead of the
 * browser-history-backed `Router`: jsdom exposes a single real
 * `window.location` shared by the whole test file, so a `Router url={...}`
 * prop only ever takes effect once per file. A dedicated in-memory history
 * per render keeps each test's navigation fully isolated.
 */
function renderAt(url: string) {
  const history = createMemoryHistory();
  history.set({ value: url, replace: true, scroll: false });
  return render(() => (
    <MemoryRouter history={history} root={AppShell}>
      <Route path="/login" component={Login} />
      <Route path="/auth/complete" component={AuthComplete} />
      <Route path="/" component={Sessions} />
      <Route path="/sessions/:id" component={SessionDetail} />
      <Route path="/settings" component={SettingsLayout}>
        <Route path="/" component={() => <Navigate href="/settings/mcp" />} />
        <Route path="/mcp" component={McpServersTab} />
        <Route path="/skills" component={SkillsTab} />
        <Route path="/providers" component={CloudProvidersTab} />
        <Route path="/api-keys" component={ApiKeysTab} />
        <Route path="/env" component={EnvTab} />
      </Route>
      <Route path="*404" component={NotFound} />
    </MemoryRouter>
  ));
}

describe("route smoke tests", () => {
  it("renders /login with the sign-in call to action", () => {
    const { getByText } = renderAt("/login");
    expect(getByText("Sign in with GitHub")).toBeInTheDocument();
  });

  it("renders /auth/complete as an explicit error state without a token", () => {
    const { getByRole } = renderAt("/auth/complete");
    expect(getByRole("alert")).toBeInTheDocument();
  });

  it("renders / as the sessions list with its empty state", async () => {
    const { getByRole, findByText } = renderAt("/");
    expect(getByRole("heading", { level: 1, name: "Sessions" })).toBeInTheDocument();
    expect(
      await findByText("No sessions yet. Start one to put an agent to work in a repo."),
    ).toBeInTheDocument();
  });

  it("renders /sessions/:id as the session shell", () => {
    const { getByText } = renderAt("/sessions/abc-123");
    expect(getByText("abc-123")).toBeInTheDocument();
  });

  it("renders /settings, redirecting to the MCP tab", async () => {
    const { findByRole } = renderAt("/settings");
    expect(
      await findByRole("heading", { level: 2, name: "MCP servers" }),
    ).toBeInTheDocument();
  });

  it("renders /settings/env", () => {
    const { getByRole } = renderAt("/settings/env");
    expect(getByRole("heading", { level: 2, name: ".env" })).toBeInTheDocument();
  });

  it("renders an unknown path as the 404 page", () => {
    const { getByText } = renderAt("/nope");
    expect(getByText("404")).toBeInTheDocument();
  });
});
