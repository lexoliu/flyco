/**
 * Stage B, the one page (docs/ux.md §4 B1): every agent, with its status.
 *
 * Flyco runs several agents and a task picks its agent when it starts, so
 * nothing is chosen here for good. The page is a list: a linked agent
 * reads `Linked` beside the account it runs on, an unlinked one says what
 * it would run on. Choosing an unlinked row turns the primary into
 * `Link …`, which walks that agent's sign-in pages and comes back here;
 * with at least one agent linked the primary is `Next`. However many
 * agents there are, stage B stays one page.
 */
import { createSignal } from "solid-js";
import Logomark, { HARNESS_MARK } from "../../Logomark";
import type { HarnessKind } from "../../../api/client";
import { HARNESS_LABEL } from "../../../lib/harnesses";
import type { PageComponent, Primary } from "../page";
import { ChoiceCards, type Choice } from "./shared";
import styles from "./pages.module.css";

/** What an unlinked agent would run on, under its name. */
const RUNS_ON: Record<HarnessKind, string> = {
  claude_code: "Not linked · a Claude subscription or an Anthropic API key",
  codex: "Not linked · a ChatGPT subscription or an OpenAI API key",
};

export const Agents: PageComponent<{ id: "agents" }> = (props) => {
  const [chosen, setChosen] = createSignal<HarnessKind | null>(
    props.state().answers.linking,
  );
  const account = (agent: HarnessKind) => props.state().answers.agents[agent];
  const anyLinked = () =>
    props.state().agents.some((agent) => account(agent) !== undefined);

  const choices = (): Choice<HarnessKind>[] =>
    props.state().agents.map((agent) => {
      const linked = account(agent);
      return {
        kind: agent,
        title: HARNESS_LABEL[agent],
        line:
          linked === undefined ? RUNS_ON[agent] : `Linked · ${linked.label}`,
        linked: linked !== undefined,
        mark: <Logomark mark={HARNESS_MARK[agent]} size={18} />,
      };
    });

  const primary = (): Primary => {
    const agent = chosen();
    if (agent !== null && account(agent) === undefined) {
      return {
        label: `Link ${HARNESS_LABEL[agent]}`,
        disabled: null,
        onClick: () => props.advance({ linking: agent }),
      };
    }
    return {
      label: "Next",
      disabled: anyLinked() ? null : "Link at least one agent to continue",
      onClick: () => props.advance({ linking: null }),
    };
  };

  return {
    title: "Link the agents you use",
    body: (
      <>
        <p class={styles.lede}>
          Each task picks its agent when you start it. Link every agent you use;
          one is enough to begin.
        </p>
        <ChoiceCards
          question="Which agent do you want to link?"
          choices={choices()}
          value={chosen()}
          onChange={setChosen}
        />
      </>
    ),
    primary,
  };
};
