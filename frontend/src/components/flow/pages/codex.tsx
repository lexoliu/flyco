/**
 * Codex's sign-in page (docs/ux.md §4 B2′): the device-code flow
 * `codex login --device-auth` runs, on one page.
 *
 * OpenAI's browser flow ends at a localhost callback a hosted web app
 * cannot offer, so the code is the transport: the page asks OpenAI for one
 * as it opens, shows it, links to the page it is typed on, and waits —
 * polling the attempt every `interval_seconds` until it is approved or
 * runs out. Approval advances the flow by itself; nothing on the page is
 * pressed for that.
 *
 * The waiting is the state machine in `src/lib/codexDevice.ts`, because it
 * has real outcomes the page has to render: an approval, a code that went
 * stale (the primary becomes `Get a new code`), and OpenAI refusing to
 * start a device sign-in at all because the account has the flow switched
 * off — the one failure the person can fix themselves, so it gets
 * instructions and a link instead of an error.
 */
import { Show, createSignal, onCleanup, onMount } from "solid-js";
import CopyButton from "../../CopyButton";
import ProblemNotice from "../../ProblemNotice";
import { useReadiness } from "../../Readiness";
import { pollCodexOauth, startCodexOauth } from "../../../api/client";
import {
  CHATGPT_SECURITY_SETTINGS_URL,
  IDLE,
  nextSignIn,
  pollDelayMs,
  type CodexAttempt,
  type CodexEvent,
  type CodexSignIn as SignIn,
} from "../../../lib/codexDevice";
import type { PageComponent, Primary } from "../page";
import { ExternalLink, QuietLink, Waiting } from "./shared";
import styles from "./pages.module.css";

export const CodexSignIn: PageComponent<{ id: "codex-sign-in" }> = (props) => {
  const readiness = useReadiness();
  const [signIn, setSignIn] = createSignal<SignIn>(IDLE);

  let timer: ReturnType<typeof setTimeout> | undefined;
  let closed = false;

  // A poll already in flight still resolves after the page goes away; the
  // flag is what stops it writing to a signal nobody is reading.
  onCleanup(() => {
    closed = true;
    clearTimeout(timer);
  });

  function transition(event: CodexEvent): SignIn {
    const next = nextSignIn(signIn(), event);
    setSignIn(next);
    return next;
  }

  function schedule(state: SignIn, attempt: CodexAttempt): void {
    const delay = pollDelayMs(state);
    if (delay === null || closed) {
      return;
    }
    timer = setTimeout(() => void ask(attempt), delay);
  }

  async function ask(attempt: CodexAttempt): Promise<void> {
    try {
      const outcome = await pollCodexOauth(attempt.attemptId);
      if (closed) {
        return;
      }
      if (outcome.state === "linked") {
        transition({ kind: "linked" });
        await readiness.refresh();
        props.linked("codex", outcome.account);
        return;
      }
      schedule(transition({ kind: "pending" }), attempt);
    } catch (error) {
      if (!closed) {
        transition({ kind: "failed", error });
      }
    }
  }

  /** Asks OpenAI for a code and starts waiting on it. */
  async function requestCode(): Promise<void> {
    clearTimeout(timer);
    transition({ kind: "start" });
    try {
      const started = await startCodexOauth();
      if (closed) {
        return;
      }
      const attempt: CodexAttempt = {
        attemptId: started.attempt_id,
        userCode: started.user_code,
        verificationUrl: started.verification_url,
        intervalSeconds: started.interval_seconds,
      };
      schedule(transition({ kind: "started", attempt }), attempt);
    } catch (error) {
      if (!closed) {
        transition({ kind: "failed", error });
      }
    }
  }

  // Requested as the page opens: the code *is* the page, and a button whose
  // only job is to reveal the one thing on the screen would be a step
  // asking the user to confirm they meant to be here.
  onMount(() => {
    void requestCode();
  });

  const waiting = () =>
    signIn().step === "waiting"
      ? (signIn() as Extract<SignIn, { step: "waiting" }>)
      : null;
  const failed = () =>
    signIn().step === "failed"
      ? (signIn() as Extract<SignIn, { step: "failed" }>)
      : null;

  const primary = (): Primary => {
    switch (signIn().step) {
      case "expired":
        return {
          label: "Get a new code",
          busy: "Asking OpenAI…",
          disabled: null,
          onClick: requestCode,
        };
      case "blocked":
      case "failed":
        return {
          label: "Try again",
          busy: "Asking OpenAI…",
          disabled: null,
          onClick: requestCode,
        };
      case "idle":
      case "starting":
        return {
          label: "Next",
          disabled: "Asking OpenAI for a code…",
          onClick: () => undefined,
        };
      case "waiting":
      case "linked":
        return {
          label: "Next",
          disabled: "Approve the code in the browser to continue",
          onClick: () => undefined,
        };
    }
  };

  return {
    title: "Link Codex",
    body: (
      <>
        <Show when={waiting()}>
          {(active) => (
            <>
              <p class={styles.lede}>
                Runs on your ChatGPT subscription. Enter this code on OpenAI's
                page; your password never reaches flyco.
              </p>
              <div class={styles.codeRow}>
                <span class={styles.code} aria-label="One-time code">
                  {active().attempt.userCode}
                </span>
                <CopyButton
                  value={active().attempt.userCode}
                  label="Copy code"
                  class={styles.pill}
                />
              </div>
              <ExternalLink href={active().attempt.verificationUrl}>
                Open auth.openai.com/codex/device
              </ExternalLink>
              <Waiting>Waiting for you to approve in the browser…</Waiting>
            </>
          )}
        </Show>

        <Show when={signIn().step === "idle" || signIn().step === "starting"}>
          <p class={styles.lede}>
            Flyco asks OpenAI for a one-time code. Your password never reaches
            flyco.
          </p>
          <div
            class={`${styles.skeleton} ${styles.skeletonCommand}`}
            aria-label="Asking for a code"
          />
        </Show>

        <Show when={signIn().step === "expired"}>
          <p class={styles.lede}>
            That code expired before it was approved. Get a new one and try
            again.
          </p>
        </Show>

        <Show when={signIn().step === "blocked"}>
          <div class={styles.notice}>
            <p class={styles.hint}>
              OpenAI will not start a device sign-in for this account. Turn on{" "}
              <strong>device code authorization</strong> in your ChatGPT
              security settings — on a workspace account a workspace admin does
              it — and try again.
            </p>
            <ExternalLink href={CHATGPT_SECURITY_SETTINGS_URL}>
              Open ChatGPT security settings
            </ExternalLink>
          </div>
        </Show>

        <Show when={failed()}>
          {(failure) => <ProblemNotice error={failure().error} />}
        </Show>

        <QuietLink
          onClick={() =>
            props.advance({
              routes: { ...props.state().answers.routes, codex: "api-key" },
            })
          }
        >
          Use an API key instead
        </QuietLink>
      </>
    ),
    primary,
  };
};
