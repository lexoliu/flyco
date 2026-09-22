/**
 * Settings → Agents (docs/ux.md §10).
 *
 * One card per harness, whether or not it is linked: the card is the answer
 * to "can flyco run Claude Code for me", and a harness with nothing linked
 * has to occupy the same space as one that is, or the absence is invisible.
 *
 * `Connect` and `Relink` both lead to `/connect/harness`, which walks the
 * very same pages the first run does (docs/ux.md §4) — there is one way to
 * connect an agent in this app, and this card is a place it is reached
 * from, not a second copy of it. The card names the agent, so the flow
 * starts on that agent's sign-in page and comes back here when it is done.
 */
import { For, Show, createSignal, onCleanup } from "solid-js";
import { A } from "@solidjs/router";
import { createQuery } from "../../lib/query";
import ConfirmDialog from "../../components/ConfirmDialog";
import Logomark, { HARNESS_MARK } from "../../components/Logomark";
import ProblemNotice from "../../components/ProblemNotice";
import HarnessUsage from "../../components/HarnessUsage";
import { useReadiness } from "../../components/Readiness";
import {
  listLlmUsage,
  unlinkHarnessAccount,
  type HarnessAccountView,
  type HarnessKind,
} from "../../api/client";
import { formatDate } from "../../lib/dates";
import { credentialExpiry } from "../../lib/expiry";
import { cx } from "../../lib/cx";
import { inUseRefusal, sessionsStillRunning, type InUseRefusal } from "../../lib/inUse";
import styles from "./Settings.module.css";

const HARNESSES: readonly { kind: HarnessKind; label: string; runsOn: string }[] = [
  {
    kind: "claude_code",
    label: "Claude Code",
    runsOn: "Runs on your Claude subscription, or an Anthropic API key.",
  },
  { kind: "codex", label: "Codex", runsOn: "Runs on your ChatGPT subscription, or an OpenAI API key." },
  { kind: "devin", label: "Devin", runsOn: "Runs on your Devin account, signed in or with a token." },
];

/** How often the clock this page reads against moves. */
const TICK_MS = 60_000;

