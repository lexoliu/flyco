/**
 * Settings → Agents (docs/ux.md §10).
 *
 * One card per harness, whether or not it is linked: the card is the answer
 * to "can flyco run Claude Code for me", and a harness with nothing linked
 * has to occupy the same space as one that is, or the absence is invisible.
 *
 * Linking and relinking both open the very same `HarnessLinkForm` that
 * `/connect/harness` and the welcome flow use — there is one credential
 * form in this app, and this is a third place it is shown, not a third copy
 * of it.
 */
import { For, Show, createResource, createSignal } from "solid-js";
import Logomark, { HARNESS_MARK } from "../../components/Logomark";
import HarnessLinkForm from "../../components/link/HarnessLinkForm";
import ProblemNotice from "../../components/ProblemNotice";
import Disclosure from "../../components/Disclosure";
import RatioBar from "../../components/RatioBar";
import { useReadiness } from "../../components/Readiness";
import HarnessMatrix from "./HarnessMatrix";
import {
  listLlmUsage,
  unlinkHarnessAccount,
  type HarnessAccountView,
  type HarnessKind,
  type LlmUsageRow,
} from "../../api/client";
import { formatDate } from "../../lib/dates";
import { formatUsd } from "../../lib/money";
import { relativeTime } from "../../lib/relativeTime";
import { cx } from "../../lib/cx";
import styles from "./Settings.module.css";

const HARNESSES: readonly { kind: HarnessKind; label: string; runsOn: string }[] = [
  {
    kind: "claude_code",
    label: "Claude Code",
    runsOn: "Runs on your Anthropic subscription or API key.",
  },
  { kind: "codex", label: "Codex", runsOn: "Runs on your OpenAI API key." },
];

export default function AgentsSection() {
  const readiness = useReadiness();
  const [usage] = createResource(listLlmUsage);
  const [linking, setLinking] = createSignal<HarnessKind | null>(null);
  const [actionError, setActionError] = createSignal<unknown>(null);

  async function unlink(id: HarnessAccountView["id"]): Promise<void> {
    setActionError(null);
    try {
      await unlinkHarnessAccount(id);
      await readiness.refresh();
    } catch (err) {
      setActionError(err);
    }
  }

  async function onLinked(): Promise<void> {
    setLinking(null);
    await readiness.refresh();
  }

  const accountsFor = (kind: HarnessKind) =>
    readiness.harness().filter((account) => account.harness === kind);

  return (
    <section class={styles.section}>
      <header class={styles.sectionHead}>
        <h2>Agents</h2>
        <p class={styles.lede}>
          Flyco runs the official Claude Code and Codex on your own account, so every token is
          billed by your plan and never resold.
        </p>
      </header>

      <ProblemNotice error={readiness.error() ?? actionError()} />

      <div class={cx(styles.cards, styles.cardsPaired)}>
        <For each={HARNESSES}>
          {(harness) => {
            const accounts = () => accountsFor(harness.kind);
            return (
              <article class={styles.card}>
                <div class={styles.cardTop}>
                  <span class={styles.mark}>
                    <Logomark mark={HARNESS_MARK[harness.kind]} size={17} />
                  </span>
                  <div class={styles.identity}>
                    <span class={styles.cardTitle}>{harness.label}</span>
                    <span class={styles.cardMeta}>{harness.runsOn}</span>
                  </div>
                  <div class={styles.actions}>
                    <span class={cx(styles.status, accounts().length > 0 && styles.statusOn)}>
                      {accounts().length > 0 ? "Linked" : "Not linked"}
                    </span>
                  </div>
                </div>

                <Show
                  when={accounts().length > 0}
                  fallback={
                    <div class={styles.cardBody}>
                      <button
                        type="button"
                        class={styles.pillPrimary}
                        onClick={() =>
                          setLinking(linking() === harness.kind ? null : harness.kind)
                        }
                      >
                        {linking() === harness.kind ? "Cancel" : `Connect ${harness.label}`}
                      </button>
                    </div>
                  }
                >
                  <div class={styles.cardBody}>
                    <For each={accounts()}>
                      {(account) => (
                        <div class={styles.cardBody}>
                          <div class={styles.cardTop}>
                            <div class={styles.identity}>
                              <span class={styles.cardTitle}>{account.label}</span>
                              <span class={styles.cardMeta}>
                                Linked {formatDate(account.linked_at_unix)}
                                {account.expires_at_unix !== null &&
                                account.expires_at_unix !== undefined
                                  ? ` · expires ${formatDate(account.expires_at_unix)}`
                                  : ""}
                              </span>
                            </div>
                            <div class={styles.actions}>
                              <button
                                type="button"
                                class={styles.pill}
                                onClick={() =>
                                  setLinking(linking() === harness.kind ? null : harness.kind)
                                }
                              >
                                Relink
                              </button>
                              <button
                                type="button"
                                class={styles.pillDanger}
                                onClick={() => void unlink(account.id)}
                              >
                                Unlink
                              </button>
                            </div>
                          </div>
                          <AccountUsage
                            row={usage()?.find((row) => row.account === account.id)}
                          />
                        </div>
                      )}
                    </For>
                  </div>
                </Show>

                <Show when={linking() === harness.kind}>
                  <HarnessLinkForm onLinked={() => void onLinked()} />
                </Show>
              </article>
            );
          }}
        </For>
      </div>

      <Disclosure summary="What works on each harness">
        <HarnessMatrix />
      </Disclosure>
    </section>
  );
}

/**
 * What `GET /v1/usage/llm` will actually say about an account.
 *
 * Deliberately not a "42% of your quota" bar. The endpoint's own contract
 * is that every field is an observation and never a quota, so the two
 * honest readings are the cost the harness reported over this window and,
 * once the vendor has actually limited the account, how much of the wait
 * for the reset has passed. Nothing renders at all until there is
 * something true to render.
 */
function AccountUsage(props: { row: LlmUsageRow | undefined }) {
  const now = Date.now();
  const limitedAt = () => props.row?.rate_limited_at_unix ?? null;
  const resetsAt = () => props.row?.resets_at_unix ?? null;
  const waited = () => {
    const from = limitedAt();
    const to = resetsAt();
    if (from === null || to === null || to <= from) {
      return undefined;
    }
    return (Math.floor(now / 1000) - from) / (to - from);
  };

  return (
    <Show when={props.row}>
      {(row) => (
        <div class={styles.cardBody}>
          {/* A cost with no ceiling is a figure, not a bar. */}
          <Show when={row().observed_cost !== null && row().observed_cost !== undefined}>
            <p class={styles.cardMeta}>
              {formatUsd(row().observed_cost ?? 0)} reported by the harness since{" "}
              {formatDate(row().period_start_unix)}
            </p>
          </Show>
          <Show when={waited() !== undefined}>
            <RatioBar
              label="Usage limit"
              ratio={waited()}
              value={`resets ${relativeTime(resetsAt() ?? 0, now)}`}
              tier="warn"
            />
          </Show>
        </div>
      )}
    </Show>
  );
}
