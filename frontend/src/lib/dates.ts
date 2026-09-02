/**
 * Absolute dates, in the reader's own locale.
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
