/**
 * What a tool call opens into, in a form written for a person.
 *
 * The transcript used to print `JSON.stringify(input, null, 2)` behind the
 * disclosure. That is the wire format of a conversation between two
 * programs: braces, quoted keys, and `\"` around every shell quote the
 * command actually contained. A person reading their agent's work should
 * never be handed the transport (docs/ux.md §9.2).
 *
 * The harnesses already send everything needed to do better. A `Bash` call
 * carries a `description` written in plain English for exactly this purpose
 * — it leads the collapsed row ({@link summarizeTool}) — and a `command`
 * that is *shell*, which belongs in a shell code block. Everything else is
 * named values, so it is rendered as named values.
 *
 * Pure and framework-free, like {@link summarizeTool}: no Solid, no DOM.
 */

/** A code block: source in a language, to be highlighted as that language. */
export interface ToolCode {
  /** A `highlight.js` language name. */
  language: string;
  text: string;
}

/** One named value from the call: `File · src/main.rs`. */
export interface ToolField {
  label: string;
  value: string;
}

/** Everything a tool call shows when it is opened. */
export interface ToolDetail {
  /** The call's own source, when the call is one. */
  code: ToolCode | null;
  /** Named values, in the order they are worth reading. */
  fields: ToolField[];
}

/** How deep a nested input is flattened before it is counted instead. */
const MAX_DEPTH = 2;

/** How long a single field value may get before it is cut short. */
const MAX_FIELD_CHARS = 400;

/**
 * Keys already spoken for by the collapsed row or by the code block, so
 * repeating them under the disclosure would say the same thing twice.
 */
const SPOKEN_FOR: ReadonlySet<string> = new Set(["description", "command"]);

/**
 * The language a tool's own source is written in.
 *
 * Only tools whose input really is source: a shell command is shell, a
 * patch is a diff. Everything else has named values and no source at all,
 * and inventing a language for it would highlight prose.
 */
const CODE_KEYS: ReadonlyMap<string, { key: string; language: string }> =
  new Map([
    ["bash", { key: "command", language: "bash" }],
    ["shell", { key: "command", language: "bash" }],
    ["bashoutput", { key: "bash_id", language: "bash" }],
    ["killshell", { key: "shell_id", language: "bash" }],
    ["apply_patch", { key: "patch", language: "diff" }],
  ]);

/** Labels for the keys both harnesses use, so a field is not `file_path`. */
const LABELS: Readonly<Record<string, string>> = {
  file_path: "File",
  filePath: "File",
  notebook_path: "Notebook",
  path: "Path",
  pattern: "Pattern",
  query: "Query",
  url: "URL",
  prompt: "Prompt",
  content: "Content",
  old_string: "Replacing",
  new_string: "With",
  offset: "From line",
  limit: "Lines",
  timeout: "Timeout",
  glob: "Files",
  output_mode: "Output",
  subagent_type: "Agent",
  replace_all: "Every occurrence",
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** `subagent_type` → `Subagent type`, for a key nobody has named yet. */
function labelFor(key: string): string {
  const known = LABELS[key];
  if (known !== undefined) {
    return known;
  }
  const words = key.replace(/[_-]+/gu, " ").replace(/([a-z])([A-Z])/gu, "$1 $2");
  return words.charAt(0).toUpperCase() + words.slice(1).toLowerCase();
}

/** A scalar as text, or `null` when the value is not one. */
function scalar(value: unknown): string | null {
  if (typeof value === "string") {
    return value;
  }
  if (typeof value === "number" || typeof value === "boolean") {
    return String(value);
  }
  return null;
}

function trim(value: string): string {
  return value.length > MAX_FIELD_CHARS
    ? `${value.slice(0, MAX_FIELD_CHARS - 1)}…`
    : value;
}

/** `3 items`, `2 files` — what an unreadable structure amounts to. */
function count(size: number, noun: string): string {
  return `${size} ${noun}${size === 1 ? "" : "s"}`;
}

/**
 * Flattens one value into named fields.
 *
 * Nested objects keep their path (`Edits · 2 items`), arrays of scalars are
 * joined, and anything deeper than {@link MAX_DEPTH} is counted rather than
 * unfolded: a field list forty rows long is the JSON dump again with the
 * braces taken out.
 */
function collect(
  value: unknown,
  label: string,
  depth: number,
  into: ToolField[],
): void {
  const flat = scalar(value);
  if (flat !== null) {
    if (flat.trim() !== "") {
      into.push({ label, value: trim(flat) });
    }
    return;
  }

  if (Array.isArray(value)) {
    if (value.length === 0) {
      return;
    }
    const scalars = value.map(scalar);
    if (scalars.every((entry) => entry !== null)) {
      into.push({ label, value: trim(scalars.join(", ")) });
      return;
    }
    into.push({ label, value: count(value.length, "item") });
    return;
  }

  if (!isRecord(value)) {
    return;
  }
  const keys = Object.keys(value);
  if (depth >= MAX_DEPTH) {
    into.push({ label, value: count(keys.length, "field") });
    return;
  }
  for (const key of keys) {
    collect(value[key], `${label} · ${labelFor(key)}`, depth + 1, into);
  }
}

/**
 * What one tool call shows when it is opened.
 *
 * Never the raw input. A call whose input is a bare string is treated as
 * that tool's own source only when the tool is one that takes source;
 * otherwise it is a single unnamed field, which is still a sentence rather
 * than a document.
 */
export function detailOfTool(tool: string, input: unknown): ToolDetail {
  const code = CODE_KEYS.get(tool.toLowerCase());
  const fields: ToolField[] = [];

  if (typeof input === "string") {
    return code === undefined
      ? { code: null, fields: [{ label: labelFor(tool), value: trim(input) }] }
      : { code: { language: code.language, text: input }, fields };
  }

  if (!isRecord(input)) {
    return { code: null, fields };
  }

  let block: ToolCode | null = null;
  if (code !== undefined) {
    const source = input[code.key];
    const text =
      typeof source === "string"
        ? source
        : // Codex sends a shell call as `["bash", "-lc", "…"]`; the last
          // element is the script and the rest is how it was invoked.
          Array.isArray(source) && source.every((part) => typeof part === "string")
          ? source.join(" ")
          : null;
    if (text !== null && text.trim() !== "") {
      block = { language: code.language, text };
    }
  }

  for (const key of Object.keys(input)) {
    if (SPOKEN_FOR.has(key) && block !== null) {
      continue;
    }
    if (key === "description") {
      // Said by the collapsed row, whether or not there is a code block.
      continue;
    }
    collect(input[key], labelFor(key), 0, fields);
  }

  return { code: block, fields };
}
