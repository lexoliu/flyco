/**
 * The model a session runs on, as the picker reads and writes it.
 *
 * A harness lists its models itself — the Agent SDK's `supportedModels()`,
 * Codex's `model/list` — and the control plane serves that list per linked
 * account, falling back to a built-in one until a session has reported.
 * Everything here is pure arithmetic over that list: which entry a choice
 * names, what the chip says, what a session opens on when nobody chose.
 */
import type { ModelChoice, ModelOption } from "../api/client";

/**
 * The choice a session opens on when the user names none.
 *
 * The harness's own default where the list marks one, else the first
 * entry: a list is ordered by the harness, and its head is what the
 * harness would show first. An empty list is a control plane that served
 * nothing to choose from, which is a bug to surface rather than a chip to
 * leave blank.
 */
export function defaultChoice(models: readonly ModelOption[]): ModelChoice {
  const preferred = models.find((option) => option.is_default) ?? models[0];
  if (preferred === undefined) {
    throw new Error("The agent offers no models to choose from.");
  }
  return { model: preferred.id };
}

/** The entry a choice names, or `undefined` for an id the list no longer carries. */
export function optionOf(
  models: readonly ModelOption[],
  choice: ModelChoice,
): ModelOption | undefined {
  return models.find((option) => option.id === choice.model);
}

/**
 * What an effort level is called on screen.
 *
 * The harnesses spell them in lowercase and one of them (`xhigh`) is not a
 * word; the chip has room for one short word and it should be a real one.
 */
export function effortLabel(effort: string): string {
  switch (effort) {
    case "xhigh":
      return "Extra high";
    default:
      return effort.charAt(0).toUpperCase() + effort.slice(1);
  }
}

/**
 * The name a chip has room for.
 *
 * Claude Code's list names its rows for a menu (`Default (recommended)`,
 * `Opus (1M context)`) and puts the model itself at the head of the
 * description (`Opus 5 with 1M context · Best for everyday, complex
 * tasks`); Codex names the row after the model and describes it in a
 * sentence with no such head. So the chip reads the description's first
 * clause where there is one and the label otherwise, which is what the
 * official apps' own chips say: `Fable 5.1`, `Sonnet 5`, `GPT-5.5`. The
 * picker keeps the full label and description.
 *
 * The clause is cut at ` with `: the default row's head is `Opus 5 with 1M
 * context`, twenty-two characters that wrap the composer's row on a phone,
 * and the context window is a fact about the row the picker states in
 * full, not part of the model's name.
 */
export function shortName(option: ModelOption): string {
  const [head] = option.description.split(" · ", 2);
  if (head === undefined || head.trim() === "" || !option.description.includes(" · ")) {
    return option.label;
  }
  const [name] = head.trim().split(" with ", 2);
  return name === undefined || name === "" ? head.trim() : name;
}

/**
 * The chip's text: the model's short name and, when one is chosen, the
 * effort after it (`Fable 5.1 · High`).
 *
 * An id the list does not know is shown as the id: a stale choice reads as
 * exactly what it is rather than as nothing, and the picker beside it
 * offers the way back to something the list does know.
 */
export function choiceLabel(models: readonly ModelOption[], choice: ModelChoice): string {
  const option = optionOf(models, choice);
  const name = option === undefined ? choice.model : shortName(option);
  const effort = choice.effort;
  return effort === undefined || effort === null ? name : `${name} · ${effortLabel(effort)}`;
}

/** Whether two choices name the same model at the same effort. */
export function sameChoice(left: ModelChoice, right: ModelChoice): boolean {
  return left.model === right.model && (left.effort ?? null) === (right.effort ?? null);
}
