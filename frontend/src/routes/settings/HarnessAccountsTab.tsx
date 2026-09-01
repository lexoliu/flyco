import { For, Show, createResource, createSignal } from "solid-js";
import ProblemNotice from "../../components/ProblemNotice";
import {
  linkHarnessAccount,
  listHarnessAccounts,
  unlinkHarnessAccount,
  type HarnessAccountView,
  type HarnessCredentialInput,
  type HarnessKind,
} from "../../api/client";
import styles from "../../components/Panel.module.css";

const HARNESS_LABEL: Record<HarnessKind, string> = {
  claude_code: "Claude Code",
  codex: "Codex",
};

type CredentialKind = HarnessCredentialInput["kind"];

function formatUnix(seconds: number): string {
  return new Date(seconds * 1000).toLocaleString();
}

function LinkForm(props: { onLinked: () => unknown }) {
  const [harness, setHarness] = createSignal<HarnessKind>("claude_code");
  const [credentialKind, setCredentialKind] = createSignal<CredentialKind>("claude_setup_token");
  const [label, setLabel] = createSignal("");
  const [secret, setSecret] = createSignal("");
  const [submitting, setSubmitting] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);

  function selectHarness(next: HarnessKind): void {
    setHarness(next);
    setCredentialKind(next === "claude_code" ? "claude_setup_token" : "codex_api_key");
    setSecret("");
  }

  function credential(): HarnessCredentialInput {
    switch (credentialKind()) {
      case "claude_setup_token":
        return { kind: "claude_setup_token", token: secret() };
      case "claude_api_key":
        return { kind: "claude_api_key", key: secret() };
      case "codex_api_key":
        return { kind: "codex_api_key", key: secret() };
    }
  }

  async function onSubmit(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    setSubmitting(true);
    setError(null);
    try {
      await linkHarnessAccount({ label: label(), credential: credential() });
      setLabel("");
      setSecret("");
      await props.onLinked();
    } catch (err) {
      setError(err);
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <form class={styles.form} onSubmit={(event) => void onSubmit(event)}>
      <div class={styles.field}>
        <label for="harness-kind">Harness</label>
        <select
          id="harness-kind"
          value={harness()}
          onChange={(event) => selectHarness(event.currentTarget.value as HarnessKind)}
        >
          <option value="claude_code">Claude Code</option>
          <option value="codex">Codex</option>
        </select>
      </div>

      <Show when={harness() === "claude_code"}>
        <div class={styles.field}>
          <label for="credential-kind">Credential</label>
          <select
            id="credential-kind"
            value={credentialKind()}
            onChange={(event) => {
              setCredentialKind(event.currentTarget.value as CredentialKind);
              setSecret("");
            }}
          >
            <option value="claude_setup_token">Claude subscription setup token</option>
            <option value="claude_api_key">Anthropic API key</option>
          </select>
        </div>
      </Show>

      <div class={styles.field}>
        <label for="harness-label">Label</label>
        <input
          id="harness-label"
          value={label()}
          onInput={(event) => setLabel(event.currentTarget.value)}
          placeholder="Personal"
          required
        />
      </div>

      <div class={styles.field}>
        <label for="harness-secret">
          {credentialKind() === "claude_setup_token" ? "Setup token" : "API key"}
        </label>
        <input
          id="harness-secret"
          type="password"
          value={secret()}
          onInput={(event) => setSecret(event.currentTarget.value)}
          autocomplete="off"
          required
        />
      </div>

      <Show when={credentialKind() === "claude_setup_token"}>
        <p class={styles.tabDescription}>
          Run <code>claude setup-token</code> on a trusted computer, then paste the long-lived
          token it prints. Flyco encrypts it before storage.
        </p>
      </Show>

      <ProblemNotice error={error()} />
      <button type="submit" class={styles.primaryButton} disabled={submitting()}>
        {submitting() ? "Linking…" : "Link account"}
      </button>
    </form>
  );
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

      <LinkForm onLinked={refetch} />

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
