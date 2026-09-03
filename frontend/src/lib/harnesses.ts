import type { HarnessKind } from "../api/client";

/** What each harness is called wherever the app names one. */
export const HARNESS_LABEL: Record<HarnessKind, string> = {
  claude_code: "Claude Code",
  codex: "Codex",
};
