/**
 * Connecting an agent (docs/ux.md §8), wherever the user is standing.
 *
 * Two cards, Claude Code and Codex, each expanding in place. It is reached
 * from `/connect/harness`, from the welcome flow, and — as a single card —
 * from Settings › Agents' `Relink`, so the flow lives here once and those
 * three places frame it rather than reimplementing it.
 *
 * Both cards are two steps because both vendors' flows are: flyco opens the
 * page it was given, and the code travels between the two screens. They
 * travel in opposite directions — Anthropic shows a code the user brings
 * back, OpenAI takes a code flyco was given — which is why one card ends in
 * a field and the other ends in a wait.
 *
 * Everything that was the old credential form — the setup token, the API
 * keys — is still there, under `Advanced`, because a person who already has
 * one should not have to run a browser flow to use it.
 */
import {
  For,
  Match,
  Show,
  Switch,
  createSignal,
  onCleanup,
  type JSX,
} from "solid-js";
import { createQuery } from "../../lib/query";
import { ArrowUpRight, Check, Copy } from "lucide-solid";
import Logomark, { HARNESS_MARK } from "../Logomark";
import Disclosure from "../Disclosure";
import HarnessUsage from "../HarnessUsage";
import ProblemNotice from "../ProblemNotice";
import { useReadiness } from "../Readiness";
import {
  completeClaudeOauth,
  linkHarnessAccount,
  listLlmUsage,
  pollCodexOauth,
  startClaudeOauth,
  startCodexOauth,
  type ClaudeOauthStart,
  type HarnessAccountView,
  type HarnessCredentialInput,
  type HarnessKind,
} from "../../api/client";
import { parsePastedCode, pastedCodeForExchange } from "../../lib/claudeCode";
import {
  CHATGPT_SECURITY_SETTINGS_URL,
  IDLE,
  isRunning,
  nextSignIn,
  pollDelayMs,
  type CodexAttempt,
  type CodexEvent,
  type CodexSignIn,
} from "../../lib/codexDevice";
import { formatDate } from "../../lib/dates";
import { cx } from "../../lib/cx";
import styles from "./HarnessChooser.module.css";

/** Where an `OpenAI` key is made. */
const OPENAI_KEYS_URL = "https://platform.openai.com/api-keys";

/** The two agents flyco runs, in the order the chooser lists them. */
const HARNESSES: readonly { kind: HarnessKind; label: string; runsOn: string }[] = [
  {
    kind: "claude_code",
    label: "Claude Code",
    runsOn: "Runs on your Claude subscription. Sign in and flyco never sees your password.",
  },
  {
    kind: "codex",
    label: "Codex",
    runsOn: "Runs on your ChatGPT subscription. Sign in and flyco never sees your password.",
  },
];

export interface HarnessChooserProps {
  /** Called after a successful link, so readiness can be reloaded. */
  onLinked: () => unknown;
}

/**
 * The two-card chooser.
 *
 * A card that is already linked shows what is linked, when its credential
 * expires, and what the harness has reported using — the readout docs/ux.md
 * §8 asks for — and can still be reconnected in place.
 */
