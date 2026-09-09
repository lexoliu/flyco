import { describe, expect, it, vi } from "vitest";
import { render } from "@solidjs/testing-library";
import ModelChip from "./ModelChip";
import type { ModelOption } from "../api/client";

/** The list Codex's app-server reports, as flyco reads it. */
const CODEX: ModelOption[] = [
  {
    id: "gpt-5.6-terra",
    label: "GPT-5.6-Terra",
    description: "Balanced agentic coding model for everyday work.",
    is_default: true,
    efforts: ["low", "medium", "high", "xhigh", "max", "ultra"],
    default_effort: "medium",
  },
  {
    id: "gpt-5.5",
    label: "GPT-5.5",
    description: "Proven previous-generation model for coding and general work.",
    is_default: false,
    efforts: ["low", "medium", "high", "xhigh"],
    default_effort: "medium",
  },
];

describe("ModelChip", () => {
  it("reads the model's name and the chosen effort, never the id", () => {
    const { getByRole, queryByText } = render(() => (
      <ModelChip models={CODEX} choice={{ model: "gpt-5.5", effort: "high" }} onChoose={vi.fn()} />
    ));

    expect(getByRole("button", { name: "GPT-5.5 · High" })).toBeInTheDocument();
    expect(queryByText("gpt-5.5")).not.toBeInTheDocument();
  });

  it("lists every model with the harness's own description, and marks the chosen one", async () => {
    const { getByRole, findByRole } = render(() => (
      <ModelChip models={CODEX} choice={{ model: "gpt-5.6-terra" }} onChoose={vi.fn()} />
    ));

    getByRole("button", { name: "GPT-5.6-Terra" }).click();

    const chosen = await findByRole("option", { name: /GPT-5.6-Terra/ });
    expect(chosen).toHaveAttribute("aria-selected", "true");
    expect(chosen).toHaveTextContent("Balanced agentic coding model for everyday work.");
    expect(getByRole("option", { name: /GPT-5.5/ })).toHaveAttribute("aria-selected", "false");
  });

  it("starts a new model on its own default effort rather than carrying the old one", async () => {
    const onChoose = vi.fn();
    const { getByRole, findByRole } = render(() => (
      <ModelChip
        models={CODEX}
        choice={{ model: "gpt-5.6-terra", effort: "ultra" }}
        onChoose={onChoose}
      />
    ));

    getByRole("button", { name: "GPT-5.6-Terra · Ultra" }).click();
    (await findByRole("option", { name: /GPT-5.5/ })).click();

    expect(onChoose).toHaveBeenCalledWith({ model: "gpt-5.5" });
  });

  it("offers the chosen model's effort levels as words, with the harness default named", async () => {
    const onChoose = vi.fn();
    const { getByRole, findByRole } = render(() => (
      <ModelChip models={CODEX} choice={{ model: "gpt-5.5" }} onChoose={onChoose} />
    ));

    getByRole("button", { name: "GPT-5.5" }).click();

    expect(await findByRole("button", { name: "Default Medium" })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    getByRole("button", { name: "Extra high" }).click();
    expect(onChoose).toHaveBeenCalledWith({ model: "gpt-5.5", effort: "xhigh" });
  });
});
