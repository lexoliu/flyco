import { describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@solidjs/testing-library";
import EffortChip from "./EffortChip";
import type { ModelOption } from "../api/client";

/** The list Codex's app-server reports, plus a model that takes no effort. */
const MODELS: ModelOption[] = [
  {
    id: "gpt-5.6-terra",
    label: "GPT-5.6-Terra",
    description: "Balanced agentic coding model for everyday work.",
    is_default: true,
    efforts: ["low", "medium", "high", "xhigh", "max"],
    default_effort: "medium",
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

describe("EffortChip", () => {
  it("names the control when the model's own effort stands, the level once chosen", () => {
    const { getByRole } = render(() => (
      <EffortChip models={MODELS} choice={{ model: "gpt-5.6-terra" }} onChoose={vi.fn()} />
    ));
    expect(getByRole("button", { name: "Effort" })).toBeInTheDocument();
  });

  it("reads the chosen level as a word, never the id", () => {
    const { getByRole, queryByText } = render(() => (
      <EffortChip
        models={MODELS}
        choice={{ model: "gpt-5.6-terra", effort: "xhigh" }}
        onChoose={vi.fn()}
      />
    ));
    expect(getByRole("button", { name: "Extra high" })).toBeInTheDocument();
    expect(queryByText("xhigh")).not.toBeInTheDocument();
  });

  it("is absent for a model that names no effort levels", () => {
    const { queryByRole } = render(() => (
      <EffortChip models={MODELS} choice={{ model: "haiku" }} onChoose={vi.fn()} />
    ));
    expect(queryByRole("button", { name: "Effort" })).not.toBeInTheDocument();
  });

  it("lays the model's levels on a rail with `Default` as the first stop", async () => {
    const { getByRole, findByRole } = render(() => (
      <EffortChip
        models={MODELS}
        choice={{ model: "gpt-5.6-terra", effort: "high" }}
        onChoose={vi.fn()}
      />
    ));

    getByRole("button", { name: "High" }).click();

    const slider = await findByRole("slider", { name: "Effort" });
    // Five levels plus `Default`, and the thumb sits on the chosen one.
    expect(slider).toHaveAttribute("max", "5");
    expect(slider).toHaveValue("3");
    expect(slider).toHaveAttribute("aria-valuetext", "High");
  });

  it("names the model's default where the harness says it, under the `Default` reading", async () => {
    const { getByRole, findByRole, findByText } = render(() => (
      <EffortChip models={MODELS} choice={{ model: "gpt-5.6-terra" }} onChoose={vi.fn()} />
    ));

    getByRole("button", { name: "Effort" }).click();

    const slider = await findByRole("slider", { name: "Effort" });
    expect(slider).toHaveValue("0");
    expect(slider).toHaveAttribute("aria-valuetext", "Default");
    expect(await findByText("GPT-5.6-Terra · Medium")).toBeInTheDocument();
  });

  it("chooses the level a stop lands on, and `Default` hands the choice back", async () => {
    const onChoose = vi.fn();
    const { getByRole, findByRole } = render(() => (
      <EffortChip
        models={MODELS}
        choice={{ model: "gpt-5.6-terra", effort: "medium" }}
        onChoose={onChoose}
      />
    ));

    getByRole("button", { name: "Medium" }).click();
    const slider = await findByRole("slider", { name: "Effort" });

    fireEvent.input(slider, { target: { value: "4" } });
    expect(onChoose).toHaveBeenCalledWith({ model: "gpt-5.6-terra", effort: "xhigh" });

    fireEvent.input(slider, { target: { value: "0" } });
    expect(onChoose).toHaveBeenCalledWith({ model: "gpt-5.6-terra" });
  });

  it("steps by detent from the keyboard", async () => {
    const onChoose = vi.fn();
    const { getByRole, findByRole } = render(() => (
      <EffortChip models={MODELS} choice={{ model: "gpt-5.6-terra" }} onChoose={onChoose} />
    ));

    getByRole("button", { name: "Effort" }).click();
    const slider = await findByRole("slider", { name: "Effort" });

    fireEvent.keyDown(slider, { key: "ArrowRight" });
    expect(onChoose).toHaveBeenCalledWith({ model: "gpt-5.6-terra", effort: "low" });
  });
});