export default function HarnessChooser(props: HarnessChooserProps) {
  const readiness = useReadiness();
  const [usage] = createQuery(listLlmUsage);
  const [open, setOpen] = createSignal<HarnessKind | null>(null);

  const accountFor = (kind: HarnessKind): HarnessAccountView | undefined =>
    readiness.harness().find((account) => account.harness === kind);

  async function onLinked(): Promise<void> {
    setOpen(null);
    await props.onLinked();
  }

  return (
    <>
      {/* Usage is what a linked account has spent, secondary to whether it is
          linked at all — so its failure is a line above the cards rather than
          anything that stops them rendering. */}
      <ProblemNotice error={usage.error} />
      <div class={styles.cards}>
        <For each={HARNESSES}>
          {(harness) => {
            const account = () => accountFor(harness.kind);
            const expanded = () => open() === harness.kind;
            return (
              <article class={cx(styles.card, expanded() && styles.cardOpen)}>
                <div class={styles.head}>
                  <span class={styles.mark}>
                    <Logomark mark={HARNESS_MARK[harness.kind]} size={17} />
                  </span>
                  <div class={styles.identity}>
                    <span class={styles.title}>{harness.label}</span>
                    <span class={styles.meta}>{account()?.label ?? harness.runsOn}</span>
                  </div>
                  <span class={cx(styles.status, account() !== undefined && styles.statusOn)}>
                    {account() === undefined ? "Not linked" : "Linked"}
                  </span>
                </div>

                <Show when={account()}>
                  {(linked) => (
                    <div class={styles.linked}>
                      <p class={styles.meta}>
                        Linked {formatDate(linked().linked_at_unix)}
                        <Show when={linked().expires_at_unix}>
                          {(expires) => <> · expires {formatDate(expires())}</>}
                        </Show>
                      </p>
                      {/* A grant with an end is renewed before a session is
                          given it, so the date above is a fact rather than a
                          deadline the user has to act on. */}
                      <Show when={linked().expires_at_unix}>
                        <p class={styles.faint}>Renewed automatically before a session uses it.</p>
                      </Show>
                      <HarnessUsage row={usage()?.find((row) => row.account === linked().id)} />
                    </div>
                  )}
                </Show>

                <Show
                  when={expanded()}
                  fallback={
                    <div class={styles.actions}>
                      <button
                        type="button"
                        class={account() === undefined ? styles.pillPrimary : styles.pill}
                        onClick={() => setOpen(harness.kind)}
                      >
                        {account() === undefined
                          ? `Connect ${harness.label}`
                          : `Reconnect ${harness.label}`}
                      </button>
                    </div>
                  }
                >
                  <HarnessConnect
                    harness={harness.kind}
                    onLinked={() => void onLinked()}
                    onCancel={() => setOpen(null)}
                  />
                </Show>
              </article>
            );
          }}
        </For>
      </div>
    </>
  );
}

export interface HarnessConnectProps {
  /** Which agent is being connected. */
  harness: HarnessKind;
  /** Called after a successful link. */
  onLinked: () => unknown;
  /** Offered as `Cancel` when the caller can close the flow. */
  onCancel?: (() => void) | undefined;
}

/**
 * One agent's connect flow, without the card around it.
 *
 * Settings › Agents opens this under the card it already draws; the chooser
 * above opens it inside the card it drew.
 */
export function HarnessConnect(props: HarnessConnectProps) {
  return (
    <Switch>
      <Match when={props.harness === "claude_code"}>
        <ClaudeConnect onLinked={props.onLinked} onCancel={props.onCancel} />
      </Match>
      <Match when={props.harness === "codex"}>
        <CodexConnect onLinked={props.onLinked} onCancel={props.onCancel} />
      </Match>
    </Switch>
  );
}

/** Where the Claude card is in its two-step flow. */
type ClaudeStep = "sign-in" | "paste";

