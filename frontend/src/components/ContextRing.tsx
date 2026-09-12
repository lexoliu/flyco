/**
 * The context ring beside send, and the usage panel it opens.
 *
 * The ring is the same `Ring` every other readout draws; what makes it a
 * control is the popover behind it: how full the context window is, when
 * the harness will compact it on its own, how much of each plan window is
 * spent and when it turns over, and the way to the full breakdown — which
 * asks the daemon and lands in the transcript as a card.
 *
 * One control rather than a ring per reading: the composer's right edge
 * holds the model chip and send, and is not a dashboard (docs/ux.md §9.3).
 * The panel asks for nothing the harness has not said — before the first
 * `context_usage` answer there is no category bar and no compaction
 * threshold, and the panel shows only what is known.
 */
import { For, Show, createMemo } from "solid-js";
import { ListTree } from "lucide-solid";
import Popover from "./Popover";
import Ring from "./Ring";
import type { ContextUsage, ContextWindow, UsageWindow } from "../api/wire";
import { resetHint, windowTier } from "../lib/planUsage";
import { tokens } from "../lib/tokens";
import styles from "./ContextRing.module.css";

export interface ContextRingProps {
  /**
   * The context window's fill, from wherever it was last said — a `usage`
   * report, a completed turn's usage, or a `context_usage` answer. `null`
   * before any of those, when there is nothing to draw.
   */
  context: ContextWindow | null;
  /**
   * The newest `context_usage` answer, where one has been asked for — the
   * panel's compaction threshold and category bar come from it. Absent
   * until the breakdown has been asked for once.
   */
  usage: ContextUsage | null;
  /** The plan's rolling limit windows, in reading order. Empty until reported. */
  windows: UsageWindow[];
  /** The page's clock, so every reset hint reads the same instant. */
  now: number;
  /**
   * Whether the session's daemon is there to answer a breakdown. The
   * request is delivered or it is nothing — the room cannot hold it the
   * way it holds a prompt — so while no machine is connected the action
   * is greyed rather than sent to die.
   */
  machineUp: boolean;
  /**
   * Sends the `context_usage` control request. The daemon's answer arrives
   * as a `context_usage` event and renders in the transcript as the
   * breakdown card, so the panel does not have to carry the whole list.
   */
  onBreakdown: () => void;
}

