import styles from "./Tab.module.css";

export default function EnvTab() {
  return (
    <div class={styles.tab}>
      <div class={styles.tabHeader}>
        <h2>.env</h2>
        <p class={styles.tabDescription}>
          Agents can read this file but not edit it; you can. Flyco doesn't yet control which
          hosts a session can reach, so secrets placed here may still leak over the network —
          keep that in mind until network control ships.
        </p>
      </div>
      <p class={styles.empty}>No .env file yet.</p>
    </div>
  );
}
