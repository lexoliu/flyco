/**
 * What flyco actually offers on each official harness.
 *
 * Kept as its own component because it is reference material, not a
 * setting: it sits under a disclosure in the Agents section (docs/ux.md
 * §10) so that the thing the user came to change — the linked account — is
 * what they see first, and the capability table is one click away when they
 * want to know why something is missing.
 */
import { For } from "solid-js";
import { createQuery } from "../../lib/query";
import ProblemNotice from "../../components/ProblemNotice";
import { listHarnessFeatures, type Availability, type Feature } from "../../api/client";
import styles from "./Settings.module.css";

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

export default function HarnessMatrix() {
  const [rows] = createQuery(listHarnessFeatures);

  return (
    <>
      <ProblemNotice error={rows.error} />
      <table class={styles.matrix}>
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
    </>
  );
}
