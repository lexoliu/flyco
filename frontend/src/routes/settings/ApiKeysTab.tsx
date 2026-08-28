import styles from "./Tab.module.css";

export default function ApiKeysTab() {
  return (
    <div class={styles.tab}>
      <div class={styles.tabHeader}>
        <h2>API keys</h2>
        <p class={styles.tabDescription}>
          API keys let you use flyco's REST API directly, without the web UI — it's the same API
          this app calls. A key's plaintext is shown exactly once, right after you create it.
        </p>
      </div>
      <p class={styles.empty}>No API keys yet.</p>
    </div>
  );
}
