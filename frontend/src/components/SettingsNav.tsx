/**
 * The rail, while the user is in settings.
 *
 * Settings used to carry a second vertical nav of its own, 200px to the
 * right of the shell's rail: two columns of links side by side, and the
 * page the user actually opened pushed into what was left. One column is
 * navigation; two are a maze.
 *
 * So the rail becomes the settings nav for as long as the user is in
 * settings, with `Sessions` at the top where `New session` sits everywhere
 * else — the way out is in the same place as the way on.
 */
import { For } from "solid-js";
import { A } from "@solidjs/router";
import { ArrowLeft, Blocks, Bot, CircleUser, FileText, Server } from "lucide-solid";
import styles from "./SettingsNav.module.css";

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

export default function SettingsNav(props: { onNavigate?: () => void }) {
  return (
    <nav class={styles.nav} aria-label="Settings sections">
      <A href="/" end class={styles.back} onClick={() => props.onNavigate?.()}>
        <ArrowLeft size={15} aria-hidden="true" />
        Sessions
      </A>
      <p class={styles.heading}>Settings</p>
      <For each={SETTINGS_SECTIONS}>
        {(section) => (
          <A
            href={section.href}
            class={styles.link}
            activeClass={styles.linkActive}
            onClick={() => props.onNavigate?.()}
          >
            <section.icon size={15} aria-hidden="true" />
            {section.label}
          </A>
        )}
      </For>
    </nav>
  );
}
