/**
 * Settings: five sections down the left, cards on the right.
 *
 * The nine tabs this replaces were a map of the database — one tab per
 * table — and asked the user to know that "harness accounts" and "harness
 * features" were different things before they could find either. The five
 * sections of docs/ux.md §10 are named after what the user came to change:
 * the agent, the computer, the tools, the instructions, the account.
 *
 * On a narrow viewport the rail becomes a horizontal row that scrolls, so
 * the section names stay visible rather than collapsing into a menu the
 * user has to open to find out where they are.
 */
import { type JSX, For } from "solid-js";
import { A } from "@solidjs/router";
import { Blocks, Bot, CircleUser, FileText, Server } from "lucide-solid";
import styles from "./Settings.module.css";

interface SettingsSection {
  href: string;
  label: string;
  icon: typeof Bot;
}

export const SETTINGS_SECTIONS: readonly SettingsSection[] = [
  { href: "/settings/agents", label: "Agents", icon: Bot },
  { href: "/settings/compute", label: "Compute", icon: Server },
  { href: "/settings/tools", label: "Tools", icon: Blocks },
  { href: "/settings/instructions", label: "Instructions", icon: FileText },
  { href: "/settings/account", label: "Account", icon: CircleUser },
];

export default function SettingsLayout(props: { children?: JSX.Element }) {
  return (
    <div class={styles.page}>
      {/* The top bar already says Settings; repeating it in 28px type would
          push the section the user asked for below the fold on a laptop. */}
      <h1 class="visually-hidden">Settings</h1>
      <nav class={styles.rail} aria-label="Settings sections">
        <For each={SETTINGS_SECTIONS}>
          {(section) => (
            <A href={section.href} class={styles.railLink} activeClass={styles.railLinkActive}>
              <section.icon size={15} aria-hidden="true" />
              {section.label}
            </A>
          )}
        </For>
      </nav>
      <div class={styles.content}>{props.children}</div>
    </div>
  );
}
