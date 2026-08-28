import { type JSX, For } from "solid-js";
import { A } from "@solidjs/router";
import styles from "./Settings.module.css";

interface SettingsTab {
  href: string;
  label: string;
}

const TABS: readonly SettingsTab[] = [
  { href: "/settings/mcp", label: "MCP servers" },
  { href: "/settings/skills", label: "Skills" },
  { href: "/settings/providers", label: "Cloud providers" },
  { href: "/settings/api-keys", label: "API keys" },
  { href: "/settings/env", label: ".env" },
];

export default function SettingsLayout(props: { children?: JSX.Element }) {
  return (
    <section class={styles.page}>
      <h1>Settings</h1>
      <nav class={styles.tabs} aria-label="Settings sections">
        <For each={TABS}>
          {(tab) => (
            <A href={tab.href} class={styles.tab} activeClass={styles.tabActive}>
              {tab.label}
            </A>
          )}
        </For>
      </nav>
      <div class={styles.tabPanel}>{props.children}</div>
    </section>
  );
}
