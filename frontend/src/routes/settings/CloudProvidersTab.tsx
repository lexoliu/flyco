import { For, Show, createResource, createSignal } from "solid-js";
import ProblemNotice from "../../components/ProblemNotice";
import ProviderLinkForm from "../../components/link/ProviderLinkForm";
import {
  listProviders,
  providerQuickstart,
  unlinkProvider,
  type CloudProviderKind,
  type ProviderBonusHint,
} from "../../api/client";
import { formatUsd } from "../../lib/money";
import { PROVIDER_LABEL } from "../../lib/providers";
import styles from "../../components/Panel.module.css";

function bonusLabel(kind: CloudProviderKind): string {
  return PROVIDER_LABEL[kind];
}

/**
 * Two-question quickstart: which of the providers' free-credit and
 * education programmes are actually worth this user's time, given whether
 * they are new to each provider and whether they qualify as a student.
 */
function QuickstartForm() {
  const [newToProvider, setNewToProvider] = createSignal(true);
  const [isStudent, setIsStudent] = createSignal(false);
  const [hints, setHints] = createSignal<ProviderBonusHint[] | null>(null);
  const [submitting, setSubmitting] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);

  async function onSubmit(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    setSubmitting(true);
    setError(null);
    try {
      const result = await providerQuickstart({
        new_to_provider: newToProvider(),
        is_student: isStudent(),
      });
      setHints(result);
    } catch (err) {
      setError(err);
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <div class={styles.form}>
      <h3>Quickstart: find free credit</h3>
      <form onSubmit={(event) => void onSubmit(event)} class={styles.form} style={{ border: "none", padding: "0" }}>
        <label class={styles.checkboxField}>
          <input
            type="checkbox"
            checked={newToProvider()}
            onChange={(event) => setNewToProvider(event.currentTarget.checked)}
          />
          I've never held an account with these providers
        </label>
        <label class={styles.checkboxField}>
          <input type="checkbox" checked={isStudent()} onChange={(event) => setIsStudent(event.currentTarget.checked)} />
          I'm a student
        </label>
        <ProblemNotice error={error()} />
        <button type="submit" class={styles.primaryButton} disabled={submitting()}>
          {submitting() ? "Checking…" : "Check for bonus programmes"}
        </button>
      </form>

      <Show when={hints()}>
        {(list) => (
          <Show when={list().length > 0} fallback={<p class={styles.empty}>No bonus programmes apply right now.</p>}>
            <ul class={styles.list}>
              <For each={list()}>
                {(hint) => (
                  <li class={styles.listItem}>
                    <div>
                      <strong>
                        {hint.title} — {bonusLabel(hint.provider)}
                      </strong>
                      <p class={styles.itemDetail}>
                        {hint.detail}
                        {hint.credit !== null && hint.credit !== undefined ? ` · up to ${formatUsd(hint.credit)}` : ""}
                      </p>
                    </div>
                    <div class={styles.itemActions}>
                      <a href={hint.url} target="_blank" rel="noreferrer noopener">
                        Sign up
                      </a>
                    </div>
                  </li>
                )}
              </For>
            </ul>
          </Show>
        )}
      </Show>
    </div>
  );
}

export default function CloudProvidersTab() {
  const [providers, { refetch }] = createResource(listProviders);
  const [unlinkError, setUnlinkError] = createSignal<unknown>(null);

  async function onUnlink(id: string): Promise<void> {
    setUnlinkError(null);
    try {
      await unlinkProvider(id);
      await refetch();
    } catch (err) {
      setUnlinkError(err);
    }
  }

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

      <QuickstartForm />
      <ProviderLinkForm onLinked={() => void refetch()} />
      <ProblemNotice error={providers.error ?? unlinkError()} />

      <Show when={!providers.loading}>
        <Show
          when={providers.error !== undefined || (providers() ?? []).length > 0}
          fallback={<p class={styles.empty}>No cloud providers connected yet.</p>}
        >
          <ul class={styles.list}>
            <For each={providers()}>
              {(provider) => (
                <li class={styles.listItem}>
                  <div>
                    <strong>{provider.label}</strong>
                    <p class={styles.itemDetail}>{PROVIDER_LABEL[provider.kind]}</p>
                  </div>
                  <div class={styles.itemActions}>
                    <button type="button" class={styles.dangerButton} onClick={() => void onUnlink(provider.id)}>
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
