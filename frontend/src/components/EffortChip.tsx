/**
 * How hard the chosen model thinks, as a detented slider (docs/ux.md §5,
 * §9.3).
 *
 * Effort is a property of the model — not every one accepts it, and the
 * levels differ — so it is its own chip beside the model's rather than a
 * row inside the model's panel: the model picker is a list, this is a
 * scale, and the official apps keep the two apart for the same reason.
 *
 * The control is the {@link Detents} slider the machine picker already
 * teaches: stops you can count, the thumb on one of them. The leftmost
 * stop is `Default` — the model keeps the choice, the same shape `Auto`
 * has on the machine slider — because the harnesses do not all say which
 * level they start on, and parking the thumb on a level that is a guess
 * would be a lie the track has room to avoid.
 */
import { Show, createMemo } from "solid-js";
import Popover from "./Popover";
import Detents from "./Detents";
import type { ModelChoice, ModelOption } from "../api/client";
import { cx } from "../lib/cx";
import { effortLabel, optionOf, shortName } from "../lib/models";
import composer from "./Composer.module.css";
import styles from "./EffortChip.module.css";

/** What the leftmost detent is called: the model keeping the choice. */
const DEFAULT_NAME = "Default";

export interface EffortChipProps {
  /** The models the agent offers — effort levels belong to the chosen one. */
  models: readonly ModelOption[];
  /** What is chosen now. */
  choice: ModelChoice;
  /** The new choice, on every change of the effort. */
  onChoose: (choice: ModelChoice) => void;
  /** Whether a change is still on its way to the agent — dims, never disables. */
  saving?: boolean | undefined;
  /** Which edge of the chip the panel lines up with. */
  align?: "start" | "end" | undefined;
}

export default function EffortChip(props: EffortChipProps) {
  const chosen = createMemo(() => optionOf(props.models, props.choice));

  /** The model's own levels, in the order the harness stated them. */
  const efforts = createMemo(() => chosen()?.efforts ?? []);

  /**
   * The rail's stops: `Default` first — the model keeps the choice — then
   * the harness's levels. `null` is the stop that is not a level.
   */
  const stops = createMemo<readonly (string | null)[]>(() => [null, ...efforts()]);

  /** The stop the thumb sits on; a chosen effort the list lost parks at `Default`. */
  const position = createMemo(() => {
    const effort = props.choice.effort;
    if (effort === undefined || effort === null) {
      return 0;
    }
    return Math.max(0, stops().indexOf(effort));
  });

  /**
   * What the stop the thumb sits on is called: the level chosen, or
   * `Default` where the model keeps the choice — the same shape `Auto`
   * has in the machine slider's reading.
   */
  const reading = createMemo(() => {
    const effort = props.choice.effort;
    return effort === undefined || effort === null ? DEFAULT_NAME : effortLabel(effort);
  });

  /**
   * The line under the level: the model it belongs to, and — where the
   * harness says which level `Default` means — what the choice resolves to.
   */
  const model = createMemo(() => {
    const option = chosen();
    return option === undefined ? props.choice.model : shortName(option);
  });
  const subline = createMemo(() => {
    const fallback = chosen()?.default_effort;
    return props.choice.effort === undefined || props.choice.effort === null
      ? fallback === undefined || fallback === null
        ? model()
        : `${model()} · ${effortLabel(fallback)}`
      : model();
  });

  function move(next: number): void {
    const stop = stops()[next];
    if (stop === undefined) {
      return;
    }
    props.onChoose(stop === null ? { model: props.choice.model } : { model: props.choice.model, effort: stop });
  }

  return (
    <Show when={efforts().length > 0}>
      <Popover
        label="Effort"
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
            title="Effort"
          >
            <span class={composer.chipLabel}>
              {props.choice.effort === undefined || props.choice.effort === null
                ? "Effort"
                : effortLabel(props.choice.effort)}
            </span>
          </button>
        )}
      >
        {() => (
          <div class={composer.popover}>
            <p class={styles.level}>{reading()}</p>
            <p class={styles.model}>{subline()}</p>
            <div class={styles.control}>
              <Detents
                count={stops().length}
                position={position()}
                ariaLabel="Effort"
                ariaValueText={reading()}
                onMove={move}
              />
              <div class={styles.ends}>
                <span>{DEFAULT_NAME}</span>
                <span>{effortLabel(efforts()[efforts().length - 1] ?? "")}</span>
              </div>
            </div>
          </div>
        )}
      </Popover>
    </Show>
  );
}
