import { createSignal, onMount } from "solid-js";
import { type ThemePreference, readStoredThemePreference, setTheme } from "../lib/theme";
import styles from "./ThemeToggle.module.css";

const ORDER: readonly ThemePreference[] = ["system", "light", "dark"];
const LABEL: Record<ThemePreference, string> = {
  system: "System",
  light: "Light",
  dark: "Dark",
};

function nextPreference(current: ThemePreference): ThemePreference {
  const index = ORDER.indexOf(current);
  const next = ORDER[(index + 1) % ORDER.length];
  if (next === undefined) {
    throw new Error("Theme preference cycle is empty");
  }
  return next;
}

export default function ThemeToggle() {
  const [preference, setPreference] = createSignal<ThemePreference>("system");

  onMount(() => {
    setPreference(readStoredThemePreference());
  });

  function cycle(): void {
    const next = nextPreference(preference());
    setTheme(next);
    setPreference(next);
  }

  return (
    <button
      type="button"
      class={styles.toggle}
      onClick={cycle}
      aria-label={`Theme: ${LABEL[preference()]}. Click to change.`}
    >
      {LABEL[preference()]}
    </button>
  );
}
