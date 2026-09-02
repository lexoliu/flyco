/**
 * What `GET /v1/usage/llm` will actually say about one linked account.
 *
 * Deliberately not a "42% of your quota" bar. The endpoint's own contract is
 * that every field is an observation and never a quota, so the two honest
 * readings are the cost the harness reported over this window and, once the
 * vendor has actually limited the account, how much of the wait for the
 * reset has passed. Nothing renders at all until there is something true to
 * render.
 *
 * Lives here rather than in Settings because the harness card is drawn in
 * three places — Settings, `/connect/harness`, and the welcome flow — and
 * they must all report usage the same way.
 */
import { Show } from "solid-js";
import RatioBar from "./RatioBar";
import type { LlmUsageRow } from "../api/client";
import { formatDate } from "../lib/dates";
import { formatUsd } from "../lib/money";
import { relativeTime } from "../lib/relativeTime";
import styles from "./HarnessUsage.module.css";

export interface HarnessUsageProps {
  /** The account's row, or `undefined` while it has none. */
  row: LlmUsageRow | undefined;
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
    <Show when={props.row}>
      {(row) => (
        <div class={styles.usage}>
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
        </div>
      )}
    </Show>
  );
}
