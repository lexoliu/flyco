/**
 * How long something took, written the way a person says it.
 *
 * Distinct from `elapsedSince` in src/lib/status.ts, and deliberately so:
 * that one answers "how long has this been going on" for a status dot and
 * is coarse on purpose, because a counter ticking every second beside a
 * label is motion without information. This one answers "how long did that
 * take" for a finished turn (`Worked for 8m 23s`, docs/ux.md §9.2), where
 * the seconds are the interesting part and rounding them away would turn
 * every quick turn into the same "1m".
 */

const MINUTE = 60;
const HOUR = 60 * MINUTE;

/**
 * A completed duration in seconds, as `8m 23s`.
 *
 * Under a minute it is seconds alone (`12s`); an exact minute drops the
 * seconds (`3m`); an hour or more leads with hours and keeps minutes
 * (`1h 4m`), because at that scale the seconds are noise. Negative input —
 * a clock that ran backwards between two events — reads as `0s` rather than
 * as a duration nobody can act on.
 */
export function formatDuration(seconds: number): string {
  const total = Math.max(0, Math.floor(seconds));
  if (total < MINUTE) {
    return `${total}s`;
  }
  if (total < HOUR) {
    const minutes = Math.floor(total / MINUTE);
    const rest = total % MINUTE;
    return rest === 0 ? `${minutes}m` : `${minutes}m ${rest}s`;
  }
  const hours = Math.floor(total / HOUR);
  const minutes = Math.floor((total % HOUR) / MINUTE);
  return minutes === 0 ? `${hours}h` : `${hours}h ${minutes}m`;
}
