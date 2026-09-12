/**
 * Which model the agent runs (docs/ux.md §5, §9.3).
 *
 * One chip in two places — the home composer, where it decides what a
 * session opens on, and the session composer, where changing it reaches
 * the running agent — because the official apps put it in exactly that
 * spot and a person who has used either looks there for it.
 *
 * The list is the harness's own, served per linked account, and the chip
 * shows the model's display name rather than its id: `Fable`, not
 * `claude-fable-5-1[1m]`. Effort is the neighbouring chip's business —
 * the levels are the model's, so the two controls travel together, but a
 * list and a scale are different questions asked in different panels.
 */
import { For, Show } from "solid-js";
import { Check } from "lucide-solid";
import Popover from "./Popover";
import type { ModelChoice, ModelOption } from "../api/client";
import { cx } from "../lib/cx";
import { modelLabel } from "../lib/models";
import composer from "./Composer.module.css";
import styles from "./ModelChip.module.css";

export interface ModelChipProps {
  /** The models the agent offers. */
  models: readonly ModelOption[];
  /** What is chosen now. */
  choice: ModelChoice;
  /**
   * The new choice, on a change of the model.
   *
   * A new model starts on its own default effort rather than carrying the
   * old one over: the levels are the model's, and `max` on one is not a
   * level the next necessarily has.
   */
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
          title="Model"
        >
          <span class={composer.chipLabel}>{modelLabel(props.models, props.choice)}</span>
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
                    onClick={() => {
                      props.onChoose({ model: option.id });
                      close();
                    }}
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
        </div>
      )}
    </Popover>
  );
}
