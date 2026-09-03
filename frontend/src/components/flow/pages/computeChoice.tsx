/**
 * Stage C, page 1: where sessions run (docs/ux.md §4 C1).
 *
 * Four cards with radio semantics. The fourth is the one that takes no
 * credential: the control plane runs on Cloudflare Workers and has no TCP
 * sockets, so a machine the user owns is *enrolled* rather than dialled.
 */
import { Show, createSignal } from "solid-js";
import { Server } from "lucide-solid";
import Logomark, { PROVIDER_MARK } from "../../Logomark";
import type { CloudProviderKind } from "../../../api/client";
import { PROVIDER_LABEL } from "../../../lib/providers";
import type { PageComponent, Primary } from "../page";
import { ChoiceCards, type Choice } from "./shared";

/** The four places a session can run, in the order the page lists them. */
const PLACES: readonly { kind: CloudProviderKind; line: string }[] = [
  {
    kind: "azure",
    line: "One command in Cloud Shell, then paste what it printed.",
  },
  {
    kind: "aws",
    line: "An IAM user carrying the policy flyco actually calls.",
  },
  {
    kind: "gcp",
    line: "A Compute Admin service account, dropped in as its key file.",
  },
  { kind: "host", line: "Enroll a Linux machine you own with one command." },
];

export const ComputeChoice: PageComponent<{ id: "compute-choice" }> = (
  props,
) => {
  const [chosen, setChosen] = createSignal<CloudProviderKind | null>(
    props.state().answers.compute,
  );

  const choices: Choice<CloudProviderKind>[] = PLACES.map((place) => ({
    kind: place.kind,
    title: PROVIDER_LABEL[place.kind],
    line: place.line,
    linked: false,
    mark: (
      <Show
        when={PROVIDER_MARK[place.kind]}
        /* A machine the user owns has no vendor behind it, so it gets the
           generic server glyph rather than borrowing a logo. */
        fallback={<Server size={18} aria-hidden="true" />}
      >
        {(mark) => <Logomark mark={mark()} size={18} />}
      </Show>
    ),
  }));

  const primary = (): Primary => {
    const compute = chosen();
    return {
      label: "Next",
      disabled:
        compute === null ? "Choose where sessions run to continue" : null,
      onClick: () => {
        if (compute === null) {
          return;
        }
        props.advance({
          compute,
          azurePaste: "",
          azurePrincipal: null,
          azureSubscription: null,
          azureKey: null,
        });
      },
    };
  };

  return {
    title: "Where should sessions run?",
    body: (
      <ChoiceCards
        question="Where should sessions run?"
        choices={choices}
        value={chosen()}
        onChange={setChosen}
      />
    ),
    primary,
  };
};
