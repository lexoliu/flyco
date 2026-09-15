/**
 * The consent road for Azure and Google Cloud (docs/ux.md §4 C5, C6).
 *
 * Both vendors offer the screen GitHub offers: their own sign-in, a list
 * of what flyco asks for, one approval. Flyco opens it in a new tab and
 * waits here, polling the attempt until the vendor has sent the browser
 * back; then it asks which subscription or project, and creates its own
 * limited identity there. The user's sign-in is used for that one setup
 * and never kept, so nothing runs as them afterwards.
 *
 * The vendor's terminal in the browser stays reachable by a quiet link,
 * for a tenant whose administrator has switched consent off.
 */
import { Show, createSignal, onCleanup } from "solid-js";
import Logomark, { PROVIDER_MARK } from "../../Logomark";
import ProblemNotice from "../../ProblemNotice";
import { useReadiness } from "../../Readiness";
import {
  finishAzureOauth,
  finishCodespacesOauth,
  finishGcpOauth,
  linkCodespaces,
  pollProviderOauth,
  startProviderOauth,
} from "../../../api/client";
import { ApiProblem } from "../../../api/problem";
import type { CloudConsent, ConsentCloud } from "../../../lib/flow";
import { PROVIDER_LABEL } from "../../../lib/providers";
import type { PageComponent, Primary } from "../page";
import {
  ChoiceCards,
  QuietLink,
  Waiting,
  openInNewTab,
  type Choice,
} from "./shared";
import styles from "./pages.module.css";

/** How often the page asks whether the consent has come back. */
const POLL_MS = 2000;

/** What each vendor's consent screen is called, and what it hands over. */
const VENDOR: Record<
  ConsentCloud,
  {
    name: string;
    signIn: string;
    lede: string;
    choice: string;
    choiceLede: string;
  }
> = {
  azure: {
    name: "Microsoft",
    signIn: "Sign in with Microsoft",
    lede: "Microsoft's own sign-in page opens in a new tab. Use the account that owns your Azure subscription, and choose work or school if asked. Flyco creates its own limited identity in the subscription you pick and never keeps your sign-in.",
    choice: "Which subscription?",
    choiceLede: "Flyco creates its identity here and builds machines in it.",
  },
  gcp: {
    name: "Google",
    signIn: "Sign in with Google",
    lede: "Google's own sign-in page opens in a new tab. Flyco creates its own service account in the project you pick and never keeps your sign-in.",
    choice: "Which project?",
    choiceLede:
      "Flyco creates its service account here and builds machines in it.",
  },
  codespaces: {
    name: "GitHub",
    signIn: "Link GitHub Codespaces",
    lede: "The GitHub account you signed in with already carries the grant — linking is a click, not another sign-in. Flyco creates one private repository, flyco-sessions, to carry the session image; sessions run inside your account's own codespaces and spend its monthly free hours first.",
    // Codespaces' finish takes no choice; these fields go unread.
    choice: "",
    choiceLede: "",
  },
};

