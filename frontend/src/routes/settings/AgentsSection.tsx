/**
 * Settings → Agents (docs/ux.md §10).
 *
 * One card per harness, whether or not it is linked: the card is the answer
 * to "can flyco run Claude Code for me", and a harness with nothing linked
 * has to occupy the same space as one that is, or the absence is invisible.
 *
 * Linking and relinking both open the very same `HarnessConnect` flow that
 * `/connect/harness` and the welcome flow use — there is one way to connect
 * an agent in this app, and this is a third place it is shown, not a third
 * copy of it.
 */
import { For, Show, createSignal } from "solid-js";
import { createQuery } from "../../lib/query";
import ConfirmDialog from "../../components/ConfirmDialog";
import Logomark, { HARNESS_MARK } from "../../components/Logomark";
import { HarnessConnect } from "../../components/link/HarnessChooser";
import ProblemNotice from "../../components/ProblemNotice";
import Disclosure from "../../components/Disclosure";
import HarnessUsage from "../../components/HarnessUsage";
import { useReadiness } from "../../components/Readiness";
import HarnessMatrix from "./HarnessMatrix";
import {
  listLlmUsage,
  unlinkHarnessAccount,
  type HarnessAccountView,
  type HarnessKind,
} from "../../api/client";
import { formatDate } from "../../lib/dates";
import { cx } from "../../lib/cx";
import styles from "./Settings.module.css";

const HARNESSES: readonly { kind: HarnessKind; label: string; runsOn: string }[] = [
  {
    kind: "claude_code",
    label: "Claude Code",
    runsOn: "Runs on your Claude subscription, or an Anthropic API key.",
  },
  { kind: "codex", label: "Codex", runsOn: "Runs on your ChatGPT subscription, or an OpenAI API key." },
];

export default function AgentsSection() {
  const readiness = useReadiness();
  const [usage] = createQuery(listLlmUsage);
  const [linking, setLinking] = createSignal<HarnessKind | null>(null);
  const [actionError, setActionError] = createSignal<unknown>(null);
  /** The account whose unlink has been asked about but not yet answered. */
  const [unlinking, setUnlinking] = createSignal<HarnessAccountView | null>(null);
  const [busy, setBusy] = createSignal(false);

  async function unlink(id: HarnessAccountView["id"]): Promise<void> {
    setActionError(null);
    setBusy(true);
    try {
      await unlinkHarnessAccount(id);
      setUnlinking(null);
      await readiness.refresh();
    } catch (err) {
      setActionError(err);
    } finally {
      setBusy(false);
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

      <ProblemNotice error={readiness.error() ?? usage.error} />

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
                                disabled={busy()}
                                onClick={() => {
                                  setActionError(null);
                                  setUnlinking(account);
                                }}
                              >
                                Unlink
                              </button>
                            </div>
                          </div>
                          {/*
                            Unlinking is not undoable from here — the
                            credential is forgotten and has to be signed in
                            again — and it takes the token every running
                            session refreshes with, so it asks first (issue
                            #139).
                          */}
                          <Show when={unlinking()?.id === account.id}>
                            <ConfirmDialog
                              title={`Unlink ${account.label}?`}
                              body={
                                <>
                                  Flyco forgets this credential and no new session can run{" "}
                                  {harness.label} on it. A session already running keeps going
                                  until its token needs refreshing, which flyco can no longer do.
                                  Signing in again links it back.
                                </>
                              }
                              confirmLabel="Unlink"
                              cancelLabel="Keep it"
                              busy={busy()}
                              onConfirm={() => void unlink(account.id)}
                              onCancel={() => setUnlinking(null)}
                            >
                              <ProblemNotice error={actionError()} />
                            </ConfirmDialog>
                          </Show>
                          <HarnessUsage row={usage()?.find((row) => row.account === account.id)} />
                        </div>
                      )}
                    </For>
                  </div>
                </Show>

                <Show when={linking() === harness.kind}>
                  <HarnessConnect
                    harness={harness.kind}
                    onLinked={() => void onLinked()}
                    onCancel={() => setLinking(null)}
                  />
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
