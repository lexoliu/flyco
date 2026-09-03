/**
 * `/welcome` — the first run, all three stages (docs/ux.md §4).
 *
 * Shown after the first sign-in while readiness is incomplete, and never
 * again once it has been finished. It is a gate, not a tour: a session
 * cannot exist without an agent and a machine, so there is no way through
 * that links neither.
 */
import { useNavigate } from "@solidjs/router";
import Flow from "../components/flow/Flow";
import { dismissWelcome } from "../lib/localPreferences";

export default function Welcome() {
  const navigate = useNavigate();

  /** Ends the flow for good; the card never returns. */
  function finish(): void {
    dismissWelcome();
    navigate("/", { replace: true });
  }

  return <Flow stages={["meet", "agent", "compute"]} onDone={finish} />;
}
