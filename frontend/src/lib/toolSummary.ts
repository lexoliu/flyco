/**
 * What a tool call did, in one line a person can read.
 *
 * A transcript that printed `Bash {"command":"cargo test --workspace"}` on
 * every row would make the reader do the parsing, and there are hundreds of
 * rows in a long session. docs/ux.md §9.2 asks for `Read src/main.rs`,
 * `Ran cargo test`, `Edited 3 files` instead: a verb and its object, with
 * the raw input still one click away behind the row.
 *
 * The two harnesses name their tools differently and neither promises a
 * schema, so this is written as a *fallback chain*, not a lookup table:
 *
 * 1. a verb for the tool name, matched case-insensitively against the names
 *    Claude Code and Codex actually use;
 * 2. an object pulled from whichever of the input's well-known keys is
 *    present, longest-lived names first;
 * 3. and, when neither is recognized, the tool's own name — which is always
 *    truer than a guess, and is what an MCP server's tool will land on.
 *
 * Pure and framework-free: no Solid, no DOM, so the summariser is unit
 * tested on its own (src/lib/toolSummary.test.ts) rather than through a
 * rendered transcript.
 */

/** How long a summary may get before it is cut short with an ellipsis. */
const MAX_SUMMARY_CHARS = 72;

/**
 * How one tool is described, in the two ways a finished call can end.
 *
 * Both forms are written out rather than derived from each other: English
 * has no rule that turns `Ran` into "did not run", and a transcript that
 * guessed would produce a sentence nobody wrote.
 */
interface ToolVerb {
  /** What the call did, when it worked: `Edited src/main.rs`. */
  done: string;
  /** What it did not do, when it failed: `Could not edit src/main.rs`. */
  failed: string;
}

/**
 * The verb each known tool is described by.
 *
 * Keys are lower-cased tool names. Both harnesses' built-ins are here;
 * anything else (an MCP server's tool, a harness that renames one) falls
 * through to the tool's own name rather than being mislabelled.
 */
const VERBS: ReadonlyMap<string, ToolVerb> = new Map([
  // Claude Code
  ["read", { done: "Read", failed: "Could not read" }],
  ["write", { done: "Wrote", failed: "Could not write" }],
  ["edit", { done: "Edited", failed: "Could not edit" }],
  ["multiedit", { done: "Edited", failed: "Could not edit" }],
  ["notebookedit", { done: "Edited", failed: "Could not edit" }],
  ["bash", { done: "Ran", failed: "Could not run" }],
  ["bashoutput", { done: "Read output of", failed: "Could not read output of" }],
  ["killshell", { done: "Stopped", failed: "Could not stop" }],
  ["glob", { done: "Searched for", failed: "Could not search for" }],
  ["grep", { done: "Searched for", failed: "Could not search for" }],
  ["webfetch", { done: "Fetched", failed: "Could not fetch" }],
  ["websearch", { done: "Searched the web for", failed: "Could not search the web for" }],
  ["task", { done: "Delegated", failed: "Could not delegate" }],
  ["todowrite", { done: "Updated the plan", failed: "Could not update the plan" }],
  ["skill", { done: "Used skill", failed: "Could not use skill" }],
  // Codex
  ["shell", { done: "Ran", failed: "Could not run" }],
  ["apply_patch", { done: "Edited", failed: "Could not edit" }],
  ["update_plan", { done: "Updated the plan", failed: "Could not update the plan" }],
  ["view_image", { done: "Viewed", failed: "Could not view" }],
]);

/** How an unrecognized tool's call is named when it failed. */
function unknownToolVerb(tool: string): ToolVerb {
  // The tool's own name still leads, because it remains the only true thing
  // about it; what changes is that the row says the call did not happen.
  return { done: tool, failed: `Could not call ${tool}` };
}

/**
 * Tools whose whole meaning is the verb: an object would repeat it.
 *
 * `TodoWrite` writes the plan; naming the plan again adds nothing, and the
 * input is a whole array of items that no one-line summary should try to
 * render.
 */
const VERB_ONLY: ReadonlySet<string> = new Set(["todowrite", "update_plan"]);

/**
 * Input keys that name what a tool acted on, in the order to prefer them.
 *
 * Ordered by specificity rather than alphabetically: a `Bash` call carries
 * both `command` and (sometimes) `description`, and the command is the
 * thing that ran.
 */
const OBJECT_KEYS: readonly string[] = [
  "command",
  "file_path",
  "path",
  "filePath",
  "notebook_path",
  "pattern",
  "query",
  "url",
  "name",
  "description",
  "prompt",
];

/** Whether a value is a plain object we can read keys off. */
function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * The object of the sentence: what the tool was pointed at.
 *
 * `null` when the input carries nothing nameable — an empty object, a
 * bare array, a tool whose arguments are all structured.
 */
function objectOf(input: unknown): string | null {
  if (typeof input === "string") {
    return collapse(input);
  }
  if (!isRecord(input)) {
    return null;
  }
  for (const key of OBJECT_KEYS) {
    const value = input[key];
    if (typeof value === "string" && value.trim() !== "") {
      return collapse(value);
    }
  }
  return null;
}

/**
 * How many files an edit touched, when the input says so.
 *
 * `MultiEdit` and Codex's `apply_patch` both act on more than one place at
 * once, and "Edited 3 files" is the honest summary of that where naming the
 * first path would be a half-truth.
 */
function fileCount(input: unknown): number | null {
  if (!isRecord(input)) {
    return null;
  }
  for (const key of ["edits", "changes", "files"]) {
    const value = input[key];
    if (Array.isArray(value) && value.length > 1) {
      return value.length;
    }
  }
  return null;
}

/** Squashes a value onto one line and trims it to a readable length. */
function collapse(value: string): string {
  const flat = value.replace(/\s+/g, " ").trim();
  return flat.length > MAX_SUMMARY_CHARS ? `${flat.slice(0, MAX_SUMMARY_CHARS - 1)}…` : flat;
}

/**
 * One line describing a tool call.
 *
 * Never empty and never a lie: an unrecognized tool with unreadable input
 * is summarised as its own name, which is exactly as much as the transcript
 * actually knows.
 *
 * `ok` is the outcome the harness reported — `false` for a call that
 * failed, `null` while it is still running. A failed call says so in the
 * verb rather than in a glyph at the end of the row: `Edited
 * crates/daemon/src/compaction.rs` beside a small ✕ is a line that reads as
 * a success to everyone who does not study the icon (issue #136).
 */
export function summarizeTool(tool: string, input: unknown, ok: boolean | null): string {
  const key = tool.toLowerCase();
  const verb = VERBS.get(key) ?? unknownToolVerb(tool);
  const word = ok === false ? verb.failed : verb.done;

  if (VERBS.has(key) && VERB_ONLY.has(key)) {
    return word;
  }

  const count = fileCount(input);
  if (count !== null && (key === "multiedit" || key === "apply_patch")) {
    return `${word} ${count} files`;
  }

  const object = objectOf(input);
  return object === null ? word : `${word} ${object}`;
}
