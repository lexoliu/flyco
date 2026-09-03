/**
 * Stage B, page 1: which agent (docs/ux.md §4 B1).
 *
 * Two cards with radio semantics. A card already linked says so with the
 * account's label, and choosing it goes straight to the linked page: there
 * is nothing to sign in to twice.
 */
import { createSignal } from "solid-js";
import Logomark, { HARNESS_MARK } from "../../Logomark";
import { useReadiness } from "../../Readiness";
import type { HarnessAccountView, HarnessKind } from "../../../api/client";
import { HARNESS_LABEL } from "../../../lib/harnesses";
import type { PageComponent, Primary } from "../page";
import { ChoiceCards, type Choice } from "./shared";

/** The two agents flyco runs, in the order the page lists them. */
const AGENTS: readonly { kind: HarnessKind; line: string }[] = [
  { kind: "claude_code", line: "Runs on your Claude subscription." },
  { kind: "codex", line: "Runs on your ChatGPT subscription." },
];

export const AgentChoice: PageComponent<{ id: "agent-choice" }> = (props) => {
  const readiness = useReadiness();
  const [chosen, setChosen] = createSignal<HarnessKind | null>(props.state().answers.agent);

  const linked = (kind: HarnessKind): HarnessAccountView | undefined =>
    readiness.harness().find((account) => account.harness === kind);

  const choices = (): Choice<HarnessKind>[] =>
    AGENTS.map((agent) => {
      const account = linked(agent.kind);
      return {
        kind: agent.kind,
        title: HARNESS_LABEL[agent.kind],
        line: account === undefined ? agent.line : `Linked · ${account.label}`,
        linked: account !== undefined,
        mark: <Logomark mark={HARNESS_MARK[agent.kind]} size={18} />,
      };
    });

  const primary = (): Primary => {
    const agent = chosen();
    return {
      label: "Next",
      disabled: agent === null ? "Choose an agent to continue" : null,
      onClick: () => {
        if (agent === null) {
          return;
        }
        props.advance({
          agent,
          agentRoute: "sign-in",
          agentAccount: linked(agent) ?? null,
          claudeAttempt: null,
        });
      },
    };
  };

  return {
    title: "Which agent do you use?",
    body: (
      <ChoiceCards
        question="Which agent do you use?"
        choices={choices()}
        value={chosen()}
        onChange={setChosen}
      />
    ),
    primary,
  };
};
