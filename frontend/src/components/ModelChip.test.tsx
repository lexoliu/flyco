import { describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@solidjs/testing-library";
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

/** A list carrying a model that takes no effort. */
const WITH_EFFORTLESS: ModelOption[] = [
  ...CODEX,
  {
    id: "haiku",
    label: "Haiku",
    description: "Haiku 4.5 · Fastest for quick answers",
    is_default: false,
    efforts: [],
    default_effort: null,
  },
];

describe("ModelChip", () => {
  it("reads the model's name and chosen level as words, never the ids", () => {
    const { getByRole, queryByText } = render(() => (
      <ModelChip models={CODEX} choice={{ model: "gpt-5.5", effort: "xhigh" }} onChoose={vi.fn()} />
    ));

    expect(getByRole("button", { name: "GPT-5.5 · Extra high" })).toBeInTheDocument();
    expect(queryByText("xhigh")).not.toBeInTheDocument();
  });

  it("reads just the model where the model takes no level at all", () => {
    const { getByRole } = render(() => (
      <ModelChip models={WITH_EFFORTLESS} choice={{ model: "haiku" }} onChoose={vi.fn()} />
    ));
    expect(getByRole("button", { name: "Haiku 4.5" })).toBeInTheDocument();
  });

  it("opens on the chosen model's effort rail, the list behind it", async () => {
    const { getByRole, findByRole, queryByRole, findByText } = render(() => (
      <ModelChip
        models={CODEX}
        choice={{ model: "gpt-5.6-terra", effort: "high" }}
        onChoose={vi.fn()}
      />
    ));

    getByRole("button", { name: "GPT-5.6-Terra · High" }).click();

    const slider = await findByRole("slider", { name: "Effort" });
    expect(slider).toHaveAttribute("max", "5");
    expect(slider).toHaveValue("2");
    expect(slider).toHaveAttribute("aria-valuetext", "High");
    expect(await findByText("GPT-5.6-Terra")).toBeInTheDocument();
    expect(queryByRole("option")).not.toBeInTheDocument();
  });

  it("opens on the list when the chosen model takes no effort", async () => {
    const { getByRole, findByRole, queryByRole } = render(() => (
      <ModelChip models={WITH_EFFORTLESS} choice={{ model: "haiku" }} onChoose={vi.fn()} />
    ));

    getByRole("button", { name: "Haiku 4.5" }).click();

    expect(await findByRole("option", { name: /Haiku/ })).toBeInTheDocument();
    expect(queryByRole("slider")).not.toBeInTheDocument();
  });

  it("lists every model behind the level link, and marks the chosen one", async () => {
    const { getByRole, findByRole } = render(() => (
      <ModelChip models={WITH_EFFORTLESS} choice={{ model: "gpt-5.6-terra" }} onChoose={vi.fn()} />
    ));

    getByRole("button", { name: "GPT-5.6-Terra" }).click();
    (await findByRole("button", { name: "Choose a model" })).click();

    const chosen = await findByRole("option", { name: /GPT-5.6-Terra/ });
    expect(chosen).toHaveAttribute("aria-selected", "true");
    expect(chosen).toHaveTextContent("Balanced agentic coding model for everyday work.");
    expect(getByRole("option", { name: /Haiku/ })).toHaveAttribute("aria-selected", "false");
  });

  it("starts a new model on its own opening level rather than carrying the old one", async () => {
    const onChoose = vi.fn();
    const { getByRole, findByRole } = render(() => (
      <ModelChip
        models={CODEX}
        choice={{ model: "gpt-5.6-terra", effort: "ultra" }}
        onChoose={onChoose}
      />
    ));

    getByRole("button", { name: "GPT-5.6-Terra · Ultra" }).click();
    (await findByRole("button", { name: "Choose a model" })).click();
    (await findByRole("option", { name: /GPT-5.5/ })).click();

    expect(onChoose).toHaveBeenCalledWith({ model: "gpt-5.5", effort: "medium" });
  });

  it("lands on the picked model's rail rather than closing", async () => {
    const onChoose = vi.fn();
    const { getByRole, findByRole, queryByRole } = render(() => (
      <ModelChip
        models={CODEX}
        choice={{ model: "gpt-5.6-terra", effort: "high" }}
        onChoose={onChoose}
      />
    ));

    getByRole("button", { name: "GPT-5.6-Terra · High" }).click();
    (await findByRole("button", { name: "Choose a model" })).click();
    (await findByRole("option", { name: /GPT-5.5/ })).click();

    expect(onChoose).toHaveBeenCalledWith({ model: "gpt-5.5", effort: "medium" });
    // The panel is still open on the picked model's own four levels, with
    // the thumb on the level it opens at — the choice has not yet answered
    // the pick, and the rail reads what the run would use.
    const slider = await findByRole("slider", { name: "Effort" });
    expect(slider).toHaveAttribute("max", "3");
    expect(slider).toHaveValue("1");
    expect(slider).toHaveAttribute("aria-valuetext", "Medium");
    expect(queryByRole("option")).not.toBeInTheDocument();
  });

  it("returns to the chosen model's rail without resetting the effort it runs at", async () => {
    const onChoose = vi.fn();
    const { getByRole, findByRole } = render(() => (
      <ModelChip
        models={CODEX}
        choice={{ model: "gpt-5.6-terra", effort: "high" }}
        onChoose={onChoose}
      />
    ));

    getByRole("button", { name: "GPT-5.6-Terra · High" }).click();
    (await findByRole("button", { name: "Choose a model" })).click();
    (await findByRole("option", { name: /GPT-5.6-Terra/ })).click();

    expect(onChoose).not.toHaveBeenCalled();
    const slider = await findByRole("slider", { name: "Effort" });
    expect(slider).toHaveValue("2");
    expect(slider).toHaveAttribute("aria-valuetext", "High");
  });

  it("closes on the pick for a model that takes no effort", async () => {
    const onChoose = vi.fn();
    const { getByRole, findByRole, queryByRole } = render(() => (
      <ModelChip
        models={WITH_EFFORTLESS}
        choice={{ model: "gpt-5.6-terra", effort: "medium" }}
        onChoose={onChoose}
      />
    ));

    getByRole("button", { name: "GPT-5.6-Terra · Medium" }).click();
    (await findByRole("button", { name: "Choose a model" })).click();
    (await findByRole("option", { name: /Haiku/ })).click();

    expect(onChoose).toHaveBeenCalledWith({ model: "haiku" });
    expect(queryByRole("slider")).not.toBeInTheDocument();
    expect(queryByRole("option")).not.toBeInTheDocument();
  });

  it("rests on the level the harness opens at when the choice names none", async () => {
    const { getByRole, findByRole } = render(() => (
      <ModelChip models={CODEX} choice={{ model: "gpt-5.6-terra" }} onChoose={vi.fn()} />
    ));

    getByRole("button", { name: "GPT-5.6-Terra" }).click();

    const slider = await findByRole("slider", { name: "Effort" });
    expect(slider).toHaveValue("1");
    expect(slider).toHaveAttribute("aria-valuetext", "Medium");
    // The rail's head is the model, and it is the way back to the list.
    expect(await findByRole("button", { name: "Choose a model" })).toHaveTextContent(
      "GPT-5.6-Terra",
    );
  });

  it("chooses the level a stop lands on", async () => {
    const onChoose = vi.fn();
    const { getByRole, findByRole } = render(() => (
      <ModelChip
        models={CODEX}
        choice={{ model: "gpt-5.6-terra", effort: "medium" }}
        onChoose={onChoose}
      />
    ));

    getByRole("button", { name: "GPT-5.6-Terra · Medium" }).click();
    const slider = await findByRole("slider", { name: "Effort" });

    fireEvent.input(slider, { target: { value: "3" } });
    expect(onChoose).toHaveBeenCalledWith({ model: "gpt-5.6-terra", effort: "xhigh" });

    fireEvent.input(slider, { target: { value: "0" } });
    expect(onChoose).toHaveBeenCalledWith({ model: "gpt-5.6-terra", effort: "low" });
  });

  it("steps by detent from the keyboard", async () => {
    const onChoose = vi.fn();
    const { getByRole, findByRole } = render(() => (
      <ModelChip models={CODEX} choice={{ model: "gpt-5.6-terra" }} onChoose={onChoose} />
    ));

    getByRole("button", { name: "GPT-5.6-Terra" }).click();
    const slider = await findByRole("slider", { name: "Effort" });

    fireEvent.keyDown(slider, { key: "ArrowRight" });
    expect(onChoose).toHaveBeenCalledWith({ model: "gpt-5.6-terra", effort: "high" });
  });

  it("keeps the rail's model name as the way back to the list", async () => {
    const { getByRole, findByRole } = render(() => (
      <ModelChip
        models={CODEX}
        choice={{ model: "gpt-5.6-terra", effort: "high" }}
        onChoose={vi.fn()}
      />
    ));

    getByRole("button", { name: "GPT-5.6-Terra · High" }).click();
    (await findByRole("button", { name: "Choose a model" })).click();

    expect(await findByRole("option", { name: /GPT-5.5/ })).toBeInTheDocument();
    expect(await findByRole("option", { name: /GPT-5.6-Terra/ })).toBeInTheDocument();
  });

  it("offers no reset, because there is no stop to reset to", async () => {
    const { getByRole, findByRole, queryByRole } = render(() => (
      <ModelChip
        models={CODEX}
        choice={{ model: "gpt-5.6-terra", effort: "high" }}
        onChoose={vi.fn()}
      />
    ));

    getByRole("button", { name: "GPT-5.6-Terra · High" }).click();
    await findByRole("slider", { name: "Effort" });

    expect(queryByRole("button", { name: "Reset effort" })).not.toBeInTheDocument();
    expect(queryByRole("slider", { name: "Effort" })).not.toHaveAttribute(
      "aria-valuetext",
      "Default",
    );
  });

  it("offers no search box on a list that already fits on one screen", async () => {
    const { getByRole, findByRole, queryByRole } = render(() => (
      <ModelChip
        models={WITH_EFFORTLESS}
        choice={{ model: "gpt-5.5", effort: "medium" }}
        onChoose={vi.fn()}
      />
    ));

    getByRole("button", { name: "GPT-5.5 · Medium" }).click();
    (await findByRole("button", { name: "Choose a model" })).click();

    await findByRole("option", { name: /GPT-5.5/ });
    expect(queryByRole("searchbox")).not.toBeInTheDocument();
  });

  it("filters a long list on every word of the query", async () => {
    const devin = Array.from({ length: 12 }, (_, index) => ({
      id: `model-${index}`,
      label: `Model ${index}`,
      description: index === 7 ? "A model on Devin, fast." : "A model on Devin.",
      is_default: index === 0,
      efforts: [],
      default_effort: null,
    }));
    const { getByRole, findAllByRole, queryAllByRole } = render(() => (
      <ModelChip models={devin} choice={{ model: "model-0" }} onChoose={vi.fn()} />
    ));

    getByRole("button", { name: "Model 0" }).click();
    const box = getByRole("searchbox", { name: "Search models" }) as HTMLInputElement;
    expect(await findAllByRole("option")).toHaveLength(12);

    box.value = "7 fast";
    box.dispatchEvent(new Event("input", { bubbles: true }));

    const options = await findAllByRole("option");
    expect(options).toHaveLength(1);
    expect(options[0]).toHaveTextContent("Model 7");
    expect(queryAllByRole("option")).toHaveLength(1);
  });

  it("says so when nothing matches", async () => {
    const devin = Array.from({ length: 9 }, (_, index) => ({
      id: `model-${index}`,
      label: `Model ${index}`,
      description: "A model on Devin.",
      is_default: index === 0,
      efforts: [],
      default_effort: null,
    }));
    const { getByRole, findByText, queryAllByRole } = render(() => (
      <ModelChip models={devin} choice={{ model: "model-0" }} onChoose={vi.fn()} />
    ));

    getByRole("button", { name: "Model 0" }).click();
    const box = getByRole("searchbox", { name: "Search models" }) as HTMLInputElement;
    box.value = "zephyr";
    box.dispatchEvent(new Event("input", { bubbles: true }));

    expect(await findByText("No models match “zephyr”.")).toBeInTheDocument();
    expect(queryAllByRole("option")).toHaveLength(0);
  });
});
