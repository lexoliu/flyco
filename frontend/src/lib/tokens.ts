/**
 * Token counts, for the two places that read them to a person.
 *
 * `41k / 200k`, because a context window is read in thousands or not at
 * all: the ones digit of a 200,000-token window is not a fact anyone
 * compares, so it is rounded away rather than carried as noise.
 */

/** `41k`, or the number itself under a thousand. */
export function tokens(count: number): string {
  return count >= 1000 ? `${Math.round(count / 1000)}k` : `${count}`;
}
