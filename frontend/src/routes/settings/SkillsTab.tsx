import styles from "./Tab.module.css";

export default function SkillsTab() {
  return (
    <div class={styles.tab}>
      <div class={styles.tabHeader}>
        <h2>Skills</h2>
        <p class={styles.tabDescription}>
          Global skills are read-only to agents. To change one, an agent uploads a zip through
          its MCP tool instead of editing files directly, and the update reaches every session
          immediately. Claude Code and Codex keep separate skill directories.
        </p>
      </div>
      <p class={styles.empty}>No skills uploaded yet.</p>
    </div>
  );
}
