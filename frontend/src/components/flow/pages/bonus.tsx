/**
 * The bonus programmes, one question a page (docs/ux.md §4 C2–C4, §7.1).
 *
 * Two questions, because two questions are what separate the free-credit
 * programmes worth telling somebody about from the ones that would waste
 * their time; they come before the credential because a user who signs up
 * through a programme runs their first month for nothing, and finding that
 * out after linking a card is finding out too late.
 *
 * The lookup runs when the second answer is given — `Next` on the student
 * page is what asks `POST /v1/providers/quickstart` — and the credit page
 * exists only when something matched. Nobody is asked to sign up.
 */
import { For, Show, createSignal } from "solid-js";
import YesNo from "../../YesNo";
import { providerQuickstart } from "../../../api/client";
import type { CloudKind, Page } from "../../../lib/flow";
import { formatUsd } from "../../../lib/money";
import { PROVIDER_LABEL } from "../../../lib/providers";
import { NEXT, type PageComponent, type Primary } from "../page";
import { ExternalLink } from "./shared";
import styles from "./pages.module.css";

export const NewToProvider: PageComponent<{ id: "new-to-provider"; provider: CloudKind }> = (
  props,
) => {
  const question = `New to ${PROVIDER_LABEL[props.page.provider]}?`;
  const [answer, setAnswer] = createSignal(props.state().answers.newToProvider);

  const primary = (): Primary => {
    const newToProvider = answer();
    return {
      label: "Next",
      disabled: newToProvider === null ? "Answer the question to continue" : null,
      onClick: () => {
        if (newToProvider !== null) {
          props.advance({ newToProvider });
        }
      },
    };
  };

  return {
    title: question,
    body: (
      <>
        <p class={styles.lede}>
          Most clouds give a new account credit to start with. Flyco only asks so it can tell you
          about a programme you qualify for.
        </p>
        <YesNo question={question} value={answer()} onChange={setAnswer} questionShown={false} />
      </>
    ),
    primary,
  };
};

export const Student: PageComponent<{ id: "student"; provider: CloudKind }> = (props) => {
  const question = "Are you a student?";
  const [answer, setAnswer] = createSignal(props.state().answers.student);

  const primary = (): Primary => {
    const student = answer();
    return {
      label: "Next",
      busy: "Checking…",
      disabled: student === null ? "Answer the question to continue" : null,
      onClick: async () => {
        const newToProvider = props.state().answers.newToProvider;
        if (student === null || newToProvider === null) {
          throw new Error("the student page was reached before the newcomer question was answered");
        }
        const programmes = await providerQuickstart({
          new_to_provider: newToProvider,
          is_student: student,
        });
        props.advance({ student, programmes });
      },
    };
  };

  return {
    title: question,
    body: (
      <>
        <p class={styles.lede}>
          Education programmes unlock credit a general account does not get.
        </p>
        <YesNo question={question} value={answer()} onChange={setAnswer} questionShown={false} />
      </>
    ),
    primary,
  };
};

export const Credit: PageComponent<Extract<Page, { id: "credit" }>> = (props) => ({
  title: `${PROVIDER_LABEL[props.page.provider]} gives you credit`,
  body: (
    <>
      <p class={styles.lede}>
        Your answers match {props.page.programmes.length === 1 ? "a programme" : "programmes"} worth
        signing up for before you link the account. Linking works either way.
      </p>
      <ul class={styles.programmes}>
        <For each={props.page.programmes}>
          {(programme) => (
            <li class={styles.programme}>
              <div class={styles.programmeText}>
                <p class={styles.programmeTitle}>
                  {programme.title}
                  <Show when={programme.credit !== null && programme.credit !== undefined}>
                    <span class={styles.credit}>{formatUsd(programme.credit ?? 0)}</span>
                  </Show>
                </p>
                <p class={styles.hint}>{programme.detail}</p>
              </div>
              <ExternalLink href={programme.url}>Sign up</ExternalLink>
            </li>
          )}
        </For>
      </ul>
    </>
  ),
  primary: () => NEXT(() => props.advance()),
});
