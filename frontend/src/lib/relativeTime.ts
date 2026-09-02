/**
 * "3 minutes ago", in the reader's own language.
 *
 * `Intl.RelativeTimeFormat` is the platform's own implementation of this and
 * ships in every browser flyco targets, so there is no date library here:
 * a dependency would add kilobytes to do worse localisation than the engine
 * already does. All this module contributes is picking the unit, which the
 * standard deliberately leaves to the caller.
 */

/** Units, largest first, with how many seconds each one is. */
const UNITS: readonly [Intl.RelativeTimeFormatUnit, number][] = [
  ["year", 365 * 24 * 60 * 60],
  ["month", 30 * 24 * 60 * 60],
  ["week", 7 * 24 * 60 * 60],
  ["day", 24 * 60 * 60],
  ["hour", 60 * 60],
  ["minute", 60],
  ["second", 1],
];

const formatter = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });

/**
 * Formats a Unix timestamp as a phrase relative to `now` (milliseconds).
 *
 * The instant is a parameter rather than `Date.now()` so a list renders
 * every row against one clock reading and so this can be tested.
 */
export function relativeTime(atUnix: number, now: number): string {
  const seconds = atUnix - Math.floor(now / 1000);
  const magnitude = Math.abs(seconds);

  for (const [unit, size] of UNITS) {
    if (magnitude >= size) {
      return formatter.format(Math.round(seconds / size), unit);
    }
  }
  // Under a second either way. `numeric: "auto"` renders this as "now".
  return formatter.format(0, "second");
}
