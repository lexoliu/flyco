import { describe, expect, it } from "vitest";
import { foldTranscript } from "../lib/transcript";
import type { ClientEvent } from "../api/wire";

describe("compaction transcript notices", () => {
  it("shows successful and failed compaction results", () => {
    const events: ClientEvent[] = [
      { type: "harness", event: { type: "context_compacted" } },
      {
        type: "harness",
        event: { type: "context_compaction_failed", error: "summary request failed" },
      },
    ];

    expect(foldTranscript(events)).toEqual([
      { kind: "notice", key: "notice-0", text: "Context compacted." },
      {
        kind: "notice",
        key: "notice-1",
        text: "Context compaction failed: summary request failed",
      },
    ]);
  });
});
