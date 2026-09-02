import { render } from "solid-js/web";
import { Navigate, Route, Router } from "@solidjs/router";
import "./styles/global.css";
import { initTheme } from "./lib/theme";
import AppShell from "./components/AppShell";
import Login from "./routes/Login";
import AuthComplete from "./routes/AuthComplete";
import Home from "./routes/Home";
import Welcome from "./routes/Welcome";
import ConnectHarness from "./routes/connect/ConnectHarness";
import ConnectCompute from "./routes/connect/ConnectCompute";
import SessionDetail from "./routes/SessionDetail";
import SettingsLayout from "./routes/settings/SettingsLayout";
import McpServersTab from "./routes/settings/McpServersTab";
import SkillsTab from "./routes/settings/SkillsTab";
import CloudProvidersTab from "./routes/settings/CloudProvidersTab";
import ApiKeysTab from "./routes/settings/ApiKeysTab";
import HarnessAccountsTab from "./routes/settings/HarnessAccountsTab";
import FeaturesTab from "./routes/settings/FeaturesTab";
import MemoryTab from "./routes/settings/MemoryTab";
import AgentsMdTab from "./routes/settings/AgentsMdTab";
import NotificationsTab from "./routes/settings/NotificationsTab";
import NotFound from "./routes/NotFound";

initTheme();

const root = document.getElementById("app");
if (root === null) {
  throw new Error("#app root element is missing from index.html");
}

render(
  () => (
    <Router root={AppShell}>
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
    </Router>
  ),
  root,
);
