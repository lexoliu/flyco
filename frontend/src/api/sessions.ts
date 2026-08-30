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
  /**
   * The machine to provision on. Omitted, flyco picks the cheapest
   * deployable Linux type from the caller's catalog.
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