export default function AgentsSection() {
  const readiness = useReadiness();
  // An expiry is a distance from now, and a settings page can be left open
  // across the day it falls due. A minute is finer than anything measured
  // in whole days needs, and cheaper than the page's own polling.
  const [now, setNow] = createSignal(Math.floor(Date.now() / 1000));
  const ticker = setInterval(() => setNow(Math.floor(Date.now() / 1000)), TICK_MS);
  onCleanup(() => clearInterval(ticker));
  const [usage] = createQuery(listLlmUsage);
  const [actionError, setActionError] = createSignal<unknown>(null);
  /** The account whose unlink has been asked about but not yet answered. */
  const [unlinking, setUnlinking] = createSignal<HarnessAccountView | null>(null);
  /** What a refused unlink said, when sessions still run on the harness. */
  const [refused, setRefused] = createSignal<InUseRefusal | null>(null);
  const [busy, setBusy] = createSignal(false);

  async function unlink(id: HarnessAccountView["id"]): Promise<void> {
    setActionError(null);
    setBusy(true);
    try {
      await unlinkHarnessAccount(id);
      setUnlinking(null);
      await readiness.refresh();
    } catch (err) {
      const inUse = inUseRefusal(err);
      if (inUse === null) {
        setActionError(err);
      } else {
        // There is no forcing this one: those sessions renew their grant
        // against this credential, and flyco will not break them because a
        // button was pressed. The dialog keeps its place and says what ends
        // the refusal instead (issue #152).
        setRefused(inUse);
      }
    } finally {
      setBusy(false);
    }
  }

  /** Puts the question away, however it was answered. */
  function stopUnlinking(): void {
    setUnlinking(null);
    setRefused(null);
    setActionError(null);
  }

  /** Where the connect flow starts for one agent, and where it returns to. */
  const connectPath = (kind: HarnessKind) => `/connect/harness?agent=${kind}&return=/settings/agents`;

  const accountsFor = (kind: HarnessKind) =>
    readiness.harness().filter((account) => account.harness === kind);

  return (
    <section class={styles.section}>
      <header class={styles.sectionHead}>
        <h2>Agents</h2>
      </header>

      <ProblemNotice error={readiness.error() ?? usage.error} />

      <div class={cx(styles.cards, styles.cardsPaired)}>
        <For each={HARNESSES}>
          {(harness) => {
            const accounts = () => accountsFor(harness.kind);
            /**
             * The one account on this card whose credential is running out,
             * so the card's own pill can say so. A harness with two accounts
             * linked is still one row in the sidebar and one answer to "can
             * flyco run this for me", and that answer is no the moment any of
             * them stops refreshing.
             */
            const soonest = () =>
              accounts()
                .map((account) => credentialExpiry(account.expires_at_unix, now()))
                .find((expiry) => expiry !== null && expiry.level !== "fine") ?? null;
            return (
              <article class={styles.card}>
                <div class={styles.cardTop}>
                  <span class={styles.mark}>
                    <Logomark mark={HARNESS_MARK[harness.kind]} size={17} />
                  </span>
                  <div class={styles.identity}>
                    <span class={styles.cardTitle}>{harness.label}</span>
                  </div>
                  <div class={styles.actions}>
                    <span
                      class={cx(
                        styles.status,
                        soonest() === null && accounts().length > 0 && styles.statusOn,
                        soonest() !== null && styles.statusWarn,
                      )}
                    >
                      {accounts().length === 0 ? "Not linked" : "Linked"}
                    </span>
                  </div>
                </div>

                <Show
                  when={accounts().length > 0}
                  fallback={
                    <div class={styles.cardBody}>
                      <A href={connectPath(harness.kind)} class={styles.pillPrimary}>
                        Connect {harness.label}
                      </A>
                    </div>
                  }
                >
                  <div class={styles.cardBody}>
                    <For each={accounts()}>
                      {(account) => {
                        const expiry = () => credentialExpiry(account.expires_at_unix, now());
                        return (
                        <div class={styles.cardBody}>
                          <div class={styles.cardTop}>
                            <div class={styles.identity}>
                              <span class={styles.cardTitle}>{account.label}</span>
                              <span class={styles.cardMeta}>
                                Linked {formatDate(account.linked_at_unix)}
                                <Show when={expiry()}>
                                  {(due) => (
                                    <>
                                      {" · "}
                                      <span
                                        class={cx(
                                          due().level !== "fine" && styles.metaUrgent,
                                        )}
                                      >
                                        {due().sentence}
                                      </span>
                                    </>
                                  )}
                                </Show>
                              </span>
                            </div>
                            <div class={styles.actions}>
                              {/*
                                Relinking is the only thing that fixes an
                                expiry, so once one is close it stops being
                                one of two equal pills and becomes the card's
                                action.
                              */}
                              <A
                                href={connectPath(harness.kind)}
                                class={
                                  expiry()?.level !== undefined && expiry()?.level !== "fine"
                                    ? styles.pillPrimary
                                    : styles.pill
                                }
                              >
                                Relink
                              </A>
                              {/*
                                Nothing else while the question is up: the
                                dialog under the card is the one thing being
                                answered, and a second `Unlink` beside it
                                would be two buttons with one meaning.
                              */}
                              <Show when={unlinking()?.id !== account.id}>
                                <button
                                  type="button"
                                  class={styles.pillDanger}
                                  disabled={busy()}
                                  onClick={() => {
                                    setActionError(null);
                                    setRefused(null);
                                    setUnlinking(account);
                                  }}
                                >
                                  Unlink
                                </button>
                              </Show>
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
                                <Show
                                  when={refused()}
                                  fallback={
                                    <>
                                      Flyco forgets this credential and no new session can run{" "}
                                      {harness.label} on it. Signing in again links it back.
                                    </>
                                  }
                                >
                                  {(inUse) => (
                                    <>
                                      {sessionsStillRunning(inUse())} They renew their{" "}
                                      {harness.label} token against this credential and would stop
                                      the moment it expired, so archive them first.
                                    </>
                                  )}
                                </Show>
                              }
                              confirmLabel="Unlink"
                              cancelLabel={refused() === null ? "Keep it" : "Close"}
                              busy={busy()}
                              {...(refused() === null
                                ? { onConfirm: () => void unlink(account.id) }
                                : {})}
                              onCancel={stopUnlinking}
                            >
                              <ProblemNotice error={actionError()} />
                              <Show when={refused()}>
                                <A href="/" class={styles.sessionsLink}>
                                  Go to Sessions
                                </A>
                              </Show>
                            </ConfirmDialog>
                          </Show>
                          <HarnessUsage
                            row={usage()?.find((row) => row.account === account.id)}
                            plan={account.usage}
                          />
                        </div>
                        );
                      }}
                    </For>
                  </div>
                </Show>
              </article>
            );
          }}
        </For>
      </div>
    </section>
  );
}
