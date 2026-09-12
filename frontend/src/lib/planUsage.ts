/**
 * The plan's rolling limit windows, as the two places that show them read
 * them (docs/ux.md §9.3, §7.4).
 *
 * Both the composer's rings and the harness account's bars in Settings ask
 * the same two questions of a window — where does it belong in the row, and
 * when does it turn over — so the answers live here rather than being
 * written twice and drifting into two orders and two phrasings.
 */
import type { UsageWindow } from "../api/wire";
import { formatDuration } from "./duration";

/**
 * The windows in reading order: shortest first.
 *
 * The five-hour window is the one about to stop the user and the weekly one
 * is the one they are pacing against, so the urgent number comes first.
 * Ordering by the label instead would put `Weekly` before `5-hour` on a
 * technicality of the alphabet. A window whose harness named no length
 * cannot be placed among the others and goes last.
 */
export function orderedWindows(windows: readonly UsageWindow[]): UsageWindow[] {
  return [...windows].sort((left, right) => {
    if (left.window_minutes === right.window_minutes) {
      return 0;
    }
    if (left.window_minutes === null || left.window_minutes === undefined) {
      return 1;
    }
    if (right.window_minutes === null || right.window_minutes === undefined) {
      return -1;
    }
    return left.window_minutes - right.window_minutes;
  });
}

/**
 * `Resets in 2h 10m`, or nothing when the harness named no reset.
 *
 * `now` is milliseconds and a parameter rather than `Date.now()`, so a row
 * of windows is rendered against one clock reading. A window whose reset is
 * already behind us reads `Resets now`: the snapshot is a moment old and
 * the honest thing to say is that the turnover is due, not to count
 * backwards into a negative duration.
 */
export function resetHint(window: UsageWindow, now: number): string | undefined {
  const resets = window.resets_at_unix;
  if (resets === null || resets === undefined) {
    return undefined;
  }
  const seconds = resets - Math.floor(now / 1000);
  return seconds <= 0 ? "Resets now" : `Resets in ${formatDuration(seconds)}`;
}

/** How full a window has to be before the bar says so in colour. */
const WARN_PERCENT = 80;
const FINAL_WARN_PERCENT = 95;

/**
 * The colour tier a fullness is drawn at.
 *
 * Colour only as the window runs out, which is the whole colour policy in
 * docs/ux.md §2: colour means something is wrong, never that something
 * exists. Shared by the settings account page and the composer's usage
 * panel, which draw the same windows the same way.
 */
export function windowTier(percent: number): "ok" | "warn" | "final-warn" {
  if (percent >= FINAL_WARN_PERCENT) {
    return "final-warn";
  }
  return percent >= WARN_PERCENT ? "warn" : "ok";
}
