/**
 * Reading an approval out loud.
 *
 * An approval card asks the user to allow one specific operation, so the
 * card's whole job is to say *which* — and that is a different sentence for
 * every shape of `ApprovalPayload`. Turning the payload into those two
 * strings is pure, so it lives here rather than inside the component: it is
 * the part with rules worth testing, and the card is layout.
 *
 * The rules that are not obvious:
 *
 * - A machine resize onto a license-bound type quotes the **charge in
 *   dollars**, not the hours. `24 hours` is a fact about an Apple licence;
 *   `$15.60` is what pressing Approve costs, and it is the number the user
 *   is actually deciding about (docs/ux.md §7.7, §9.5).
 * - The agent's own reason is shown verbatim. The user is being asked to
 *   spend money on the agent's say-so, and "because it said so" is not a
 *   basis for a decision.
 * - A tool call's input is read out as the arguments it is: one row per
 *   key, the value as it was written. A pretty-printed JSON object asks the
 *   reader to find the shell command among the braces, and the command is
 *   the whole decision (issue #136).
 */
import type { ApprovalPayload } from "../api/wire";
import { formatUsd } from "./money";

/** One named argument of a tool call, as the card reads it out. */
export interface OperationField {
  /** The input's key, as the harness wrote it. */
  name: string;
  /**
   * The value: a string exactly as it was given, anything else as JSON.
   *
   * A shell command is a string, and quoting and re-indenting it would
   * change the very text the user is being asked to allow.
   */
  value: string;
  /** Whether the value runs over more than one line, and needs a block. */
  block: boolean;
}

/**
 * Exactly what would happen, in whichever shape says it best.
 *
 * `text` is a sentence or two the card wrote itself; `fields` is a set of
 * arguments the card is reading out of a payload it did not write.
 */
export type OperationDetail =
  | { kind: "text"; text: string }
  | { kind: "fields"; fields: OperationField[] };

/** What one approval card says. */
export interface AskedOperation {
  /** The operation, named in a few words. */
  title: string;
  /** Exactly what would happen, in full. */
  detail: OperationDetail;
}

/** Whether a value is a plain object whose keys can be read out. */
function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** One field per key, strings verbatim and everything else as JSON. */
function fieldsOf(input: Record<string, unknown>): OperationField[] {
  return Object.entries(input).map(([name, value]) => {
    const written = typeof value === "string" ? value : JSON.stringify(value, null, 2);
    return { name, value: written, block: written.includes("\n") };
  });
}

/**
 * A tool call's input, read out as its arguments.
 *
 * An input that is not an object has no keys to name — a bare string, an
 * array — so it is shown as the one value it is rather than invented into
 * a field list.
 */
function toolInput(input: unknown): OperationDetail {
  if (isRecord(input)) {
    const fields = fieldsOf(input);
    if (fields.length > 0) {
      return { kind: "fields", fields };
    }
    // An empty object is a tool called with no arguments, and `{}` on the
    // card says that more plainly than an empty list would.
  }
  return {
    kind: "text",
    text: typeof input === "string" ? input : JSON.stringify(input, null, 2),
  };
}

/** A detail the card wrote itself, as one block of text. */
function text(...lines: string[]): OperationDetail {
  return { kind: "text", text: lines.join("\n") };
}

/** Turns one approval payload into the exact operation being asked for. */
export function operation(payload: ApprovalPayload): AskedOperation {
  switch (payload.kind) {
    case "merge":
      return {
        title: "Merge a branch",
        detail: text(`${payload.repo}: ${payload.from_branch} → ${payload.into_branch}`),
      };
    case "history_rewrite":
      return {
        title: "Rewrite history",
        detail: text(`${payload.repo} on ${payload.branch}: ${payload.description}`),
      };
    case "agents_md_change":
      return {
        title: "Change AGENTS.md",
        detail: text(`Replace "${payload.find}" with "${payload.replace}"`),
      };
    case "machine_resize_license_bound":
      return {
        title: `Switch to ${payload.machine_type}`,
        detail: text(
          `Starts a ${payload.minimum.hours}-hour minimum charge of ${formatUsd(
            payload.minimum.charge,
          )} the moment it boots.`,
          `The agent says: ${payload.reason}`,
          "Resizing restarts the machine; the disk is kept.",
        ),
      };
    case "tool_use":
      return { title: `Run ${payload.tool}`, detail: toolInput(payload.input) };
  }
}
