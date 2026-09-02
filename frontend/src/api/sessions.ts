/**
 * Session creation, on top of the typed client.
 */
import { createSession, type HarnessKind, type SessionDetail } from "./client";
import { dollarsToUsdMicros } from "../lib/money";

export interface NewSessionInput {
  /**
   * What the agent should do first.
   *
   * Required, and the only thing the user has to type: it becomes the
   * session's first user message and its title. The user types once.
   */
  prompt: string;
  repo: string;
  harness: HarnessKind;
  /** Whole-dollar budget limit, as the budget chip sets it. */
  budgetLimitDollars: number;
  /**
   * The machine to provision on. Omitted, flyco picks the cheapest
   * deployable Linux type from the caller's catalog that clears its size
   * floor, and records the session's machine as automatically chosen.
   */
  machine?: {
    providerAccount: string;
    machineType: string;
    region: string;
    spot: boolean;
    diskGib?: number;
  };
  /**
   * Spot preference when flyco picks the machine. Ignored when `machine`
   * names a type.
   */
  spot?: boolean;
}

export function requestNewSession(input: NewSessionInput): Promise<SessionDetail> {
  return createSession({
    prompt: input.prompt,
    repo: input.repo,
    harness: input.harness,
    budget_limit: dollarsToUsdMicros(input.budgetLimitDollars),
    ...(input.machine === undefined
      ? { spot: input.spot ?? true }
      : {
          machine: {
            provider_account: input.machine.providerAccount,
            machine_type: input.machine.machineType,
            region: input.machine.region,
            spot: input.machine.spot,
            ...(input.machine.diskGib === undefined
              ? {}
              : { disk_gib: input.machine.diskGib }),
          },
        }),
  });
}
