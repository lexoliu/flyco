/**
 * `/connect/harness` and `/connect/compute` — one stage of the first run,
 * on its own (docs/ux.md §4).
 *
 * Settings › Agents › `Connect …`, Settings › Compute › `Add compute`, the
 * home page's readiness cards and the composer's chips all land here, and
 * all of them get the very same pages `/welcome` walks, in the same frame.
 * The flow exists once; these routes only say which stage to walk and where
 * to go afterwards.
 *
 * `?return=` names the page to go back to, both when the flow finishes and
 * when `Back` is pressed on its first page; it defaults to home. `?agent=`
 * lets a settings card name the one agent to link, so its page is the whole
 * flow.
 */
import { useNavigate, useSearchParams } from "@solidjs/router";
import Flow from "../../components/flow/Flow";
import { ReadinessGate, useReadiness } from "../../components/Readiness";
import type { HarnessKind } from "../../api/client";
import { EVERY_AGENT, linkedAgents, type Stage } from "../../lib/flow";
import { HARNESS_LABEL } from "../../lib/harnesses";

/** Where a connect flow goes when it is over, or abandoned. */
function returnPath(param: string | undefined): string {
  // A same-origin path only: a return address is a place in this app, and
  // anything else in the parameter is not an address the flow will follow.
  return param !== undefined && param.startsWith("/") && !param.startsWith("//")
    ? param
    : "/";
}

/** The agent a settings card named, when it named one flyco runs. */
function agentParam(param: string | undefined): HarnessKind | null {
  return param !== undefined && param in HARNESS_LABEL
    ? (param as HarnessKind)
    : null;
}

function ConnectStage(props: { stage: Stage; agents: readonly HarnessKind[] }) {
  const navigate = useNavigate();
  const readiness = useReadiness();
  const [params] = useSearchParams<{ return?: string }>();
  const leave = () => navigate(returnPath(params.return));

  return (
    <ReadinessGate>
      <Flow
        stages={[props.stage]}
        agents={props.agents}
        answers={{ agents: linkedAgents(readiness.harness()) }}
        onDone={leave}
        onLeave={leave}
      />
    </ReadinessGate>
  );
}

export function ConnectHarness() {
  const [params] = useSearchParams<{ agent?: string }>();
  const agent = agentParam(params.agent);
  return (
    <ConnectStage
      stage="agent"
      agents={agent === null ? EVERY_AGENT : [agent]}
    />
  );
}

export function ConnectCompute() {
  return <ConnectStage stage="compute" agents={EVERY_AGENT} />;
}
