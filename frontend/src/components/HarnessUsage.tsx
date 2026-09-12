/**
 * What flyco knows about one linked harness account's spending.
 *
 * Two halves, from two sources and answering two questions. The *plan*
 * windows come from the vendor itself, through the account's last session:
 * how much of the five-hour and weekly limits is gone, and when each turns
 * over. That is the answer to "how much of my plan is left", and it is the
 * reason this component exists at all (docs/ux.md §7.4).
 *
 * The observed half below it is flyco's own record and is deliberately not
 * a quota: `GET /v1/usage/llm` reports what the harness said a session cost
 * and, once the vendor actually refused a call, how much of the wait for
 * the reset has passed. Nothing renders at all until there is something
 * true to render.
 *
 * A component of its own rather than a corner of the settings card, so that
 * wherever a linked account is read out, its usage is read out the same
 * way.
 */
import { For, Show } from "solid-js";
import RatioBar from "./RatioBar";
import type { LlmUsageRow } from "../api/client";
import type { UsageWindow } from "../api/wire";
import { formatDate } from "../lib/dates";
import { formatUsd } from "../lib/money";
import { orderedWindows, resetHint, windowTier } from "../lib/planUsage";
import { relativeTime } from "../lib/relativeTime";
import styles from "./HarnessUsage.module.css";

export interface HarnessUsageProps {
  /** The account's observed-usage row, or `undefined` while it has none. */
  row: LlmUsageRow | undefined;
  /** The plan windows the account last reported. Empty until one has. */
  windows: readonly UsageWindow[];
}

export default function HarnessUsage(props: HarnessUsageProps) {
  const now = Date.now();
  const limitedAt = () => props.row?.rate_limited_at_unix ?? null;
  const resetsAt = () => props.row?.resets_at_unix ?? null;
  const waited = () => {
    const from = limitedAt();
    const to = resetsAt();
    if (from === null || to === null || to <= from) {
      return undefined;
    }
    return (Math.floor(now / 1000) - from) / (to - from);
  };

  return (
    <div class={styles.usage}>
      {/*
        Shortest window first: the one about to stop the user, then the one
        they are pacing against.
      */}
      <For each={orderedWindows(props.windows)}>
        {(window) => (
          <RatioBar
            label={window.label}
            ratio={window.used_percent / 100}
            value={readout(window, now)}
            tier={windowTier(window.used_percent)}
          />
        )}
      </For>
      <Show when={props.row}>
        {(row) => (
          <>
            {/* A cost with no ceiling is a figure, not a bar. */}
            <Show when={row().observed_cost !== null && row().observed_cost !== undefined}>
              <p class={styles.line}>
                {formatUsd(row().observed_cost ?? 0)} reported by the harness since{" "}
                {formatDate(row().period_start_unix)}
              </p>
            </Show>
            <Show when={waited() !== undefined}>
              <RatioBar
                label="Usage limit"
                ratio={waited()}
                value={`resets ${relativeTime(resetsAt() ?? 0, now)}`}
                tier="warn"
              />
            </Show>
          </>
        )}
      </Show>
    </div>
  );
}

/** `26% · Resets in 2h 10m`, or the percentage alone where there is no reset. */
function readout(window: UsageWindow, now: number): string {
  const hint = resetHint(window, now);
  return hint === undefined ? `${window.used_percent}%` : `${window.used_percent}% · ${hint}`;
}
