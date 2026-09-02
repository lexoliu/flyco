import { For, Show, createResource, createSignal } from "solid-js";
import ProblemNotice from "../../components/ProblemNotice";
import HarnessLinkForm from "../../components/link/HarnessLinkForm";
import {
  listHarnessAccounts,
  unlinkHarnessAccount,
  type HarnessAccountView,
  type HarnessKind,
} from "../../api/client";
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

  async function onUnlink(id: HarnessAccountView["id"]): Promise<void> {
    setUnlinkError(null);
    try {
      await unlinkHarnessAccount(id);
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
          Link Claude Code with a subscription setup token or Anthropic API key, or link Codex
          with an OpenAI API key. Flyco never receives a vendor password and stores credentials
          encrypted.
        </p>
      </div>

      <HarnessLinkForm onLinked={refetch} />

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
                    <button
                      type="button"
                      class={styles.dangerButton}
                      onClick={() => void onUnlink(account.id)}
                    >
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
