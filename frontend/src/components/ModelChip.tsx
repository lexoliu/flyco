/**
 * Which model the agent runs, and how hard it thinks (docs/ux.md §5,
 * §9.3).
 *
 * One chip in two places — the home composer, where it decides what a
 * session opens on, and the session composer, where changing it reaches
 * the running agent — because the official apps put it in exactly that
 * spot and a person who has used either looks there for it.
 *
 * The panel asks the question in two views the way ChatGPT's own picker
 * nests them: the model's list first, and picking a model that takes
 * effort lands on its detented slider rather than closing on the pick —
 * the levels are the model's, so the scale follows the choice. A model
 * that takes none still closes on the pick. The chip reads the whole
 * choice: `Fable`, then `· High` when a level is chosen.
 *
 * The list is the harness's own, served per linked account, and the chip
 * shows the model's display name rather than its id: `Fable`, not
 * `claude-fable-5-1[1m]`.
 */
import { For, Show, createEffect, createMemo, createSignal } from "solid-js";
import { Check, ChevronLeft, RotateCcw } from "lucide-solid";
import Popover from "./Popover";
import Detents from "./Detents";
import type { ModelChoice, ModelOption } from "../api/client";
import { cx } from "../lib/cx";
import { choiceLabel, effortLabel, shortName } from "../lib/models";
import composer from "./Composer.module.css";
import styles from "./ModelChip.module.css";

/**
 * Below this many rows a filter box is furniture, not help: every model
 * fits on one screen, and reading them is faster than typing.
 */
const SEARCH_WORTH_IT = 8;

/** What the leftmost detent is called: the model keeping the choice. */
const DEFAULT_NAME = "Default";

export interface ModelChipProps {
  /** The models the agent offers. */
  models: readonly ModelOption[];
  /** What is chosen now. */
  choice: ModelChoice;
  /**
   * The new choice, on a change of the model or the effort.
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
      label="Model and effort"
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
          <span class={composer.chipLabel}>{choiceLabel(props.models, props.choice)}</span>
        </button>
      )}
    >
      {(close) => {
        // The query and the view live with the panel rather than the
        // chip: a closed picker forgets both, so reopening always lands
        // on the full list.
        const [query, setQuery] = createSignal("");
        /**
         * The model the effort view configures — the row that was picked.
         *
         * Tracked separately from {@link ModelChipProps.choice} because a
         * session composer's pick is a PATCH the choice answers a round
         * trip later: the slider has to show the picked model's levels
         * now, not the stale choice's.
         */
        const [effortFor, setEffortFor] = createSignal<ModelOption | null>(null);

        /**
         * The effort the drilled-in model runs at: the choice's own level
         * once the choice names that model, `undefined` — the `Default`
         * stop — while the pick is still on its way.
         */
        const effort = createMemo(() =>
          props.choice.model === effortFor()?.id ? props.choice.effort : undefined,
        );

        /** The drilled-in model's own levels, in the order the harness stated them. */
        const efforts = createMemo(() => effortFor()?.efforts ?? []);

        /**
         * The rail's stops: `Default` first — the model keeps the choice —
         * then the harness's levels. `null` is the stop that is not a level.
         */
        const stops = createMemo<readonly (string | null)[]>(() => [null, ...efforts()]);

        /** The stop the thumb sits on; a chosen effort the list lost parks at `Default`. */
        const position = createMemo(() => {
          const level = effort();
          if (level === undefined || level === null) {
            return 0;
          }
          return Math.max(0, stops().indexOf(level));
        });

        /**
         * What the stop the thumb sits on is called: the level chosen, or
         * `Default` where the model keeps the choice.
         */
        const reading = createMemo(() => {
          const level = effort();
          return level === undefined || level === null ? DEFAULT_NAME : effortLabel(level);
        });

        /**
         * The line under the level: the model it belongs to, and — where
         * the harness says which level `Default` means — what the choice
         * resolves to.
         */
        const subline = createMemo(() => {
          const option = effortFor();
          if (option === null) {
            return "";
          }
          const name = shortName(option);
          const fallback = option.default_effort;
          const level = effort();
          return level === undefined || level === null
            ? fallback === undefined || fallback === null
              ? name
              : `${name} · ${effortLabel(fallback)}`
            : name;
        });

        /**
         * The rows the box's query admits — every word has to appear
         * somewhere in the row's own text, so `opus fast` finds
         * `Claude Opus 5 Fast`.
         */
        const filtered = createMemo<readonly ModelOption[]>(() => {
          const words = query().trim().toLowerCase().split(/\s+/).filter(Boolean);
          if (words.length === 0) {
            return props.models;
          }
          return props.models.filter((option) => {
            const text = `${option.label} ${option.id} ${option.description}`.toLowerCase();
            return words.every((word) => text.includes(word));
          });
        });

        // The effort view is only reachable behind a model that takes
        // effort: a drilled-in option that loses its levels under an open
        // panel — the account's model list refreshing — drops the view
        // back to the list rather than drawing a rail of nothing.
        createEffect(() => {
          if (effortFor() !== null && efforts().length === 0) {
            setEffortFor(null);
          }
        });

        /**
         * The pick a row stands for: the model becomes the choice, and a
         * model that takes effort opens its slider — the pick is not done
         * until the level is seen — while one that takes none is done
         * already and closes.
         *
         * Picking the model already chosen asks for nothing: it is the
         * way into its slider, and re-sending `{model}` would drop the
         * effort it already runs at.
         */
        function pick(option: ModelOption): void {
          if (option.id !== props.choice.model) {
            props.onChoose({ model: option.id });
          }
          if ((option.efforts?.length ?? 0) > 0) {
            setEffortFor(option);
          } else {
            close();
          }
        }

        function move(next: number): void {
          const stop = stops()[next];
          const option = effortFor();
          if (stop === undefined || option === null) {
            return;
          }
          props.onChoose(
            stop === null ? { model: option.id } : { model: option.id, effort: stop },
          );
        }

        return (
          <Show
            when={effortFor() !== null && efforts().length > 0}
            fallback={
              <div class={composer.popover}>
                <Show when={props.models.length > SEARCH_WORTH_IT}>
                  <input
                    class={composer.search}
                    type="search"
                    placeholder="Search models"
                    aria-label="Search models"
                    value={query()}
                    onInput={(event) => setQuery(event.currentTarget.value)}
                  />
                </Show>
                <p class={composer.popoverTitle}>Model</p>
                <ul class={composer.options} role="listbox" aria-label="Model">
                  <For each={filtered()}>
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
                          onClick={() => pick(option)}
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
                <Show when={query().trim() !== "" && filtered().length === 0}>
                  <p class={composer.note}>No models match “{query().trim()}”.</p>
                </Show>
              </div>
            }
          >
            <div class={composer.popover}>
              <div class={styles.head}>
                <button
                  type="button"
                  class={styles.back}
                  aria-label="Back to models"
                  onClick={() => setEffortFor(null)}
                >
                  <ChevronLeft size={14} aria-hidden="true" />
                  {reading()}
                </button>
                <button
                  type="button"
                  class={styles.reset}
                  aria-label="Reset effort"
                  title="Reset effort"
                  disabled={position() === 0}
                  onClick={() => move(0)}
                >
                  <RotateCcw size={14} aria-hidden="true" />
                </button>
              </div>
              <p class={styles.effortModel}>{subline()}</p>
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
          </Show>
        );
      }}
    </Popover>
  );
}
