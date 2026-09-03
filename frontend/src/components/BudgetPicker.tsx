/**
 * The one control that sets a session's compute budget.
 *
 * Two places set a budget and they are the same question asked twice: the
 * composer, before a session exists, and the session header, where raising
 * it is what releases a session paused on an exhausted one (docs/ux.md
 * §9.1). So the slider, the amount, and the sentence explaining what the
 * money buys are written once here, and each caller supplies only what
 * differs — the control that opens the panel, and, where the change costs a
 * round trip, the action that commits it.
 *
 * Controlled, deliberately: the amount lives with the caller, because the
 * trigger renders it (the composer's chip reads `$10`, the header's ring
 * reads `$1.20 / $10`) and a draft hidden in here would leave both showing
 * a number the slider had already moved off.
 */
import type { JSX } from "solid-js";
import { Show, createSignal } from "solid-js";
import Popover, { type TriggerAttrs } from "./Popover";
import styles from "./BudgetPicker.module.css";

/** What the budget slider spans, in whole dollars. */
export const MIN_BUDGET = 1;
export const MAX_BUDGET = 200;
export const DEFAULT_BUDGET = 10;

export interface BudgetPickerProps {
  /** The amount the panel shows, in whole dollars. */
  dollars: number;
  /** The new amount, on every move of the slider. */
  onChange: (dollars: number) => void;
  /** Lowest amount the slider offers. Defaults to {@link MIN_BUDGET}. */
  min?: number;
  /** Highest amount the slider offers. Defaults to {@link MAX_BUDGET}. */
  max?: number;
  /** What the panel is, for assistive technology. */
  label?: string;
  /** Which edge of the trigger the panel lines up with. */
  align?: "start" | "end" | undefined;
  /** The control that opens the panel. */
  trigger: (attrs: TriggerAttrs) => JSX.Element;
  /**
   * What commits the change, for a caller whose change is a request.
   *
   * Absent in the composer, where the amount is a field of a form nobody
   * has submitted yet and every move of the slider is already the answer.
   */
  action?: ((close: () => void) => JSX.Element) | undefined;
}

export default function BudgetPicker(props: BudgetPickerProps) {
  const min = () => props.min ?? MIN_BUDGET;
  const max = () => props.max ?? MAX_BUDGET;

  return (
    <Popover label={props.label ?? "Budget"} align={props.align} trigger={props.trigger}>
      {(close) => (
        <div class={styles.popover}>
          <div class={styles.sliderRow}>
            <span class={styles.amount}>${props.dollars}</span>
            <span class={styles.note}>
              ${min()}–${max()}
            </span>
          </div>
          <input
            class={styles.slider}
            type="range"
            min={min()}
            max={max()}
            step="1"
            value={props.dollars}
            aria-label="Session budget in dollars"
            onInput={(event) => props.onChange(Number(event.currentTarget.value))}
          />
          <p class={styles.note}>
            Covers the machine and its disk. Model tokens are billed by your Claude or Codex plan.
          </p>
          <Show when={props.action}>{(action) => action()(close)}</Show>
        </div>
      )}
    </Popover>
  );
}

export interface BudgetRaiseProps {
  /** What the session may spend now, in whole dollars. */
  limitUsd: number | undefined;
  /** What it has spent, in whole dollars. */
  spentUsd: number | undefined;
  /** Whether a change is in flight. */
  saving: boolean;
  /** Sets the new limit, in whole dollars. */
  onSet: (dollars: number) => void;
  /** The control that opens the panel. */
  trigger: (attrs: TriggerAttrs) => JSX.Element;
  /** What the panel is, for assistive technology. */
  label?: string;
  /** Which edge of the trigger the panel lines up with. */
  align?: "start" | "end" | undefined;
}

/**
 * The budget of a session that already exists, which is a request rather
 * than a field.
 *
 * Where the composer's chip is the whole answer the moment the slider
 * moves, this one costs a round trip, so it holds the amount as a draft and
 * commits it on an explicit action. Two places raise a budget — the ring in
 * the session header and the notice on a session paused because the budget
 * ran out — and they differ only in what the user clicks to get here.
 */
export function BudgetRaise(props: BudgetRaiseProps) {
  /**
   * The amount the slider is showing while the user drags it, or
   * `undefined` for "whatever the session says" — what the panel opens on,
   * and what it falls back to once a change has landed.
   */
  const [draft, setDraft] = createSignal<number | undefined>(undefined);

  /**
   * The first whole dollar above what the session has already spent.
   *
   * The floor of the slider, because money that is gone is gone: a limit at
   * or under the spend leaves the budget exhausted, so offering it would be
   * offering a way to press the button and stay paused.
   */
  const floor = () => Math.floor(props.spentUsd ?? 0) + 1;
  const amount = () => draft() ?? Math.max(floor(), Math.round(props.limitUsd ?? DEFAULT_BUDGET));

  function commit(close: () => void): void {
    close();
    const next = amount();
    setDraft(undefined);
    if (next !== props.limitUsd) {
      props.onSet(next);
    }
  }

  return (
    <BudgetPicker
      label={props.label ?? "Session budget"}
      align={props.align}
      dollars={amount()}
      onChange={setDraft}
      min={floor()}
      max={Math.max(MAX_BUDGET, amount(), floor())}
      trigger={props.trigger}
      action={(close) => (
        <button
          type="button"
          class={styles.commit}
          disabled={props.saving}
          onClick={() => commit(close)}
        >
          {props.saving ? "Saving…" : `Set budget to $${amount()}`}
        </button>
      )}
    />
  );
}
