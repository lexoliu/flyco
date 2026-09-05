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
  finishGcpOauth,
  pollProviderOauth,
  startProviderOauth,
} from "../../../api/client";
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
  { signIn: string; lede: string; choice: string; choiceLede: string }
> = {
  azure: {
    signIn: "Sign in with Microsoft",
    lede: "Microsoft's own sign-in page opens in a new tab. Flyco creates its own limited identity in the subscription you pick and never keeps your password or your sign-in.",
    choice: "Which subscription?",
    choiceLede: "Flyco creates its identity here and builds machines in it.",
  },
  gcp: {
    signIn: "Sign in with Google",
    lede: "Google's own sign-in page opens in a new tab. Flyco creates its own service account in the project you pick and never keeps your password or your sign-in.",
    choice: "Which project?",
    choiceLede:
      "Flyco creates its service account here and builds machines in it.",
  },
};

export const CloudSignIn: PageComponent<{
  id: "cloud-sign-in";
  provider: ConsentCloud;
}> = (props) => {
  const vendor = VENDOR[props.page.provider];
  const [attempt, setAttempt] = createSignal<string | null>(null);
  const [failure, setFailure] = createSignal<unknown>(null);

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
        const consent: CloudConsent = {
          attemptId,
          account: progress.account,
          choices: progress.choices,
        };
        props.advance({ cloudConsent: consent, cloudChoice: null });
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
      label: failure() === null ? vendor.signIn : "Try again",
      busy: "Opening…",
      disabled: null,
      onClick: async () => {
        setFailure(null);
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
        <Show when={attempt() !== null}>
          <Waiting>Waiting for you to finish in the other tab…</Waiting>
        </Show>
        <Show when={failure()}>
          {(error) => <ProblemNotice error={error()} />}
        </Show>
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
    // Azure's key page follows; Google Cloud has nothing left to ask.
    if (props.page.provider === "azure") {
      return {
        label: "Next",
        disabled: missing,
        onClick: () => {
          if (choice !== null) {
            props.advance({ cloudChoice: choice });
          }
        },
      };
    }
    return {
      label: `Link ${PROVIDER_LABEL.gcp}`,
      busy: "Linking…",
      disabled: missing,
      onClick: async () => {
        if (choice === null) {
          return;
        }
        await finishGcpOauth(consent.attemptId, { project_id: choice });
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
