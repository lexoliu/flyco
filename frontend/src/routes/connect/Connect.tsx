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
 * lets a settings card that already names the agent skip the question that
 * asks for it.
 */
import { useNavigate, useSearchParams } from "@solidjs/router";
import Flow from "../../components/flow/Flow";
import type { HarnessKind } from "../../api/client";
import type { FlowAnswers, Stage } from "../../lib/flow";
import { HARNESS_LABEL } from "../../lib/harnesses";

/** Where a connect flow goes when it is over, or abandoned. */
function returnPath(param: string | undefined): string {
  // A same-origin path only: a return address is a place in this app, and
  // anything else in the parameter is not an address the flow will follow.
  return param !== undefined && param.startsWith("/") && !param.startsWith("//") ? param : "/";
}

/** The agent a settings card named, when it named one flyco runs. */
function agentParam(param: string | undefined): HarnessKind | null {
  return param !== undefined && param in HARNESS_LABEL ? (param as HarnessKind) : null;
}

function ConnectStage(props: { stage: Stage; answers: Partial<FlowAnswers>; position: number }) {
  const navigate = useNavigate();
  const [params] = useSearchParams<{ return?: string }>();
  const leave = () => navigate(returnPath(params.return));

  return (
    <Flow
      stages={[props.stage]}
      answers={props.answers}
      position={props.position}
      onDone={leave}
      onLeave={leave}
    />
  );
}

export function ConnectHarness() {
  const [params] = useSearchParams<{ agent?: string }>();
  const agent = agentParam(params.agent);
  // A named agent starts on its sign-in page; the choice page is one `Back`
  // away for someone who meant the other one.
  return (
    <ConnectStage
      stage="agent"
      answers={agent === null ? {} : { agent, agentRoute: "sign-in" }}
      position={agent === null ? 0 : 1}
    />
  );
}

export function ConnectCompute() {
  return <ConnectStage stage="compute" answers={{}} position={0} />;
}
