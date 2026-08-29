/**
 * Session creation, on top of the typed client.
 */
import { createSession, type HarnessKind, type SessionDetail } from "./client";
import { dollarsToUsdMicros } from "../lib/money";

export interface NewSessionInput {
  repo: string;
  harness: HarnessKind;
  /** Whole-dollar budget limit, as entered in the new-session form. */
  budgetLimitDollars: number;
  spot: boolean;
}

export function requestNewSession(input: NewSessionInput): Promise<SessionDetail> {
  return createSession({
    repo: input.repo,
    harness: input.harness,
    budget_limit: dollarsToUsdMicros(input.budgetLimitDollars),
    spot: input.spot,
  });
}
