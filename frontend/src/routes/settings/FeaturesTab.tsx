import { For, createResource } from "solid-js";
import ProblemNotice from "../../components/ProblemNotice";
import { listHarnessFeatures, type Availability, type Feature } from "../../api/client";
import styles from "../../components/Panel.module.css";

const FEATURE_LABEL: Record<Feature, string> = {
  usage_display: "Usage display",
  context_window_display: "Context window display",
  goal_mode: "Goal mode",
  auto_mode: "Auto mode",
  side_chat: "Side chat (btw)",
  dynamic_workflows: "Ultra mode / dynamic workflows",
  settings: "Any setting",
  compact: "Compact",
  advisor: "Advisor",
  monitor: "Monitor",
  background_tasks: "Background tasks",
  auto_continue_at_usage_limit: "Auto continue at usage reset",
  remote_control: "Remote control",
  resume: "Resume (harness-native)",
  skills: "Skills",
  mcp: "MCP",
  memory: "Memory",
  browser_control: "Browser control",
  computer_control: "Computer control",
};

const AVAILABILITY_LABEL: Record<Availability, string> = {
  supported: "supported",
  harness_limitation: "harness-limitation",
  planned: "planned",
  disabled: "disabled",
  takeover: "takeover",
  phase2: "phase 2",
  not_applicable: "—",
};

export default function FeaturesTab() {
  const [rows] = createResource(listHarnessFeatures);

  return (
    <div class={styles.tab}>
      <div class={styles.tabHeader}>
        <h2>Harness features</h2>
        <p class={styles.tabDescription}>
          What flyco actually offers on each official harness. Vendor releases create tracked gaps
          here rather than broken promises.
        </p>
      </div>
      <ProblemNotice error={rows.error} />
      <table class={styles.table}>
        <thead>
          <tr>
            <th>Feature</th>
            <th>Claude Code</th>
            <th>Codex</th>
          </tr>
        </thead>
        <tbody>
          <For each={rows()}>
            {(row) => (
              <tr>
                <th scope="row">{FEATURE_LABEL[row.feature]}</th>
                <td data-availability={row.claude_code}>{AVAILABILITY_LABEL[row.claude_code]}</td>
                <td data-availability={row.codex}>{AVAILABILITY_LABEL[row.codex]}</td>
              </tr>
            )}
          </For>
        </tbody>
      </table>
    </div>
  );
}