export default function ContextRing(props: ContextRingProps) {
  /**
   * What the trigger ring draws. The context fill is the reading the ring
   * is named for; before the harness has reported one, the fullest plan
   * window stands in, labelled for what it is rather than dressed as a
   * context it is not.
   */
  const ring = createMemo(() => {
    const context = props.context;
    if (context !== null) {
      return {
        label: "Context",
        value: context.used_tokens,
        total: context.size_tokens,
        readout: `${tokens(context.used_tokens)} / ${tokens(context.size_tokens)}`,
        hint: undefined,
      };
    }
    const fullest = props.windows.reduce<UsageWindow | null>(
      (fullest, window) =>
        fullest === null || window.used_percent > fullest.used_percent ? window : fullest,
      null,
    );
    if (fullest === null) {
      return null;
    }
    return {
      label: "Plan",
      value: fullest.used_percent,
      total: 100,
      readout: `${fullest.used_percent}%`,
      hint: resetHint(fullest, props.now),
    };
  });

  const percent = createMemo(() => {
    const context = props.context;
    if (context === null || context.size_tokens <= 0) {
      return null;
    }
    return Math.min(100, Math.round((context.used_tokens / context.size_tokens) * 100));
  });

  /**
   * The window's fill as one segment per category.
   *
   * Segments are laid down in the harness's own order — system prompt,
   * tools, messages — and stop where `used_tokens` does, so a trailing
   * remainder category (the SDK lists free space as one) never draws over
   * what is actually empty. `null` until a breakdown has been asked for,
   * and the plain bar is drawn instead.
   */
  const segments = createMemo(() => {
    const usage = props.usage;
    const window = usage?.window ?? props.context;
    if (usage === null || window === null || window.size_tokens <= 0) {
      return null;
    }
    const categories = usage.categories;
    if (categories.length === 0) {
      return null;
    }
    let covered = 0;
    const out: { name: string; ratio: number; deferred: boolean }[] = [];
    for (const category of categories) {
      if (covered >= window.used_tokens) {
        break;
      }
      const taken = Math.min(category.tokens, window.used_tokens - covered);
      covered += taken;
      out.push({
        name: category.name,
        ratio: taken / window.size_tokens,
        deferred: category.deferred,
      });
    }
    return out;
  });

  /** The fill at which the harness compacts on its own, as a percent of the window. */
  const compactAt = createMemo(() => {
    const at = props.usage?.auto_compact;
    const window = props.usage?.window ?? props.context;
    if (at === null || at === undefined || window === null || window.size_tokens <= 0) {
      return null;
    }
    return Math.round((at / window.size_tokens) * 100);
  });

  return (
    <Popover
      label="Context and plan usage"
      align="end"
      panelClass={styles.panel}
      trigger={(attrs) => (
        <button
          id={attrs.id}
          onClick={attrs.onClick}
          aria-expanded={attrs.expanded()}
          aria-haspopup="dialog"
          type="button"
          class={styles.trigger}
          title="Context and plan usage"
        >
          <Show when={ring()}>
            {(view) => (
              <Ring
                label={view().label}
                value={view().value}
                total={view().total}
                readout={view().readout}
                hint={view().hint}
              />
            )}
          </Show>
        </button>
      )}
    >
      {(close) => (
        <div class={styles.body}>
          <Show when={props.context}>
            {(context) => (
              <section class={styles.section} aria-label="Context window">
                <p class={styles.heading}>
                  Context window
                  <span class={styles.readout}>
                    {tokens(context().used_tokens)} / {tokens(context().size_tokens)}
                    <Show when={percent()}>{(value) => <> · {value()}%</>}</Show>
                  </span>
                </p>
                <div
                  class={styles.track}
                  role="progressbar"
                  aria-label="Context window"
                  aria-valuemin={0}
                  aria-valuemax={100}
                  aria-valuenow={percent() ?? 0}
                >
                  <Show
                    when={segments()}
                    fallback={
                      <div
                        class={styles.fill}
                        data-tier={windowTier(percent() ?? 0)}
                        style={{ width: `${percent() ?? 0}%` }}
                      />
                    }
                  >
                    {(list) => (
                      <For each={list()}>
                        {(segment) => (
                          <div
                            class={styles.segment}
                            data-deferred={segment.deferred || undefined}
                            title={`${segment.name}: ${tokens(Math.round(segment.ratio * context().size_tokens))}`}
                            style={{ width: `${segment.ratio * 100}%` }}
                          />
                        )}
                      </For>
                    )}
                  </Show>
                </div>
                <Show when={compactAt()}>
                  {(value) => (
                    <p class={styles.note}>Compacts automatically at {value()}%</p>
                  )}
                </Show>
              </section>
            )}
          </Show>
          <Show when={props.windows.length > 0}>
            <section class={styles.section} aria-label="Plan usage">
              <p class={styles.heading}>Plan usage</p>
              <For each={props.windows}>
                {(window) => {
                  const hint = resetHint(window, props.now);
                  return (
                    <div class={styles.windowRow}>
                      <p class={styles.windowLabel}>{window.label}</p>
                      <p class={styles.windowReadout}>
                        <Show when={hint !== undefined}>{hint}</Show> {window.used_percent}%
                      </p>
                      <div
                        class={styles.track}
                        role="progressbar"
                        aria-label={window.label}
                        aria-valuemin={0}
                        aria-valuemax={100}
                        aria-valuenow={window.used_percent}
                      >
                        <div
                          class={styles.fill}
                          data-tier={windowTier(window.used_percent)}
                          style={{ width: `${Math.min(window.used_percent, 100)}%` }}
                        />
                      </div>
                    </div>
                  );
                }}
              </For>
            </section>
          </Show>
          <button
            type="button"
            class={styles.breakdown}
            disabled={!props.machineUp}
            title={props.machineUp ? undefined : "The machine is not connected"}
            onClick={() => {
              props.onBreakdown();
              close();
            }}
          >
            <ListTree size={13} aria-hidden="true" />
            See the detailed breakdown
          </button>
        </div>
      )}
    </Popover>
  );
}
