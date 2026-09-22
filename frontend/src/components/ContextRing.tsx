/**
 * The context ring beside send, and the usage panel it opens.
 *
 * The ring is the same `Ring` every other readout draws; what makes it a
 * control is the popover behind it: how full the context window is, when
 * the harness will compact it on its own, how much of each plan window is
 * spent and when it turns over, and — once asked for — the full breakdown
 * the daemon answered with.
 *
 * One control rather than a ring per reading: the composer's right edge
 * holds the model chip and send, and is not a dashboard (docs/ux.md §9.3).
 * The panel asks for nothing the harness has not said — before the first
 * `context_usage` answer there is no category bar, no breakdown list and
 * no compaction threshold, and the panel shows only what is known.
 */
import { For, Show, createEffect, createMemo, createSignal, onCleanup } from "solid-js";
import { ChevronRight, ListTree, Loader } from "lucide-solid";
import Popover from "./Popover";
import Ring from "./Ring";
import { cx } from "../lib/cx";
import type { ContextCost, ContextUsage, ContextWindow, UsageWindow } from "../api/wire";
import { orderedWindows, resetHint, windowTier } from "../lib/planUsage";
import { tokens } from "../lib/tokens";
import { formatDuration } from "../lib/duration";
import { formatUsd } from "../lib/money";
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
  /**
   * The session's cumulative accounting, where it has been reported —
   * the panel's `This session` rows and the answer `/usage` used to be
   * asked for. `null` before the first report or the first finished turn.
   */
  session: SessionTotals | null;
  /** The page's clock, so every reset hint reads the same instant. */
  now: number;
  /**
   * Whether the session's daemon is there to answer a breakdown. The
   * request is delivered or it is nothing — the room cannot hold it the
   * way it holds a prompt — so while no machine is connected the action
   * either wakes the machine (when `onResume` is offered) or says why it
   * cannot run, rather than dying silently.
   */
  machineUp: boolean;
  /**
   * Resumes an interrupted session so its machine can answer, offered only
   * when the session's own action is a resume. Resolves `true` once the
   * control plane accepted the resume; `false` means the attempt failed
   * and the button should offer the wake again rather than wait on a
   * machine that is not coming.
   */
  onResume?: (() => Promise<boolean>) | undefined;
  /**
   * Sends the `context_usage` control request. The daemon's answer arrives
   * as a `context_usage` event and lands back in `usage`, which is how the
   * panel knows the asking is over and the breakdown is in.
   */
  onBreakdown: () => void;
}

/** How long an unanswered request keeps the button saying it asked. */
const ASK_TIMEOUT_MS = 15_000;

/**
 * What the session has done so far, in numbers a `/usage` answer used to
 * spell out: tokens in and out, the harness's own cost estimate where it
 * gives one, and how long its turns have run.
 */
export interface SessionTotals {
  inputTokens: number;
  outputTokens: number;
  /**
   * Estimated cost in microdollars — `Usd` on the wire, kept in micros
   * here because money stays in microdollars until it is displayed.
   * `null` where the harness says nothing.
   */
  costMicros: number | null;
  /** Seconds the session's turns have run, finished and in flight. */
  workedSeconds: number;
}

