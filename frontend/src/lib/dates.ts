/**
 * Absolute dates and clock times, in the reader's own locale.
 *
 * `src/lib/relativeTime.ts` covers "3 minutes ago", which is what a session
 * list wants. Settings wants the other half: when an account was linked and
 * when its credential expires are facts a user checks against a calendar,
 * and "in 2 months" is not something you can check against a calendar.
 */

const DATE = new Intl.DateTimeFormat(undefined, {
  year: "numeric",
  month: "short",
  day: "numeric",
});

/** Formats a Unix timestamp in seconds as a short date, e.g. `4 Sep 2026`. */
export function formatDate(atUnix: number): string {
  return DATE.format(new Date(atUnix * 1000));
}

/**
 * The clock, without the calendar.
 *
 * `7:35 PM` is how a person says when something happens later today, which
 * is the only distance a usage limit is ever announced over (docs/ux.md
 * §9.2). Seconds and a date there would be precision about a wait, which is
 * the one thing nobody is timing.
 */
const TIME = new Intl.DateTimeFormat(undefined, {
  hour: "numeric",
  minute: "2-digit",
});

/** Formats a Unix timestamp in seconds as a clock time, e.g. `7:35 PM`. */
export function formatTimeOfDay(atUnix: number): string {
  return TIME.format(new Date(atUnix * 1000));
}
