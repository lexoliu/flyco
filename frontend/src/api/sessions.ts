/**
 * Session creation, on top of the typed client.
 */
import { createSession, type HarnessKind, type ModelChoice, type SessionDetail } from "./client";
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
  /**
   * Branch to work on. Omitted, the control plane records the repository's
   * default branch, so a session always names the branch it is on.
   */
  branch?: string;
  harness: HarnessKind;
  /**
   * The model the agent runs, and at what effort. Omitted, the session
   * opens on the harness's own default.
   */
  model?: ModelChoice;
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
    ...(input.branch === undefined ? {} : { branch: input.branch }),
    harness: input.harness,
    ...(input.model === undefined ? {} : { model: input.model }),
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
