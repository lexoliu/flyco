/**
 * Claude Code's pages (docs/ux.md §4 B1, B2).
 *
 * The first page links Claude Code or moves past it; nothing chooses an
 * agent here, because a task chooses its agent in the composer. Both
 * vendors' sign-ins are two steps because both are: flyco opens the page
 * it was given, and a code travels between the two screens. Anthropic's
 * travels *back* — the authorize page shows `CODE#STATE` and the user
 * brings it here — which is why this pair ends in a field.
 */
import { Show, createSignal } from "solid-js";
import Logomark, { HARNESS_MARK } from "../../Logomark";
import ProblemNotice from "../../ProblemNotice";
import { useReadiness } from "../../Readiness";
import { completeClaudeOauth, startClaudeOauth } from "../../../api/client";
import {
  parsePastedCode,
  pastedCodeForExchange,
} from "../../../lib/claudeCode";
import { NEXT, type PageComponent, type Primary } from "../page";
import { LinkedAgent, QuietLink, openInNewTab } from "./shared";
import styles from "./pages.module.css";

export const ClaudeSignIn: PageComponent<{ id: "claude-sign-in" }> = (
  props,
) => {
  const linked = () => props.state().answers.agents.claude_code;
  const routes = () => props.state().answers.routes;

  const primary = (): Primary =>
    linked() !== undefined
      ? NEXT(() => props.advance())
      : {
          label: "Sign in with Claude",
          busy: "Opening…",
          disabled: null,
          onClick: async () => {
            const started = await startClaudeOauth();
            openInNewTab(started.authorize_url);
            props.advance({
              routes: { ...routes(), claude_code: "sign-in" },
              claudeAttempt: started,
            });
          },
        };

  return {
    title: "Link Claude Code",
    body: (
      <Show
        when={linked()}
        fallback={
          <>
            <p class={styles.lede}>
              Runs on your Claude subscription. Flyco opens Anthropic's own
              sign-in page; your password never reaches flyco.
            </p>
            <QuietLink
              onClick={() =>
                props.advance({
                  routes: { ...routes(), claude_code: "api-key" },
                  claudeAttempt: null,
                })
              }
            >
              Use an API key instead
            </QuietLink>
            <QuietLink
              onClick={() =>
                props.advance({
                  routes: { ...routes(), claude_code: "sign-in" },
                  claudeAttempt: null,
                })
              }
            >
              I don't use Claude Code
            </QuietLink>
          </>
        }
      >
        {(account) => (
          <>
            <LinkedAgent
              mark={<Logomark mark={HARNESS_MARK.claude_code} size={18} />}
              label={account().label}
              linkedAtUnix={account().linked_at_unix}
            />
            <p class={styles.hint}>
              Sessions can use it now. Settings › Agents relinks it.
            </p>
          </>
        )}
      </Show>
    ),
    primary,
  };
};

export const ClaudePaste: PageComponent<{ id: "claude-paste" }> = (props) => {
  const readiness = useReadiness();
  const attempt = props.state().answers.claudeAttempt;
  if (attempt === null) {
    throw new Error(
      "the paste page was reached without a Claude sign-in to redeem against",
    );
  }
  const [pasted, setPasted] = createSignal("");
  const [refusal, setRefusal] = createSignal<unknown>(null);

  const code = () => parsePastedCode(pasted());

  const primary = (): Primary => {
    const parsed = code();
    return {
      label: "Link Claude Code",
      busy: "Linking…",
      disabled:
        parsed === null
          ? "Paste the code Anthropic showed you to continue"
          : null,
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
          props.advance({
            agents: { ...props.state().answers.agents, claude_code: account },
          });
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
