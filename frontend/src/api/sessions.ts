/**
 * Session creation, on top of the typed client.
 */
import {
  createSession,
  type HarnessKind,
  type ModelChoice,
  type Runtime,
  type SessionDetail,
} from "./client";
import { dollarsToUsdMicros } from "../lib/money";

/**
 * One repository a session is asked to work in, as the repository chip
 * records it. Order matters: the first is the session's primary
 * repository — the one the header names.
 */
export interface NewSessionRepo {
  repo: string;
  /**
   * Branch to work on. Omitted, the control plane records the repository's
   * default branch, so a session always names the branch each checkout is
   * on.
   */
  branch?: string;
}

export interface NewSessionInput {
  /**
   * What the agent should do first.
   *
   * Required, and the only thing the user has to type: it becomes the
   * session's first user message and its title. The user types once.
   */
  prompt: string;
  /**
   * The repositories the session works across, first is primary. Never
   * empty — a session with no repository has no workspace to work in.
   */
  repos: NewSessionRepo[];
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
    /**
     * Whether that type is a virtual machine or a managed container, as the
     * catalog entry it came from says. Sent because the control plane
     * checks it: a request whose runtime disagrees with the type it names
     * is a picker working from a catalog that has since changed, and it is
     * refused rather than provisioned as whatever is on offer.
     */
    runtime: Runtime;
    region: string;
    spot: boolean;
    diskGib?: number;
  };
  /**
   * Spot preference when flyco picks the machine. Ignored when `machine`
   * names a type.
   */
  spot?: boolean;
  /**
   * Whether the session's machine gets a desktop the agent can drive —
   * display server, encoder, the `computer_*` tools. Omitted is off: a
   * desktop is provisioned only for a session that asked.
   */
  computerUse?: boolean;
}

export function requestNewSession(input: NewSessionInput): Promise<SessionDetail> {
  return createSession({
    prompt: input.prompt,
    // A branch is sent only when the user picked one: omitted, the control
    // plane reads each repository's default from GitHub and records *that*,
    // so the branch a session is on is never this browser's guess.
    repos: input.repos.map(({ repo, branch }) =>
      branch === undefined ? { repo } : { repo, branch },
    ),
    harness: input.harness,
    ...(input.model === undefined ? {} : { model: input.model }),
    budget_limit: dollarsToUsdMicros(input.budgetLimitDollars),
    ...(input.computerUse === undefined
      ? {}
      : { computer_use: input.computerUse }),
    ...(input.machine === undefined
      ? { spot: input.spot ?? true }
      : {
          machine: {
            provider_account: input.machine.providerAccount,
            machine_type: input.machine.machineType,
            runtime: input.machine.runtime,
            region: input.machine.region,
            spot: input.machine.spot,
            ...(input.machine.diskGib === undefined
              ? {}
              : { disk_gib: input.machine.diskGib }),
          },
        }),
  });
}
