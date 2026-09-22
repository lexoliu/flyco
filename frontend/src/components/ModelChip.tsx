/**
 * Which model the agent runs, and how hard it thinks (docs/ux.md §5,
 * §9.3).
 *
 * One chip in two places — the home composer, where it decides what a
 * session opens on, and the session composer, where changing it reaches
 * the running agent — because the official apps put it in exactly that
 * spot and a person who has used either looks there for it.
 *
 * The panel asks the question in two views the way Codex's own picker
 * nests them: the effort rail first — the model's name at its head as the
 * way into the model list, the level the thumb sits on under it — and the
 * agent's models behind that link, each with the harness's one-line
 * description. Picking a model that takes effort returns to its rail
 * rather than closing on the pick — the levels are the model's, so the
 * scale follows the choice — and one that takes none closes. The chip
 * reads the whole choice: `Fable · High`.
 *
 * There is no `Default` stop. A level nobody has moved is the harness's
 * own default where it states one and `medium` where it does not
 * ({@link openingEffort}), and it is sent with the model rather than left
 * out: a stop whose only meaning was "we did not say" made the chip, the
 * rail and the run three different answers to one question.
 *
 * The list is the harness's own, served per linked account, and the chip
 * shows the model's display name rather than its id: `Fable`, not
 * `claude-fable-5-1[1m]`.
 */
import { For, Show, createEffect, createMemo, createSignal } from "solid-js";
import { Check, ChevronRight } from "lucide-solid";
import Popover from "./Popover";
import SearchField from "./SearchField";
import Detents from "./Detents";
import type { ModelChoice, ModelOption } from "../api/client";
import { cx } from "../lib/cx";
import { choiceLabel, effortLabel, openingEffort, shortName } from "../lib/models";
import composer from "./Composer.module.css";
import styles from "./ModelChip.module.css";

/**
 * Below this many rows a filter box is furniture, not help: every model
 * fits on one screen, and reading them is faster than typing.
 */
const SEARCH_WORTH_IT = 8;

export interface ModelChipProps {
  /** The models the agent offers. */
  models: readonly ModelOption[];
  /** What is chosen now. */
  choice: ModelChoice;
  /**
   * The new choice, on a change of the model or the effort.
   *
   * A new model starts on its own opening level rather than carrying the
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
        // on the chosen model's slider or, when it takes none, the list.
        const [query, setQuery] = createSignal("");
        /**
         * The model the effort view configures — the chosen one where it
         * takes effort, else the row that was picked.
         *
         * The panel opens where Codex's does: on the chosen model's rail
         * when it has one — the level is what a person adjusting effort
         * came for, and the list is the view behind it — and on the list
         * when the choice's own model takes none, since there is nothing
         * to slide. Tracked separately from
         * {@link ModelChipProps.choice} because a session composer's pick
         * is a PATCH the choice answers a round trip later: the slider
         * has to show the picked model's levels now, not the stale
         * choice's.
         */
        const [effortFor, setEffortFor] = createSignal<ModelOption | null>(
          props.models.find(
            (option) => option.id === props.choice.model && (option.efforts?.length ?? 0) > 0,
          ) ?? null,
        );

        /** The drilled-in model's own levels, in the order the harness stated them. */
        const stops = createMemo<readonly string[]>(() => effortFor()?.efforts ?? []);

        /**
         * The level the thumb sits on: the choice's own once the choice
         * names the drilled-in model, else that model's opening level —
         * which is what a session with no level stated runs at, so the
         * rail reads true while a pick is still on its way.
         */
        const level = createMemo(() => {
          const option = effortFor();
          if (option === null) {
            return undefined;
          }
          const chosen = props.choice.model === option.id ? props.choice.effort : undefined;
          return chosen ?? openingEffort(option);
        });

        /** Where on the rail that level sits; an unknown level parks at the first stop. */
        const position = createMemo(() => {
          const chosen = level();
          return chosen === undefined ? 0 : Math.max(0, stops().indexOf(chosen));
        });

        /** What the stop the thumb sits on is called. */
        const reading = createMemo(() => {
          const chosen = level();
          return chosen === undefined ? "" : effortLabel(chosen);
        });

        /** The head of the rail: the model whose levels it runs on. */
        const modelName = createMemo(() => {
          const option = effortFor();
          return option === null ? "" : shortName(option);
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
          if (effortFor() !== null && stops().length === 0) {
            setEffortFor(null);
          }
        });

        /**
         * The pick a row stands for: the model becomes the choice, and a
         * model that takes effort lands on its slider — the pick is not
         * done until the level is seen — while one that takes none is
         * done already and closes.
         *
         * Picking the model already chosen asks for nothing: it is the
         * way back to its slider, and re-sending `{model}` would drop
         * the effort it already runs at.
         */
        function pick(option: ModelOption): void {
          const opening = openingEffort(option);
          if (option.id !== props.choice.model) {
            props.onChoose(
              opening === undefined
                ? { model: option.id }
                : { model: option.id, effort: opening },
            );
          }
          if (option.efforts.length > 0) {
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
          props.onChoose({ model: option.id, effort: stop });
        }

        return (
          <Show
            when={effortFor() !== null && stops().length > 0}
            fallback={
              <div
                class={composer.popover}
                tabindex="-1"
                ref={(el) =>
                  // The arriving view takes focus — the search box where
                  // there is one: the control that led here went away
                  // with the view that held it, and focus dropped to
                  // <body> reads to the popover as leaving. The ref runs
                  // while the element is still detached, so the focus
                  // waits a microtask for the swap to finish inserting.
                  queueMicrotask(() =>
                    (el.querySelector<HTMLElement>("input[type=search]") ?? el).focus({
                      preventScroll: true,
                    }),
                  )
                }
              >
                <Show when={props.models.length > SEARCH_WORTH_IT}>
                  <SearchField
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
            <div
              class={composer.popover}
              tabindex="-1"
              ref={(el) =>
                // The rail's own input takes focus, so the arrow keys
                // reach the detents without a stop on the way. The ref
                // runs while the element is still detached, so the focus
                // waits a microtask for the swap to finish inserting.
                queueMicrotask(() =>
                  (el.querySelector<HTMLElement>("input[type=range]") ?? el).focus({
                    preventScroll: true,
                  }),
                )
              }
            >
              <div class={styles.head}>
                <button
                  type="button"
                  class={styles.models}
                  aria-label="Choose a model"
                  onClick={() => setEffortFor(null)}
                >
                  {modelName()}
                  <ChevronRight size={14} aria-hidden="true" />
                </button>
              </div>
              <p class={styles.effortReading}>{reading()}</p>
              <div class={styles.control}>
                <Detents
                  count={stops().length}
                  position={position()}
                  ariaLabel="Effort"
                  ariaValueText={reading()}
                  onMove={move}
                />
                <div class={styles.ends}>
                  <span>{effortLabel(stops()[0] ?? "")}</span>
                  <span>{effortLabel(stops()[stops().length - 1] ?? "")}</span>
                </div>
              </div>
            </div>
          </Show>
        );
      }}
    </Popover>
  );
}
