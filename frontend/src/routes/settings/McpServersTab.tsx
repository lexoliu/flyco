import styles from "./Tab.module.css";

export default function McpServersTab() {
  return (
    <div class={styles.tab}>
      <div class={styles.tabHeader}>
        <h2>MCP servers</h2>
        <p class={styles.tabDescription}>
          This is the one place your agents' MCP servers are configured. Agents cannot add or
          edit servers themselves — flyco enforces that with a read-only allowlist on every
          session.
        </p>
      </div>
      <p class={styles.empty}>No MCP servers configured yet.</p>
    </div>
  );
}
