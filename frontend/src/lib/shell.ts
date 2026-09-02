/**
 * The composer's `!` prefix, and how a run of it reads (docs/ux.md §9.3).
 *
 * A message beginning with `!` is a command for the session's machine
 * rather than a prompt for the agent: it is run in bash, in the session's
 * working directory, and the agent is never told it happened. Everything
 * about that convention lives here — the prefix the composer labels, the
 * split the session page routes on, and the sentence a finished run is read
 * by — so the three can never disagree about what a `!` message is.
 */
import type { ShellOutcome } from "../api/wire";

/** The prefix that sends a message to the machine's bash instead of the agent. */
export const BASH_PREFIX = "!";

/**
 * The command in a composer message, or `null` when it is a prompt.
 *
 * The `!` is interface, not shell: what runs is what follows it. A message
 * that is only `!` is nothing to run, and is treated as a prompt rather
 * than as an empty command the machine would answer with an empty row.
 */
export function shellCommandIn(message: string): string | null {
  if (!message.startsWith(BASH_PREFIX)) {
    return null;
  }
  const command = message.slice(BASH_PREFIX.length).trim();
  return command === "" ? null : command;
}

/**
 * What a finished run says about itself.
 *
 * Every way a command can end has its own sentence, including the three
 * where it never ran: "nothing happened" is the one thing a person watching
 * a command they typed must never be left to conclude on their own.
 */
export function shellOutcomeLabel(outcome: ShellOutcome): string {
  switch (outcome.kind) {
    case "exited":
      return `Exit ${outcome.code}`;
    case "signalled":
      return "Killed by a signal";
    case "timed_out":
      return `Timed out after ${outcome.after_seconds}s`;
    case "cancelled":
      return "Stopped";
    case "offline":
      return "Not run — the machine was not connected";
    case "busy":
      return "Not run — another command was still running";
    case "refused":
      return "Not run — the session is not accepting commands";
    case "failed":
      return `Could not run: ${outcome.error}`;
  }
}

/** Whether a run ended the way the user hoped, which is what colours it. */
export function shellSucceeded(outcome: ShellOutcome): boolean {
  return outcome.kind === "exited" && outcome.code === 0;
}
