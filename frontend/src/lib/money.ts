/**
 * Money helpers for `Usd` (`flyco_core::money::Usd`): an integer amount of
 * microdollars (1 USD = 1e6), never a float, on the wire and in every DTO
 * that carries a price or a spend.
 *
 * Every other module converts to whole dollars only at the last moment,
 * for display — arithmetic on money stays in microdollars.
 */

const MICROS_PER_USD = 1_000_000;

/** Converts a wire `Usd` (integer microdollars) to whole dollars, for display. */
export function usdMicrosToDollars(microdollars: number): number {
  return microdollars / MICROS_PER_USD;
}

/** Converts whole dollars (e.g. from a form field) to a wire `Usd`. */
export function dollarsToUsdMicros(dollars: number): number {
  return Math.round(dollars * MICROS_PER_USD);
}

/** Formats a wire `Usd` as `$12.34`. */
export function formatUsd(microdollars: number): string {
  return `$${usdMicrosToDollars(microdollars).toFixed(2)}`;
}