function ClaudeConnect(props: Omit<HarnessConnectProps, "harness">) {
  const [step, setStep] = createSignal<ClaudeStep>("sign-in");
  const [attempt, setAttempt] = createSignal<ClaudeOauthStart | null>(null);
  const [pasted, setPasted] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);

  const code = () => parsePastedCode(pasted());

  /** Opens the authorize page flyco was handed, in a tab of its own. */
  function open(url: string): void {
    window.open(url, "_blank", "noopener,noreferrer");
  }

  async function signIn(): Promise<void> {
    setBusy(true);
    setError(null);
    try {
      const started = await startClaudeOauth();
      setAttempt(started);
      open(started.authorize_url);
      setStep("paste");
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  async function submit(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    const started = attempt();
    const parsed = code();
    if (started === null || parsed === null) {
      return;
    }

    setBusy(true);
    setError(null);
    try {
      await completeClaudeOauth({
        attempt_id: started.attempt_id,
        code: pastedCodeForExchange(parsed),
      });
      setPasted("");
      await props.onLinked();
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  return (
    <div class={styles.flow}>
      <ol class={styles.steps}>
        <li class={cx(styles.step, step() === "sign-in" && styles.stepCurrent)}>
          <span class={styles.stepMark}>1</span>
          <div class={styles.stepBody}>
            <p class={styles.stepTitle}>Sign in at Anthropic</p>
            <Show when={step() === "sign-in"}>
              <p class={styles.hint}>
                Flyco opens Anthropic's own sign-in page. Your password never reaches flyco.
              </p>
              <button
                type="button"
                class={styles.pillPrimary}
                disabled={busy()}
                onClick={() => void signIn()}
              >
                {busy() ? "Opening…" : "Sign in with Claude"}
                <ArrowUpRight size={14} aria-hidden="true" />
              </button>
            </Show>
          </div>
        </li>

        <li class={cx(styles.step, step() === "paste" && styles.stepCurrent)}>
          <span class={styles.stepMark}>2</span>
          <div class={styles.stepBody}>
            <p class={styles.stepTitle}>Paste the code Anthropic shows you</p>
            <Show when={step() === "paste"}>
              <form class={styles.form} onSubmit={(event) => void submit(event)}>
                <div class={styles.field}>
                  <label for="claude-oauth-code">Code from Anthropic</label>
                  <input
                    id="claude-oauth-code"
                    class={styles.mono}
                    value={pasted()}
                    onInput={(event) => setPasted(event.currentTarget.value)}
                    autocomplete="off"
                    spellcheck={false}
                    autofocus
                  />
                  <p class={styles.hint}>
                    It looks like <code>CODE#STATE</code> — paste the whole thing.
                  </p>
                </div>
                <div class={styles.actions}>
                  <button type="submit" class={styles.pillPrimary} disabled={busy() || code() === null}>
                    {busy() ? "Linking…" : "Link Claude Code"}
                  </button>
                  <button
                    type="button"
                    class={styles.quiet}
                    onClick={() => {
                      const started = attempt();
                      if (started !== null) {
                        open(started.authorize_url);
                      }
                    }}
                  >
                    Open the sign-in page again
                  </button>
                </div>
              </form>
            </Show>
          </div>
        </li>
      </ol>

      <ProblemNotice error={error()} />

      <Disclosure summary="Advanced">
        <CredentialForm
          fields={[
            {
              kind: "claude_setup_token",
              label: "Setup token",
              hint: (
                <>
                  Run <code>claude setup-token</code> on a computer you trust and paste the
                  long-lived token it prints.
                </>
              ),
              accountLabel: "Claude subscription",
              submitLabel: "Link with a setup token",
            },
            {
              kind: "claude_api_key",
              label: "Anthropic API key",
              hint: "Billed per token by Anthropic rather than by your subscription.",
              accountLabel: "Anthropic API key",
              submitLabel: "Link with an API key",
            },
          ]}
          onLinked={props.onLinked}
        />
      </Disclosure>

      <Show when={props.onCancel}>
        {(cancel) => (
          <button type="button" class={styles.quiet} onClick={() => cancel()()}>
            Cancel
          </button>
        )}
      </Show>
    </div>
  );
}

/**
 * Codex's sign-in, which is the device-code flow `codex login
 * --device-auth` runs.
 *
 * OpenAI's browser flow ends at a localhost callback a hosted web app
 * cannot offer, so the code is the transport: flyco asks OpenAI for one,
 * shows it, opens the page it is typed on, and then waits — polling the
 * attempt every `interval_seconds` until it is approved or runs out.
 *
 * The waiting is a state machine rather than a spinner (see
 * `src/lib/codexDevice.ts`), because it has real outcomes the card has to
 * render: an approval, a code that went stale, and OpenAI refusing to start
 * a device sign-in at all because the account has the flow switched off —
 * the one failure the person can fix themselves, so it gets instructions
 * and a link instead of an error.
 */
function CodexConnect(props: Omit<HarnessConnectProps, "harness">) {
  const [signIn, setSignIn] = createSignal<CodexSignIn>(IDLE);
  const [copied, setCopied] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);

  let timer: ReturnType<typeof setTimeout> | undefined;
  let closed = false;

  // A poll already in flight still resolves after the card goes away; the
  // flag is what stops it writing to a signal nobody is reading.
  onCleanup(() => {
    closed = true;
    if (timer !== undefined) {
      clearTimeout(timer);
    }
  });

  function advance(event: CodexEvent): CodexSignIn {
    const next = nextSignIn(signIn(), event);
    setSignIn(next);
    return next;
  }

  function schedule(state: CodexSignIn, attempt: CodexAttempt): void {
    const delay = pollDelayMs(state);
    if (delay === null || closed) {
      return;
    }
    timer = setTimeout(() => void ask(attempt), delay);
  }

  async function ask(attempt: CodexAttempt): Promise<void> {
    try {
      const progress = await pollCodexOauth(attempt.attemptId);
      if (closed) {
        return;
      }
      if (progress.state === "linked") {
        advance({ kind: "linked" });
        await props.onLinked();
        return;
      }
      schedule(advance({ kind: "pending" }), attempt);
    } catch (err) {
      if (closed) {
        return;
      }
      setError(err);
      advance({ kind: "failed", error: err });
    }
  }

  async function signInWithChatGpt(): Promise<void> {
    setError(null);
    setCopied(false);
    advance({ kind: "start" });
    try {
      const started = await startCodexOauth();
      const attempt: CodexAttempt = {
        attemptId: started.attempt_id,
        userCode: started.user_code,
        verificationUrl: started.verification_url,
        intervalSeconds: started.interval_seconds,
      };
      const state = advance({ kind: "started", attempt });
      window.open(attempt.verificationUrl, "_blank", "noopener,noreferrer");
      schedule(state, attempt);
    } catch (err) {
      setError(err);
      advance({ kind: "failed", error: err });
    }
  }

  /** Puts the code on the clipboard, where there is one to put it on. */
  async function copy(code: string): Promise<void> {
    await navigator.clipboard?.writeText(code);
    setCopied(true);
  }

  const state = () => signIn();
  const waiting = () => (state().step === "waiting" ? (state() as { attempt: CodexAttempt }) : null);
  const expired = () => (state().step === "expired" ? (state() as { attempt: CodexAttempt }) : null);

  return (
    <div class={styles.flow}>
      <ol class={styles.steps}>
        <li class={cx(styles.step, waiting() === null && styles.stepCurrent)}>
          <span class={styles.stepMark}>1</span>
          <div class={styles.stepBody}>
            <p class={styles.stepTitle}>Sign in with ChatGPT</p>
            <Show when={waiting() === null}>
              <p class={styles.hint}>
                Flyco asks OpenAI for a one-time code and opens OpenAI's own page. Your password
                never reaches flyco.
              </p>
              <button
                type="button"
                class={styles.pillPrimary}
                disabled={isRunning(state())}
                onClick={() => void signInWithChatGpt()}
              >
                {state().step === "starting"
                  ? "Asking OpenAI…"
                  : expired() === null
                    ? "Sign in with ChatGPT"
                    : "Get a new code"}
                <ArrowUpRight size={14} aria-hidden="true" />
              </button>
            </Show>
          </div>
        </li>

        <li class={cx(styles.step, waiting() !== null && styles.stepCurrent)}>
          <span class={styles.stepMark}>2</span>
          <div class={styles.stepBody}>
            <p class={styles.stepTitle}>Enter the code at auth.openai.com</p>
            <Show when={waiting()}>
              {(active) => (
                <>
                  <div class={styles.codeRow}>
                    <span class={styles.code} aria-label="One-time code">
                      {active().attempt.userCode}
                    </span>
                    <button
                      type="button"
                      class={styles.pill}
                      onClick={() => void copy(active().attempt.userCode)}
                    >
                      {copied() ? (
                        <Check size={14} aria-hidden="true" />
                      ) : (
                        <Copy size={14} aria-hidden="true" />
                      )}
                      {copied() ? "Copied" : "Copy code"}
                    </button>
                  </div>
                  <a
                    class={styles.link}
                    href={active().attempt.verificationUrl}
                    target="_blank"
                    rel="noreferrer"
                  >
                    Open auth.openai.com/codex/device
                    <ArrowUpRight size={13} aria-hidden="true" />
                  </a>
                  <p class={styles.waiting} role="status">
                    <span class={styles.pulse} aria-hidden="true" />
                    Waiting for you to approve in the browser…
                  </p>
                </>
              )}
            </Show>
            <Show when={expired()}>
              <p class={styles.hint}>
                That code expired before it was approved. Get a new one and try again.
              </p>
            </Show>
          </div>
        </li>
      </ol>

      <Show when={state().step === "blocked"}>
        <div class={styles.notice}>
          <p class={styles.hint}>
            OpenAI will not start a device sign-in for this account. Turn on{" "}
            <strong>device code authorization</strong> in your ChatGPT security settings — on a
            workspace account a workspace admin does it — and try again.
          </p>
          <a
            class={styles.link}
            href={CHATGPT_SECURITY_SETTINGS_URL}
            target="_blank"
            rel="noreferrer"
          >
            Open ChatGPT security settings
            <ArrowUpRight size={13} aria-hidden="true" />
          </a>
          <button type="button" class={styles.pill} onClick={() => void signInWithChatGpt()}>
            Try again
          </button>
        </div>
      </Show>

      <Show when={state().step !== "blocked"}>
        <ProblemNotice error={error()} />
      </Show>

      <Disclosure summary="Advanced">
        <p class={styles.hint}>
          Billed per token by OpenAI rather than by your subscription.{" "}
          <a class={styles.link} href={OPENAI_KEYS_URL} target="_blank" rel="noreferrer">
            Create a key on the API keys page
            <ArrowUpRight size={13} aria-hidden="true" />
          </a>
        </p>
        <CredentialForm
          fields={[
            {
              kind: "codex_api_key",
              label: "OpenAI API key",
              hint: (
                <>
                  Starts with <code>sk-</code>. Flyco encrypts it before storing it.
                </>
              ),
              accountLabel: "OpenAI API key",
              submitLabel: "Link with an API key",
            },
          ]}
          onLinked={props.onLinked}
        />
      </Disclosure>

      <Show when={props.onCancel}>
        {(cancel) => (
          <button type="button" class={styles.quiet} onClick={() => cancel()()}>
            Cancel
          </button>
        )}
      </Show>
    </div>
  );
}

/** One credential mode the plain link route accepts. */
interface CredentialField {
  kind: HarnessCredentialInput["kind"];
  /** The field's label, which is also the button's subject. */
  label: string;
  /** The sentence under the field. */
  hint: JSX.Element;
  /** What the account is called once linked. */
  accountLabel: string;
  /** What the button says, so two ways to link never share a name. */
  submitLabel: string;
}

/**
 * The credential form the API has always had: one secret, sealed on arrival.
 *
 * No account name is asked for. A user holds one account per harness, so a
 * name the user invents would distinguish it from nothing; the card is
 * labelled with what the credential *is* instead.
 */
function CredentialForm(props: { fields: readonly CredentialField[]; onLinked: () => unknown }) {
  const [selected, setSelected] = createSignal(0);
  const [secret, setSecret] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);

  const field = () => props.fields[selected()] ?? props.fields[0];

  function credential(kind: CredentialField["kind"], value: string): HarnessCredentialInput {
    switch (kind) {
      case "claude_setup_token":
        return { kind, token: value };
      case "claude_api_key":
      case "codex_api_key":
        return { kind, key: value };
      case "claude_oauth":
      case "codex_oauth":
        // Minted by the sign-in flows above, never typed by hand.
        throw new Error("an OAuth grant is not a credential anybody pastes");
    }
  }

  async function submit(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    const chosen = field();
    if (chosen === undefined || secret().trim() === "") {
      return;
    }

    setBusy(true);
    setError(null);
    try {
      await linkHarnessAccount({
        label: chosen.accountLabel,
        credential: credential(chosen.kind, secret().trim()),
      });
      setSecret("");
      await props.onLinked();
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  return (
    <form class={styles.form} onSubmit={(event) => void submit(event)}>
      <Show when={props.fields.length > 1}>
        <div class={styles.segmented} role="group" aria-label="Credential">
          <For each={props.fields}>
            {(option, index) => (
              <button
                type="button"
                class={cx(styles.segment, selected() === index() && styles.segmentOn)}
                aria-pressed={selected() === index()}
                onClick={() => {
                  setSelected(index());
                  setSecret("");
                }}
              >
                {option.label}
              </button>
            )}
          </For>
        </div>
      </Show>

      <div class={styles.field}>
        <label for={`harness-secret-${field()?.kind ?? "credential"}`}>{field()?.label}</label>
        <input
          id={`harness-secret-${field()?.kind ?? "credential"}`}
          class={styles.mono}
          type="password"
          value={secret()}
          onInput={(event) => setSecret(event.currentTarget.value)}
          autocomplete="off"
        />
        <p class={styles.hint}>{field()?.hint}</p>
      </div>

      <ProblemNotice error={error()} />
      <div class={styles.actions}>
        <button type="submit" class={styles.pill} disabled={busy() || secret().trim() === ""}>
          {busy() ? "Linking…" : (field()?.submitLabel ?? "Link this account")}
        </button>
      </div>
    </form>
  );
}
