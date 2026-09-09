/**
 * Which model the agent runs, and at what effort (docs/ux.md §5, §9.3).
 *
 * One chip in two places — the home composer, where it decides what a
 * session opens on, and the session composer, where changing it reaches
 * the running agent — because the official apps put it in exactly that
 * spot and a person who has used either looks there for it.
 *
 * The list is the harness's own, served per linked account, and the chip
 * shows the model's display name rather than its id: `Fable · High`, not
 * `claude-fable-5-1[1m]`. Effort sits under the model in the same panel
 * because it is a property of the model — not every one accepts it, and
 * the levels differ — so choosing a model first is what makes the effort
 * row make sense.
 */
import { For, Show, createMemo } from "solid-js";
import { Check } from "lucide-solid";
import Popover from "./Popover";
import type { ModelChoice, ModelOption } from "../api/client";
import { cx } from "../lib/cx";
import { choiceLabel, effortLabel, optionOf } from "../lib/models";
import composer from "./Composer.module.css";
import styles from "./ModelChip.module.css";

export interface ModelChipProps {
  /** The models the agent offers. */
  models: readonly ModelOption[];
  /** What is chosen now. */
  choice: ModelChoice;
  /** The new choice, on every change of the model or the effort. */
  onChoose: (choice: ModelChoice) => void;
  /**
   * Whether a change is still on its way to the agent.
   *
   * The session composer's chip costs a round trip; the home composer's is
   * a field of a form and never waits. While it waits the chip dims rather
   * than disables, so the reading stays legible.
   */
  saving?: boolean | undefined;
  /** Which edge of the chip the panel lines up with. */
  align?: "start" | "end" | undefined;
}

export default function ModelChip(props: ModelChipProps) {
  const chosen = createMemo(() => optionOf(props.models, props.choice));
  const efforts = createMemo(() => chosen()?.efforts ?? []);

  /** What the effort row's `Default` pill stands for, when the harness says. */
  const defaultEffort = createMemo(() => chosen()?.default_effort ?? null);

  function chooseModel(option: ModelOption, close: () => void): void {
    // A new model starts on its own default effort rather than carrying
    // the old one over: the levels are the model's, and `max` on one is
    // not a level the next necessarily has.
    props.onChoose({ model: option.id });
    if (option.efforts.length === 0) {
      close();
    }
  }

  function chooseEffort(effort: string | null): void {
    props.onChoose(effort === null ? { model: props.choice.model } : { model: props.choice.model, effort });
  }

  return (
    <Popover
      label="Model"
      align={props.align}
      panelClass={styles.panel}
      trigger={(attrs) => (
        <button
          id={attrs.id}
          onClick={attrs.onClick}
          aria-expanded={attrs.expanded()}
          aria-haspopup="dialog"
          type="button"
          class={cx(composer.chip, props.saving === true && styles.saving)}
          title="Model and effort"
        >
          <span class={composer.chipLabel}>{choiceLabel(props.models, props.choice)}</span>
        </button>
      )}
    >
      {(close) => (
        <div class={composer.popover}>
          <p class={composer.popoverTitle}>Model</p>
          <ul class={composer.options} role="listbox" aria-label="Model">
            <For each={props.models}>
              {(option) => (
                <li>
                  <button
                    type="button"
                    role="option"
                    aria-selected={option.id === props.choice.model}
                    class={cx(
                      composer.option,
                      styles.model,
                      option.id === props.choice.model && composer.optionChosen,
                    )}
                    onClick={() => chooseModel(option, close)}
                  >
                    <span class={styles.modelText}>
                      <span class={styles.modelName}>{option.label}</span>
                      <span class={styles.modelDescription}>{option.description}</span>
                    </span>
                    <Show when={option.id === props.choice.model}>
                      <Check size={14} class={cx(styles.check)} aria-hidden="true" />
                    </Show>
                  </button>
                </li>
              )}
            </For>
          </ul>
          <Show when={efforts().length > 0}>
            <p class={composer.popoverTitle}>Effort</p>
            <div class={styles.efforts} role="group" aria-label="Effort">
              <button
                type="button"
                class={cx(styles.effort, (props.choice.effort ?? null) === null && styles.effortChosen)}
                aria-pressed={(props.choice.effort ?? null) === null}
                onClick={() => chooseEffort(null)}
              >
                Default
                <Show when={defaultEffort()}>
                  {(level) => <span class={styles.effortNote}>{effortLabel(level())}</span>}
                </Show>
              </button>
              <For each={efforts()}>
                {(level) => (
                  <button
                    type="button"
                    class={cx(styles.effort, props.choice.effort === level && styles.effortChosen)}
                    aria-pressed={props.choice.effort === level}
                    onClick={() => chooseEffort(level)}
                  >
                    {effortLabel(level)}
                  </button>
                )}
              </For>
            </div>
          </Show>
        </div>
      )}
    </Popover>
  );
}