export const CloudSignIn: PageComponent<{
  id: "cloud-sign-in";
  provider: ConsentCloud;
}> = (props) => {
  const readiness = useReadiness();
  const vendor = VENDOR[props.page.provider];
  const [attempt, setAttempt] = createSignal<string | null>(null);
  const [failure, setFailure] = createSignal<unknown>(null);
  /** What the vendor said when the browser came back without a sign-in. */
  const [refusal, setRefusal] = createSignal<string | null>(null);
  /**
   * Whether the GitHub tab that just opened is widening an old grant: the
   * direct link answered `github-scope-missing`, so the sign-in predates
   * the codespace ask and the OAuth hop is what adds it.
   */
  const [widening, setWidening] = createSignal(false);

  let timer: ReturnType<typeof setTimeout> | undefined;
  let closed = false;
  onCleanup(() => {
    closed = true;
    clearTimeout(timer);
  });

  /** Asks once whether the vendor has sent the browser back; asks again if not. */
  async function ask(attemptId: string): Promise<void> {
    try {
      const progress = await pollProviderOauth(props.page.provider, attemptId);
      if (closed) {
        return;
      }
      if (progress.state === "authorized") {
        if (props.page.provider === "codespaces") {
          // Nothing to choose: the finish creates the environment
          // repository and links the account in one call.
          await finishCodespacesOauth(attemptId);
          await readiness.refresh();
          props.advance({});
          return;
        }
        const consent: CloudConsent = {
          attemptId,
          account: progress.account,
          choices: progress.choices,
        };
        props.advance({ cloudConsent: consent, cloudChoice: null });
        return;
      }
      if (progress.state === "failed") {
        // The vendor's tab has already said this; this tab is the one that
        // can act on it.
        setAttempt(null);
        setRefusal(progress.reason);
        return;
      }
      timer = setTimeout(() => void ask(attemptId), POLL_MS);
    } catch (error) {
      if (!closed) {
        setAttempt(null);
        setFailure(error);
      }
    }
  }

  const primary = (): Primary => {
    const open = attempt();
    if (open !== null) {
      return {
        label: "Next",
        disabled: "Finish signing in in the other tab to continue",
        onClick: () => undefined,
      };
    }
    return {
      label:
        failure() === null && refusal() === null ? vendor.signIn : "Try again",
      // A tab opens for the OAuth road; the Codespaces direct link is the
      // call itself, so its in-flight label names that.
      busy: props.page.provider === "codespaces" ? "Linking…" : "Opening…",
      disabled: null,
      onClick: async () => {
        setFailure(null);
        setRefusal(null);
        if (props.page.provider === "codespaces") {
          try {
            await linkCodespaces();
            await readiness.refresh();
            props.advance({});
            return;
          } catch (error) {
            if (
              !(error instanceof ApiProblem) ||
              !error.type.endsWith("/github-scope-missing")
            ) {
              setFailure(error);
              return;
            }
            // The sign-in grant predates the codespace ask — the OAuth hop
            // below is what adds the scope to it.
            setWidening(true);
          }
        }
        const started = await startProviderOauth(props.page.provider);
        openInNewTab(started.authorize_url);
        setAttempt(started.attempt_id);
        timer = setTimeout(() => void ask(started.attempt_id), POLL_MS);
      },
    };
  };

  return {
    title: vendor.signIn,
    body: (
      <>
        <p class={styles.lede}>{vendor.lede}</p>
        <Show when={widening()}>
          <p class={styles.hint}>
            This sign-in predates the Codespaces grant — the GitHub tab adds
            it.
          </p>
        </Show>
        <Show when={attempt() !== null}>
          <Waiting>Waiting for you to finish in the other tab…</Waiting>
        </Show>
        <Show when={failure()}>
          {(error) => <ProblemNotice error={error()} />}
        </Show>
        <Show when={refusal()}>
          {(reason) => (
            <>
              <p class={styles.error} role="alert">
                {vendor.name} did not grant the sign-in: {reason()}
              </p>
              <Show when={props.page.provider !== "codespaces"}>
                <p class={styles.hint}>
                  If your organization needs an administrator to approve apps
                  like flyco, Cloud Shell needs no approval.
                </p>
              </Show>
            </>
          )}
        </Show>
        <Show when={props.page.provider !== "codespaces"}>
          <QuietLink
            onClick={() =>
              props.advance({
                cloudRoute: "cloud-shell",
                cloudConsent: null,
                cloudChoice: null,
              })
            }
          >
            Use Cloud Shell instead
          </QuietLink>
        </Show>
      </>
    ),
    primary,
  };
};

export const CloudChoice: PageComponent<{
  id: "cloud-choice";
  provider: ConsentCloud;
}> = (props) => {
  const readiness = useReadiness();
  const vendor = VENDOR[props.page.provider];
  const consent = props.state().answers.cloudConsent;
  if (consent === null) {
    throw new Error(
      "the choice page was reached without a consent to choose from",
    );
  }
  const [chosen, setChosen] = createSignal<string | null>(
    props.state().answers.cloudChoice,
  );

  const choices: Choice<string>[] = consent.choices.map((choice) => ({
    kind: choice.id,
    title: choice.name,
    line: choice.id,
    linked: false,
    mark: (
      <Show when={PROVIDER_MARK[props.page.provider]}>
        {(mark) => <Logomark mark={mark()} size={18} />}
      </Show>
    ),
  }));

  const primary = (): Primary => {
    const choice = chosen();
    const missing =
      choice === null
        ? `Choose a ${props.page.provider === "azure" ? "subscription" : "project"} to continue`
        : null;
    // Nothing is left to ask either vendor: the control plane creates the
    // identity, mints the machine key, and links.
    return {
      label: `Link ${PROVIDER_LABEL[props.page.provider]}`,
      busy: "Linking…",
      disabled: missing,
      onClick: async () => {
        if (choice === null) {
          return;
        }
        if (props.page.provider === "azure") {
          await finishAzureOauth(consent.attemptId, {
            subscription_id: choice,
          });
        } else {
          await finishGcpOauth(consent.attemptId, { project_id: choice });
        }
        await readiness.refresh();
        props.advance({ cloudChoice: choice });
      },
    };
  };

  return {
    title: vendor.choice,
    body: (
      <>
        <p class={styles.lede}>
          Signed in as <strong>{consent.account}</strong>. {vendor.choiceLede}
        </p>
        <ChoiceCards
          question={vendor.choice}
          choices={choices}
          value={chosen()}
          onChange={setChosen}
        />
      </>
    ),
    primary,
  };
};
