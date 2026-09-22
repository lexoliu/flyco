/**
 * How near a linked credential is to being useless.
 *
 * Settings showed `Linked Sep 3, 2026 · expires Sep 6, 2026` in the same
 * grey as everything else on the card, with `Linked` in green beside it. On
 * Sep 5 that card said the account was fine and, in the same breath and the
 * same weight, that it would stop working tomorrow — taking every running
 * session's token refresh with it. A date the reader has to subtract from
 * today is not a warning.
 *
 * So the distance is computed here and said in words, once, in the line
 * under the account: within a week it turns urgent, and `Relink` — which
 * was already on the card, in the same weight as `Unlink` — becomes the
 * thing to do.
 *
 * Pure: takes `now` rather than reading the clock, so a test can stand
 * anywhere relative to an expiry.
 */
import { formatDate } from "./dates";

/** How close an expiry has to be before the card stops calling it fine. */
const SOON_DAYS = 7;

const DAY_SECONDS = 86_400;

export interface CredentialExpiry {
  /** `gone` is already useless; `soon` will be within {@link SOON_DAYS}. */
  level: "fine" | "soon" | "gone";
  /**
   * The line under the account name: `Expires tomorrow`.
   *
   * The card says this once. A pill above it reading `Expires soon` over a
   * line reading `Expires tomorrow` was the same fact twice, and the
   * vaguer of the two first.
   */
  sentence: string;
}

/**
 * What to say about a credential's expiry, or `null` when it has none —
 * an API key does not expire, and inventing a reassurance for it would be
 * one more thing on the card that never changes.
 */
export function credentialExpiry(
  expiresAtUnix: number | null | undefined,
  nowUnix: number,
): CredentialExpiry | null {
  if (expiresAtUnix === null || expiresAtUnix === undefined) {
    return null;
  }
  if (expiresAtUnix <= nowUnix) {
    return {
      level: "gone",
      sentence: `Expired ${formatDate(expiresAtUnix)}`,
    };
  }
  // Whole days, rounded up: something that runs out in twenty hours runs
  // out tomorrow, and saying "today" of it would be a day early.
  const days = Math.ceil((expiresAtUnix - nowUnix) / DAY_SECONDS);
  if (days > SOON_DAYS) {
    return {
      level: "fine",
      sentence: `Expires ${formatDate(expiresAtUnix)}`,
    };
  }
  return {
    level: "soon",
    sentence: days === 1 ? "Expires tomorrow" : `Expires in ${days} days`,
  };
}
