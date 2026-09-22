import { describe, expect, it } from "vitest";
import type { ModelOption } from "../api/client";
import { choiceLabel, defaultChoice, effortLabel, optionOf, sameChoice, shortName } from "./models";

/** The list the Claude Agent SDK reports on a Max plan, abridged. */
const CLAUDE: ModelOption[] = [
  {
    id: "default",
    label: "Default (recommended)",
    description: "Opus 5 with 1M context · Best for everyday, complex tasks",
    is_default: true,
    efforts: ["low", "medium", "high", "xhigh", "max"],
    default_effort: null,
  },
  {
    id: "claude-fable-5-1[1m]",
    label: "Fable",
    description: "Fable 5.1 · Most capable for your hardest and longest-running tasks",
    is_default: false,
    efforts: ["low", "medium", "high", "xhigh", "max"],
    default_effort: null,
  },
  {
    id: "haiku",
    label: "Haiku",
    description: "Haiku 4.5 · Fastest for quick answers",
    is_default: false,
    efforts: [],
    default_effort: null,
  },
];

describe("defaultChoice", () => {
  it("opens on the entry the harness marks as its default, at its opening level", () => {
    expect(defaultChoice(CLAUDE)).toEqual({ model: "default", effort: "medium" });
  });

  it("opens on the first entry when the harness marks none", () => {
    const unmarked = CLAUDE.map((option) => ({ ...option, is_default: false }));
    expect(defaultChoice(unmarked)).toEqual({ model: "default", effort: "medium" });
  });

  it("refuses an empty list rather than choosing nothing", () => {
    expect(() => defaultChoice([])).toThrow(/no models/);
  });
});

describe("shortName", () => {
  it("reads the model off the head of a Claude description", () => {
    expect(shortName(CLAUDE[1]!)).toBe("Fable 5.1");
    expect(shortName(CLAUDE[0]!)).toBe("Opus 5");
  });

  it("falls back to the label when the description is a sentence", () => {
    expect(
      shortName({
        id: "gpt-5.5",
        label: "GPT-5.5",
        description: "Proven previous-generation model for coding and general work.",
        is_default: false,
        efforts: [],
        default_effort: null,
      }),
    ).toBe("GPT-5.5");
  });
});

describe("choiceLabel", () => {
  it("names the model and the effort when one is chosen", () => {
    expect(choiceLabel(CLAUDE, { model: "claude-fable-5-1[1m]", effort: "high" })).toBe(
      "Fable 5.1 · High",
    );
  });

  it("names the model alone when the effort is the harness's own", () => {
    expect(choiceLabel(CLAUDE, { model: "haiku" })).toBe("Haiku 4.5");
  });

  it("spells an effort the harness abbreviates as a word", () => {
    expect(effortLabel("xhigh")).toBe("Extra high");
    expect(effortLabel("ultra")).toBe("Ultra");
  });

  it("shows an id the list no longer carries as itself", () => {
    expect(choiceLabel(CLAUDE, { model: "opus-4" })).toBe("opus-4");
    expect(optionOf(CLAUDE, { model: "opus-4" })).toBeUndefined();
  });
});

describe("sameChoice", () => {
  it("treats a missing effort and a null one as the same choice", () => {
    expect(sameChoice({ model: "haiku" }, { model: "haiku", effort: null })).toBe(true);
    expect(sameChoice({ model: "haiku" }, { model: "haiku", effort: "low" })).toBe(false);
  });
});
