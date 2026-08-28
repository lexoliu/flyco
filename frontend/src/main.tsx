import { render } from "solid-js/web";
import { Navigate, Route, Router } from "@solidjs/router";
import "./styles/global.css";
import { initTheme } from "./lib/theme";
import AppShell from "./components/AppShell";
import Login from "./routes/Login";
import AuthComplete from "./routes/AuthComplete";
import Sessions from "./routes/Sessions";
import SessionDetail from "./routes/SessionDetail";
import SettingsLayout from "./routes/settings/SettingsLayout";
import McpServersTab from "./routes/settings/McpServersTab";
import SkillsTab from "./routes/settings/SkillsTab";
import CloudProvidersTab from "./routes/settings/CloudProvidersTab";
import ApiKeysTab from "./routes/settings/ApiKeysTab";
import EnvTab from "./routes/settings/EnvTab";
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
    </Router>
  ),
  root,
);