export default function ContextRing(props: ContextRingProps) {
  /**
   * A request is in flight. The panel says so until the answer lands —
   * `usage` becoming a different object is the answer arriving — or the
   * machine stays silent long enough that asking again is honest.
   */
  const [asking, setAsking] = createSignal(false);
  /**
   * A resume asked for the breakdown is waiting on the machine. The
   * question is sent the moment the daemon is back — the click that chose
   * "wake for the breakdown" already said what it wanted.
   */
  const [waking, setWaking] = createSignal(false);
  /**
   * The last ask timed out with no answer. The button offers to try again
   * and the panel says so, instead of looking as though nothing happened.
   */
  const [missed, setMissed] = createSignal(false);
  let baseline: ContextUsage | null = null;
  let timer: ReturnType<typeof setTimeout> | undefined;

  createEffect(() => {
    const usage = props.usage;
    if (asking() && usage !== null && usage !== baseline) {
      setAsking(false);
      setMissed(false);
      clearTimeout(timer);
      timer = undefined;
    }
  });
  createEffect(() => {
    if (waking() && props.machineUp) {
      setWaking(false);
      ask();
    }
  });
  onCleanup(() => clearTimeout(timer));

  function ask(): void {
    baseline = props.usage;
    setAsking(true);
    setMissed(false);
    props.onBreakdown();
    clearTimeout(timer);
    timer = setTimeout(() => {
      setAsking(false);
      setMissed(true);
    }, ASK_TIMEOUT_MS);
  }

  async function askOrWake(): Promise<void> {
    if (props.machineUp) {
      ask();
      return;
    }
    const resume = props.onResume;
    if (resume === undefined) {
      return;
    }
    setWaking(true);
    if (!(await resume())) {
      setWaking(false);
    }
  }

  /** What the breakdown control is doing, in the order the states run. */
  const action = createMemo(() => {
    if (asking()) {
      return { label: "Asking the machine…", enabled: false };
    }
    if (waking()) {
      return { label: "Waking the machine…", enabled: false };
    }
    if (!props.machineUp) {
      return props.onResume === undefined
        ? { label: "See the detailed breakdown", enabled: false }
        : { label: "Wake the machine for the breakdown", enabled: true };
    }
    return {
      label:
        props.usage === null ? "See the detailed breakdown" : "Refresh the breakdown",
      enabled: true,
    };
  });

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
                compact
              />
            )}
          </Show>
        </button>
      )}
    >
      {() => (
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
          <Show when={props.session}>
            {(session) => (
              <section class={styles.section} aria-label="This session">
                <p class={styles.heading}>This session</p>
                <div class={styles.statRow}>
                  <p class={styles.statLabel}>Tokens</p>
                  <p class={styles.statReadout}>
                    {tokens(session().inputTokens)} in · {tokens(session().outputTokens)} out
                  </p>
                </div>
                <Show when={session().costMicros !== null}>
                  <div class={styles.statRow}>
                    <p class={styles.statLabel}>Reported cost</p>
                    <p class={styles.statReadout}>{formatUsd(session().costMicros ?? 0)}</p>
                  </div>
                </Show>
                <Show when={session().workedSeconds > 0}>
                  <div class={styles.statRow}>
                    <p class={styles.statLabel}>Working</p>
                    <p class={styles.statReadout}>{formatDuration(session().workedSeconds)}</p>
                  </div>
                </Show>
              </section>
            )}
          </Show>
          <Breakdown usage={props.usage} />
          <section class={styles.section} aria-label="Plan usage">
            <p class={styles.heading}>Plan usage</p>
            <Show
              when={props.windows.length > 0}
              fallback={
                <p class={styles.note}>No plan limits reported</p>
              }
            >
              <For each={orderedWindows(props.windows)}>
                {(window) => {
                  const hint = resetHint(window, props.now);
                  return (
                    <div class={styles.windowRow}>
                      <p class={styles.windowLabel}>{window.label}</p>
                      <p class={styles.windowReadout}>
                        {window.used_percent}%
                        <Show when={hint !== undefined}> · {hint}</Show>
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
            </Show>
          </section>
          <button
            type="button"
            class={styles.breakdown}
            disabled={!action().enabled}
            onClick={() => void askOrWake()}
          >
            <Show when={asking() || waking()} fallback={<ListTree size={13} aria-hidden="true" />}>
              <Loader size={13} class={cx(styles.spin)} aria-hidden="true" />
            </Show>
            {action().label}
          </button>
          <Show when={missed()}>
            <p class={styles.note}>No answer — the machine may not know this request. Try again.</p>
          </Show>
          <Show when={!props.machineUp && props.onResume === undefined}>
            <p class={styles.note}>The machine is not connected — it answers the breakdown.</p>
          </Show>
        </div>
      )}
    </Popover>
  );
}

/**
 * What the window is spent on, once asked for.
 *
 * One row per category — the legend of the segmented bar above — then the
 * detail lists behind disclosures: they are where a "why is it full" hunt
 * goes, not part of the headline. Nothing until a `context_usage` answer
 * has arrived; the ask is the button below.
 */
function Breakdown(props: { usage: ContextUsage | null }) {
  return (
    <Show when={props.usage}>
      {(usage) => (
        <>
          <ul class={styles.costs}>
            <For each={usage().categories}>
              {(row) => <CostRow row={row} />}
            </For>
          </ul>
          <CostSection label="MCP tools" rows={usage().mcp_tools} />
          <CostSection label="Memory files" rows={usage().memory_files} />
          <CostSection label="Agents" rows={usage().agents} />
          <CostSection label="Skills" rows={usage().skills} />
          <Show when={usage().model}>
            {(model) => <p class={styles.note}>As reported by {model()}</p>}
          </Show>
        </>
      )}
    </Show>
  );
}

/** One `name — 4k` row of a breakdown list. */
function CostRow(props: { row: ContextCost }) {
  return (
    <li class={styles.cost}>
      <span class={styles.costName}>
        {props.row.name}
        <Show when={props.row.deferred}>
          <span class={styles.deferred}>deferred</span>
        </Show>
      </span>
      <span class={styles.costTokens}>{tokens(props.row.tokens)}</span>
    </li>
  );
}

/**
 * One of the breakdown's detail lists, behind a disclosure.
 *
 * The summary carries the section's total, so the collapsed panel still
 * answers "what do the tools cost" without making forty tool names the
 * price of the answer.
 */
function CostSection(props: { label: string; rows: ContextCost[] }) {
  const total = () => props.rows.reduce((sum, row) => sum + row.tokens, 0);
  return (
    <Show when={props.rows.length > 0}>
      <details class={styles.detail}>
        <summary class={styles.detailSummary}>
          <ChevronRight size={13} class={cx(styles.detailChevron)} aria-hidden="true" />
          {props.label}
          <span class={styles.detailTotal}>{tokens(total())}</span>
        </summary>
        <ul class={styles.costs}>
          <For each={props.rows}>{(row) => <CostRow row={row} />}</For>
        </ul>
      </details>
    </Show>
  );
}
