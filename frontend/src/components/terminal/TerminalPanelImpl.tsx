import styles from "./TerminalPanel.module.css";

/**
 * Placeholder for the session terminal. The real implementation will
 * attach xterm.js to the daemon's terminal stream; it isn't pulled in yet,
 * so this stays a plain status panel until that lands.
 */
export default function TerminalPanelImpl() {
  return (
    <div class={styles.placeholder} role="status">
      <p>Terminal is not connected yet.</p>
      <p class={styles.hint}>
        This pane will attach to the session's fish shell once the terminal transport lands.
      </p>
    </div>
  );
}
