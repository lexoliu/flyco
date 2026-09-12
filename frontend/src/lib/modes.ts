/**
 * The permission mode a session runs under (docs/ux.md §9.3).
 *
 * One union across both harnesses: Claude takes the modes natively
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

/** A row in the mode picker: the mode's id and what the row says. */
export interface ModeOption {
  id: PermissionMode;
  /** The name the chip and the picker row carry. */
  label: string;
  /** What the mode does, in the picker's second line. */
  description: string;
}

const AUTO: ModeOption = {
  id: "auto",
  label: "Auto",
  description: "The agent decides what needs asking; routine work runs through.",
};
const DEFAULT: ModeOption = {
  id: "default",
  label: "Default",
  description: "The harness's own permission rules decide, unchanged.",
};
const PLAN: ModeOption = {
  id: "plan",
  label: "Plan",
  description: "Read-only. The agent researches and proposes; nothing changes.",
};
const ACCEPT_EDITS: ModeOption = {
  id: "acceptEdits",
  label: "Accept edits",
  description: "File edits apply without asking; commands still ask.",
};
const BYPASS: ModeOption = {
  id: "bypassPermissions",
  label: "Yolo",
  description: "Nothing asks. Every command and edit runs immediately.",
};
const DONT_ASK: ModeOption = {
  id: "dontAsk",
  label: "Don't ask",
  description: "Whatever is not already allowed is refused instead of asked.",
};

const MODES: readonly ModeOption[] = [AUTO, DEFAULT, PLAN, ACCEPT_EDITS, BYPASS, DONT_ASK];

/**
 * The modes a harness's picker offers, in menu order.
 *
 * Codex expresses every mode as an approval/sandbox pair, and `dontAsk`
 * maps to the same pair `plan` does — offering it would be two rows for
 * one behavior, so Codex's list stops at five.
 */
export function modesFor(harness: HarnessKind): ModeOption[] {
  if (harness === "codex") {
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
