import RatioBar, { type MeterTier } from "./RatioBar";

export interface BudgetBarProps {
  label?: string | undefined;
  /** Already converted to whole dollars; the wire format is microdollars. */
  spentUsd?: number | undefined;
  limitUsd?: number | undefined;
}

/** Matches the budget engine's thresholds (flyco_core::budget): notice 50%, warn 80%, final warn 90%, pause 100%. */
const NOTICE_RATIO = 0.5;
const WARN_RATIO = 0.8;
const FINAL_WARN_RATIO = 0.9;

function tierFor(ratio: number): MeterTier {
  if (ratio >= 1) return "paused";
  if (ratio >= FINAL_WARN_RATIO) return "final-warn";
  if (ratio >= WARN_RATIO) return "warn";
  if (ratio >= NOTICE_RATIO) return "notice";
  return "ok";
}

export default function BudgetBar(props: BudgetBarProps) {
  const known = () => props.spentUsd !== undefined && props.limitUsd !== undefined;
  const ratio = () => {
    const spent = props.spentUsd;
    const limit = props.limitUsd;
    if (spent === undefined || limit === undefined || limit <= 0) return 0;
    return Math.min(spent / limit, 1);
  };

  return (
    <RatioBar
      label={props.label ?? "Budget"}
      ratio={known() ? ratio() : undefined}
      value={`$${props.spentUsd?.toFixed(2)} / $${props.limitUsd?.toFixed(2)}`}
      tier={tierFor(ratio())}
    />
  );
}
