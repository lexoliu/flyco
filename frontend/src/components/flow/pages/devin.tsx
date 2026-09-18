/**
 * Devin's sign-in pages (docs/ux.md §4 B2).
 *
 * Reached from the agents list, or alone when a settings card names Devin.
 * Two pages for the same reason Claude's are two: flyco opens the page it
 * was given, and a code travels between the two screens. Devin's OAuth
 * client admits only localhost redirect addresses, so flyco runs the
 * CLI's port-free flow instead, and Devin's page shows the code after
 * sign-in, which is what the paste field takes.
 */
import { Show, createSignal } from "solid-js";
import ProblemNotice from "../../ProblemNotice";
import { useReadiness } from "../../Readiness";
import { completeDevinOauth, startDevinOauth } from "../../../api/client";
import { pastedDevinCode } from "../../../lib/devinOauth";
import type { PageComponent, Primary } from "../page";
import { QuietLink, openInNewTab } from "./shared";
import styles from "./pages.module.css";

export const DevinSignIn: PageComponent<{ id: "devin-sign-in" }> = (props) => {
  const routes = () => props.state().answers.routes;

  const primary = (): Primary => ({
    label: "Sign in with Devin",
    busy: "Opening…",
    disabled: null,
    onClick: async () => {
      const started = await startDevinOauth();
      openInNewTab(started.authorize_url);
      props.advance({
        routes: { ...routes(), devin: "sign-in" },
        devinAttempt: started,
      });
    },
  });

  return {
    title: "Link Devin",
    body: (
      <>
        <p class={styles.lede}>
          Runs on your Devin account. Flyco opens Devin's own sign-in page;
          your password never reaches flyco.
        </p>
        <QuietLink
          onClick={() =>
            props.advance({
              routes: { ...routes(), devin: "api-key" },
              devinAttempt: null,
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

export const DevinPaste: PageComponent<{ id: "devin-paste" }> = (props) => {
  const readiness = useReadiness();
  /** The sign-in the paste redeems against: the one the page opened with, or a fresh one. */
  const attempt = () => {
    const open = props.state().answers.devinAttempt;
    if (open === null) {
      throw new Error(
        "the paste page was reached without a Devin sign-in to redeem against",
      );
    }
    return open;
  };
  const [pasted, setPasted] = createSignal("");
  const [refusal, setRefusal] = createSignal<unknown>(null);

  const code = () => pastedDevinCode(pasted());

  /**
   * A new sign-in, in place: a code that was refused, or spent by an
   * exchange that then failed, is not coming back, and the way out is a
   * new code — which needs a new attempt, since the old one's state has
   * been used. The page stays; the attempt under it changes.
   */
  async function restart(): Promise<void> {
    const started = await startDevinOauth();
    setPasted("");
    setRefusal(null);
    props.record({ devinAttempt: started });
    openInNewTab(started.authorize_url);
  }

  const primary = (): Primary => {
    const parsed = code();
    return {
      label: "Link Devin",
      busy: "Linking…",
      disabled:
        parsed === null ? "Paste the code Devin showed you to continue" : null,
      onClick: async () => {
        if (parsed === null) {
          return;
        }
        setRefusal(null);
        try {
          const account = await completeDevinOauth({
            attempt_id: attempt().attempt_id,
            code: parsed,
          });
          await readiness.refresh();
          props.linked("devin", account);
        } catch (error) {
          // Under the field rather than above the footer: the paste is what
          // was refused, and the way out is to paste it again or start over.
          setRefusal(error);
        }
      },
    };
  };

  return {
    title: "Paste the code Devin shows you",
    body: (
      <>
        <p class={styles.lede}>
          After you sign in, Devin shows a code. Copy it and paste it here.
        </p>
        <div class={styles.field}>
          <label for="devin-oauth-code">Code from Devin</label>
          <input
            id="devin-oauth-code"
            class={`${styles.input} ${styles.mono}`}
            value={pasted()}
            onInput={(event) => setPasted(event.currentTarget.value)}
            autocomplete="off"
            spellcheck={false}
            autofocus
          />
          <Show when={refusal()}>
            <ProblemNotice error={refusal()} />
          </Show>
        </div>
        <QuietLink onClick={() => void restart()}>
          Start the sign-in again
        </QuietLink>
      </>
    ),
    primary,
  };
};
