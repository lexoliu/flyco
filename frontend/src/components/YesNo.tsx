/**
 * One yes-or-no question, answered by choosing, not by flipping a switch.
 *
 * A switch says "on"; a question wants an answer, and "unanswered" is a real
 * state the caller must be able to see, so the value is `null` until the
 * user picks. Rendered as a radio group so the two options are keyboard
 * navigable together and read as one question by assistive technology.
 */
import { For, createUniqueId } from "solid-js";
import { cx } from "../lib/cx";
import styles from "./YesNo.module.css";

export interface YesNoProps {
  /** The question, shown as the group's legend. */
  question: string;
  /** `null` while unanswered. */
  value: boolean | null;
  onChange: (answer: boolean) => void;
  /**
   * Whether the question is shown, or only read out.
   *
   * A page whose title *is* the question would ask it twice; the legend
   * still names the group for assistive technology.
   */
  questionShown?: boolean | undefined;
}

const OPTIONS: readonly { value: boolean; label: string }[] = [
  { value: true, label: "Yes" },
  { value: false, label: "No" },
];

export default function YesNo(props: YesNoProps) {
  const name = createUniqueId();
  return (
    <fieldset class={styles.group}>
      <legend class={cx(styles.question, props.questionShown === false && styles.questionHidden)}>
        {props.question}
      </legend>
      <div class={styles.options} role="radiogroup" aria-label={props.question}>
        <For each={OPTIONS}>
          {(option) => (
            <label class={cx(styles.option, props.value === option.value && styles.chosen)}>
              <input
                class={styles.input}
                type="radio"
                name={name}
                value={String(option.value)}
                checked={props.value === option.value}
                onChange={() => props.onChange(option.value)}
              />
              {option.label}
            </label>
          )}
        </For>
      </div>
    </fieldset>
  );
}
