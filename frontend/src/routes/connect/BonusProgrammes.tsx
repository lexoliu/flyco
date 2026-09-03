/**
 * The first screen of every cloud wizard (docs/ux.md §7.1).
 *
 * Two questions, because two questions are what separate the free-credit
 * programmes worth telling somebody about from the ones that would waste
 * their time. It is the first screen rather than a footnote for a reason: a
 * user who signs up through a credit programme runs their first month for
 * nothing, and finding that out after linking a card is finding out too
 * late.
 *
 * Continuing without signing up is always available. This is an offer, not a
 * gate.
 */
import { For, Show, createSignal } from "solid-js";
import { createQuery } from "../../lib/query";
import { ArrowRight, ExternalLink } from "lucide-solid";
import ProblemNotice from "../../components/ProblemNotice";
import YesNo from "../../components/YesNo";
import { providerQuickstart, type CloudProviderKind } from "../../api/client";
import { formatUsd } from "../../lib/money";
import styles from "./Connect.module.css";

export interface BonusProgrammesProps {
  /** Which provider's programmes to show. */
  provider: CloudProviderKind;
  /** Moves to the credential step. */
  onContinue: () => void;
}

export default function BonusProgrammes(props: BonusProgrammesProps) {
  const [newToProvider, setNewToProvider] = createSignal<boolean | null>(null);
  const [isStudent, setIsStudent] = createSignal<boolean | null>(null);

  /** Both answers, once both are given; the lookup waits until then. */
  const answers = () => {
    const newcomer = newToProvider();
    const student = isStudent();
    return newcomer === null || student === null
      ? null
      : { new_to_provider: newcomer, is_student: student };
  };

  const [hints] = createQuery(answers, providerQuickstart);

  const matching = () =>
    (hints() ?? []).filter((hint) => hint.provider === props.provider);

  return (
    <div class={styles.step}>
      <div class={styles.questions}>
        <YesNo
          question="New to this provider?"
          value={newToProvider()}
          onChange={setNewToProvider}
        />
        <YesNo question="Are you a student?" value={isStudent()} onChange={setIsStudent} />
      </div>

      <ProblemNotice error={hints.error} />

      <Show
        when={matching().length > 0}
        fallback={
          <p class={styles.hint}>
            {answers() === null
              ? "Answer both questions to see the credit this provider offers."
              : "No credit programme matches those answers. Linking an account still works."}
          </p>
        }
      >
        <ul class={styles.programmes}>
          <For each={matching()}>
            {(hint) => (
              <li class={styles.programme}>
                <div>
                  <p class={styles.programmeTitle}>
                    {hint.title}
                    <Show when={hint.credit !== null && hint.credit !== undefined}>
                      <span class={styles.credit}>{formatUsd(hint.credit ?? 0)}</span>
                    </Show>
                  </p>
                  <p class={styles.programmeDetail}>{hint.detail}</p>
                </div>
                <a class={styles.pill} href={hint.url} target="_blank" rel="noreferrer noopener">
                  Sign up
                  <ExternalLink size={13} aria-hidden="true" />
                </a>
              </li>
            )}
          </For>
        </ul>
      </Show>

      <button type="button" class={styles.primary} onClick={props.onContinue}>
        Continue
        <ArrowRight size={14} aria-hidden="true" />
      </button>
    </div>
  );
}
