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
import { providerQuickstart } from "../../../api/client";
import type { CloudKind, Page } from "../../../lib/flow";
import { formatUsd } from "../../../lib/money";
import { PROVIDER_LABEL } from "../../../lib/providers";
import { NEXT, type PageComponent, type Primary } from "../page";
import { ChoiceCards, ExternalLink, type Choice } from "./shared";
import styles from "./pages.module.css";

type Answer = "yes" | "no";

const toAnswer = (value: boolean | null): Answer | null =>
  value === null ? null : value ? "yes" : "no";
const fromAnswer = (value: Answer | null): boolean | null =>
  value === null ? null : value === "yes";

/**
 * Yes and No as the page's body: two full-width cards, each saying what
 * answering it means, because a question page's answer is the page — not
 * a control under it.
 */
function yesNo(yes: string, no: string): Choice<Answer>[] {
  return [
    { kind: "yes", title: "Yes", line: yes, linked: false },
    { kind: "no", title: "No", line: no, linked: false },
  ];
}

export const NewToProvider: PageComponent<{
  id: "new-to-provider";
  provider: CloudKind;
}> = (props) => {
  const provider = PROVIDER_LABEL[props.page.provider];
  const question = `New to ${provider}?`;
  const [answer, setAnswer] = createSignal(
    toAnswer(props.state().answers.newToProvider),
  );

  const primary = (): Primary => {
    const newToProvider = fromAnswer(answer());
    return {
      label: "Next",
      disabled:
        newToProvider === null ? "Answer the question to continue" : null,
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
          Most clouds give a new account credit to start with. Flyco only asks
          so it can tell you about a programme you qualify for.
        </p>
        <ChoiceCards
          question={question}
          choices={yesNo(
            `I have never used ${provider}.`,
            `I already use ${provider}.`,
          )}
          value={answer()}
          onChange={setAnswer}
        />
      </>
    ),
    primary,
  };
};

export const Student: PageComponent<{ id: "student"; provider: CloudKind }> = (
  props,
) => {
  const question = "Are you a student?";
  const [answer, setAnswer] = createSignal(
    toAnswer(props.state().answers.student),
  );

  const primary = (): Primary => {
    const student = fromAnswer(answer());
    return {
      label: "Next",
      busy: "Checking…",
      disabled: student === null ? "Answer the question to continue" : null,
      onClick: async () => {
        const newToProvider = props.state().answers.newToProvider;
        if (student === null || newToProvider === null) {
          throw new Error(
            "the student page was reached before the newcomer question was answered",
          );
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
        <ChoiceCards
          question={question}
          choices={yesNo(
            "I have a school email address to verify with.",
            "I am not studying right now.",
          )}
          value={answer()}
          onChange={setAnswer}
        />
      </>
    ),
    primary,
  };
};

export const Credit: PageComponent<Extract<Page, { id: "credit" }>> = (
  props,
) => ({
  title: `${PROVIDER_LABEL[props.page.provider]} gives you credit`,
  body: (
    <>
      <p class={styles.lede}>
        Your answers match{" "}
        {props.page.programmes.length === 1 ? "a programme" : "programmes"}{" "}
        worth signing up for before you link the account. Linking works either
        way.
      </p>
      <ul class={styles.programmes}>
        <For each={props.page.programmes}>
          {(programme) => (
            <li class={styles.programme}>
              <div class={styles.programmeText}>
                <p class={styles.programmeTitle}>
                  {programme.title}
                  <Show
                    when={
                      programme.credit !== null &&
                      programme.credit !== undefined
                    }
                  >
                    <span class={styles.credit}>
                      {formatUsd(programme.credit ?? 0)}
                    </span>
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
