/**
 * What flyco knows about one linked harness account's spending.
 *
 * Two halves, from two sources and answering two questions. The *plan*
 * windows come from the vendor, read while this page was answered: how
 * much of the five-hour and weekly limits is gone, and when each turns
 * over. That is the answer to "how much of my plan is left", and it is the
 * reason this component exists at all (docs/ux.md §7.4). A vendor that
 * would not answer says so here — a bar at zero would be an invention.
 *
 * The observed half below it is flyco's own record and is deliberately not
 * a quota: `GET /v1/usage/llm` reports what the harness said a session cost
 * and, once the vendor actually refused a call, how much of the wait for
 * the reset has passed. That wait is a fallback and nothing more — it
 * names no window, so beside live plan windows it reads as a second,
 * contradictory answer to a question they already answer per window, and
 * it renders only where the vendor would not state the plan. Nothing
 * renders at all until there is something true to render.
 *
 * A component of its own rather than a corner of the settings card, so that
 * wherever a linked account is read out, its usage is read out the same
 * way.
 */
import { For, Show } from "solid-js";
import RatioBar from "./RatioBar";
import type { HarnessAccountView, LlmUsageRow } from "../api/client";
import type { UsageWindow } from "../api/wire";
import { formatDate } from "../lib/dates";
import { formatUsd } from "../lib/money";
import { orderedWindows, resetHint, windowTier } from "../lib/planUsage";
import { relativeTime } from "../lib/relativeTime";
import styles from "./HarnessUsage.module.css";

export interface HarnessUsageProps {
  /** The account's observed-usage row, or `undefined` while it has none. */
  row: LlmUsageRow | undefined;
  /** The plan the vendor stated, or why it could not be stated. */
  plan: HarnessAccountView["usage"];
}

export default function HarnessUsage(props: HarnessUsageProps) {
  const now = Date.now();
  /** Whether the vendor stated the plan, which the wait bar defers to. */
  const stated = () => props.plan.state === "windows" && props.plan.windows.length > 0;
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
      <For each={orderedWindows(props.plan.state === "windows" ? props.plan.windows : [])}>
        {(window) => (
          <RatioBar
            label={window.label}
            ratio={window.used_percent / 100}
            value={readout(window, now)}
            tier={windowTier(window.used_percent)}
          />
        )}
      </For>
      {/* The vendor would not say. A bar at zero would be an invention,
          and the reason it gave is in the control plane's log rather than
          under the account: what the reader can do about it is relink. */}
      <Show when={props.plan.state === "unavailable"}>
        <p class={styles.line}>Plan usage unavailable</p>
      </Show>
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
            <Show when={waited() !== undefined && !stated()}>
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
