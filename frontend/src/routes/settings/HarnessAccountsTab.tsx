import { For, Show, createResource, createSignal } from "solid-js";
import ProblemNotice from "../../components/ProblemNotice";
import { listHarnessAccounts, unlinkHarnessAccount, type HarnessKind } from "../../api/client";
import styles from "../../components/Panel.module.css";

const HARNESS_LABEL: Record<HarnessKind, string> = {
  claude_code: "Claude Code",
  codex: "Codex",
};

function formatUnix(seconds: number): string {
  return new Date(seconds * 1000).toLocaleString();
}

export default function HarnessAccountsTab() {
  const [accounts, { refetch }] = createResource(listHarnessAccounts);
  const [unlinkError, setUnlinkError] = createSignal<unknown>(null);

  async function onUnlink(harness: HarnessKind): Promise<void> {
    setUnlinkError(null);
    try {
      await unlinkHarnessAccount(harness);
      await refetch();
    } catch (err) {
      setUnlinkError(err);
    }
  }

  return (
    <div class={styles.tab}>
      <div class={styles.tabHeader}>
        <h2>Harness accounts</h2>
        <p class={styles.tabDescription}>
          Claude Code and Codex accounts linked against the vendor's own authorization page —
          flyco never sees a password, only the resulting sealed token, provisioned onto a
          session machine when needed.
        </p>
      </div>

      <div class={styles.form}>
        <p class={styles.tabDescription}>
          Linking a new account needs a round trip through the vendor's OAuth consent page, which
          this build doesn't have credentials to start yet. Existing linked accounts still work,
          and unlinking one below is fully supported — new links are on the way.
        </p>
        <button type="button" class={styles.primaryButton} disabled title="Not built yet">
          Link a new account
        </button>
      </div>

      <ProblemNotice error={accounts.error ?? unlinkError()} />
      <Show when={!accounts.loading}>
        <Show
          when={accounts.error !== undefined || (accounts() ?? []).length > 0}
          fallback={<p class={styles.empty}>No harness accounts linked yet.</p>}
        >
          <ul class={styles.list}>
            <For each={accounts()}>
              {(account) => (
                <li class={styles.listItem}>
                  <div>
                    <strong>{account.label}</strong>
                    <p class={styles.itemDetail}>
                      {HARNESS_LABEL[account.harness]} · linked {formatUnix(account.linked_at_unix)}
                      {account.expires_at_unix !== null && account.expires_at_unix !== undefined
                        ? ` · expires ${formatUnix(account.expires_at_unix)}`
                        : ""}
                    </p>
                  </div>
                  <div class={styles.itemActions}>
                    <button type="button" class={styles.dangerButton} onClick={() => void onUnlink(account.harness)}>
                      Unlink
                    </button>
                  </div>
                </li>
              )}
            </For>
          </ul>
        </Show>
      </Show>
    </div>
  );
}
