import styles from "./Tab.module.css";

export default function CloudProvidersTab() {
  return (
    <div class={styles.tab}>
      <div class={styles.tabHeader}>
        <h2>Cloud providers</h2>
        <p class={styles.tabDescription}>
          Connect AWS, Google Cloud, or Azure, or add your own machine over SSH. Spot capacity is
          used by default to save on cost; each provider can also be switched off spot
          individually.
        </p>
      </div>
      <p class={styles.empty}>No cloud providers connected yet.</p>
    </div>
  );
}
