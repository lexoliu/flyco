import { render } from "solid-js/web";
import { Navigate, Route, Router } from "@solidjs/router";
import "./styles/global.css";
import { registerSW } from "virtual:pwa-register";
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
import AgentsSection from "./routes/settings/AgentsSection";
import ComputeSection from "./routes/settings/ComputeSection";
import ToolsSection from "./routes/settings/ToolsSection";
import InstructionsSection from "./routes/settings/InstructionsSection";
import AccountSection from "./routes/settings/AccountSection";
import NotFound from "./routes/NotFound";

initTheme();
// Registers the push service worker; with `registerType: "autoUpdate"` a
// new build replaces the old one without asking.
registerSW({ immediate: true });

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
        <Route path="/" component={() => <Navigate href="/settings/agents" />} />
        <Route path="/agents" component={AgentsSection} />
        <Route path="/compute" component={ComputeSection} />
        <Route path="/tools" component={ToolsSection} />
        <Route path="/instructions" component={InstructionsSection} />
        <Route path="/account" component={AccountSection} />
      </Route>
      <Route path="*404" component={NotFound} />
    </Router>
  ),
  root,
);
