/**
 * Claude Code's two sign-in pages (docs/ux.md §4 B2, B3).
 *
 * Both vendors' flows are two steps because both are: flyco opens the page
 * it was given, and a code travels between the two screens. Anthropic's
 * travels *back* — the authorize page shows `CODE#STATE` and the user
 * brings it here — which is why this pair ends in a field.
 */
import { Show, createSignal } from "solid-js";
import ProblemNotice from "../../ProblemNotice";
import { useReadiness } from "../../Readiness";
import { completeClaudeOauth, startClaudeOauth } from "../../../api/client";
import { parsePastedCode, pastedCodeForExchange } from "../../../lib/claudeCode";
import type { PageComponent, Primary } from "../page";
import { QuietLink, openInNewTab } from "./shared";
import styles from "./pages.module.css";

export const ClaudeSignIn: PageComponent<{ id: "claude-sign-in" }> = (props) => ({
  title: "Sign in at Anthropic",
  body: (
    <>
      <p class={styles.lede}>
        Flyco opens Anthropic's own sign-in page. Your password never reaches flyco.
      </p>
      <QuietLink onClick={() => props.advance({ agentRoute: "api-key" })}>
        Use an API key instead
      </QuietLink>
    </>
  ),
  primary: () => ({
    label: "Sign in with Claude",
    busy: "Opening…",
    disabled: null,
    onClick: async () => {
      const started = await startClaudeOauth();
      openInNewTab(started.authorize_url);
      props.advance({ agentRoute: "sign-in", claudeAttempt: started });
    },
  }),
});

export const ClaudePaste: PageComponent<{ id: "claude-paste" }> = (props) => {
  const readiness = useReadiness();
  const attempt = props.state().answers.claudeAttempt;
  if (attempt === null) {
    throw new Error("the paste page was reached without a Claude sign-in to redeem against");
  }
  const [pasted, setPasted] = createSignal("");
  const [refusal, setRefusal] = createSignal<unknown>(null);

  const code = () => parsePastedCode(pasted());

  const primary = (): Primary => {
    const parsed = code();
    return {
      label: "Link Claude Code",
      busy: "Linking…",
      disabled: parsed === null ? "Paste the code Anthropic showed you to continue" : null,
      onClick: async () => {
        if (parsed === null) {
          return;
        }
        setRefusal(null);
        try {
          const account = await completeClaudeOauth({
            attempt_id: attempt.attempt_id,
            code: pastedCodeForExchange(parsed),
          });
          await readiness.refresh();
          props.advance({ agentAccount: account });
        } catch (error) {
          // Under the field rather than above the footer: the code is what
          // was refused, and the way out is to paste it again.
          setRefusal(error);
        }
      },
    };
  };

  return {
    title: "Paste the code Anthropic shows you",
    body: (
      <>
        <div class={styles.field}>
          <label for="claude-oauth-code">Code from Anthropic</label>
          <input
            id="claude-oauth-code"
            class={`${styles.input} ${styles.mono}`}
            value={pasted()}
            onInput={(event) => setPasted(event.currentTarget.value)}
            autocomplete="off"
            spellcheck={false}
            autofocus
          />
          <p class={styles.hint}>
            It looks like <code>CODE#STATE</code> — paste the whole thing.
          </p>
          <Show when={refusal()}>
            <ProblemNotice error={refusal()} />
          </Show>
        </div>
        <QuietLink onClick={() => openInNewTab(attempt.authorize_url)}>
          Open the sign-in page again
        </QuietLink>
      </>
    ),
    primary,
  };
};
