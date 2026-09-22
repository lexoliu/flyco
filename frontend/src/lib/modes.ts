/**
 * The permission mode a session runs under (docs/ux.md §9.3).
 *
 * One union across the harnesses: Claude takes the modes natively
 * (`setPermissionMode`), Codex takes the same six as the
 * approval/sandbox pair each maps to — so the picker is the product's
 * list, not either harness's. What differs per harness is which entries
 * are offered: `dontAsk` means "refuse what isn't allowed" and on Codex
 * that is exactly what `plan` already maps to, so a Codex picker does
 * not carry two rows that do one thing.
 */
import type { components } from "../api/schema";
import type { PermissionMode } from "../api/wire";

type HarnessKind = components["schemas"]["HarnessKind"];

/**
 * The mark a mode carries, as a name its picker resolves to a glyph.
 *
 * A row of five sentences is read word by word; a row of five marks is
 * read at a glance, and the mark is what the chip then carries so the
 * closed control says which one is on without being read at all.
 */
export type ModeIcon = "auto" | "ask" | "read" | "edits" | "open" | "refuse";

/** A row in the mode picker: the mode's id and what the row says. */
export interface ModeOption {
  id: PermissionMode;
  /** The name the chip and the picker row carry. */
  label: string;
  /** What the mode does, in the picker's second line. */
  description: string;
  /** Which mark stands beside it. */
  icon: ModeIcon;
  /** Whether the mode gives everything away, and is coloured for it. */
  unrestricted?: boolean;
}

const AUTO: ModeOption = {
  id: "auto",
  label: "Auto",
  description: "The agent decides what needs asking.",
  icon: "auto",
};
/**
 * The harness's own rules, which for every harness flyco drives means
 * asking before anything that is not already allowed.
 *
 * Named for what it does rather than for its place in the list: `Default`
 * told the user only that somebody else had decided, which is not a
 * choice anybody can make.
 */
const ASK: ModeOption = {
  id: "default",
  label: "Ask",
  description: "Anything not already allowed asks first.",
  icon: "ask",
};
const PLAN: ModeOption = {
  id: "plan",
  label: "Plan",
  description: "Read-only: it researches and proposes.",
  icon: "read",
};
const ACCEPT_EDITS: ModeOption = {
  id: "acceptEdits",
  label: "Accept edits",
  description: "Edits apply; commands still ask.",
  icon: "edits",
};
const BYPASS: ModeOption = {
  id: "bypassPermissions",
  label: "Yolo",
  description: "Nothing asks. Every command and edit runs.",
  icon: "open",
  unrestricted: true,
};
const DONT_ASK: ModeOption = {
  id: "dontAsk",
  label: "Don't ask",
  description: "Anything not already allowed is refused.",
  icon: "refuse",
};

const MODES: readonly ModeOption[] = [AUTO, ASK, PLAN, ACCEPT_EDITS, BYPASS, DONT_ASK];

/**
 * The modes a harness's picker offers, in menu order.
 *
 * Codex expresses every mode as an approval/sandbox pair and Devin floors
 * it to `plan`, so for both `dontAsk` is a second row for `plan`'s
 * behavior and the list stops at five.
 */
export function modesFor(harness: HarnessKind): ModeOption[] {
  if (harness === "codex" || harness === "devin") {
    return MODES.filter((mode) => mode.id !== "dontAsk");
  }
  return [...MODES];
}

/**
 * What a mode is called on screen — the chip's text and the transcript
 * line's name for it. An id the catalog does not know is shown as the
 * id: a mode from a newer control plane reads as exactly what it is.
 */
export function modeLabel(mode: PermissionMode): string {
  return MODES.find((option) => option.id === mode)?.label ?? mode;
}
