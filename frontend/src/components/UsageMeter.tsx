import RatioBar from "./RatioBar";

export interface UsageMeterProps {
  label: string;
  used?: number | undefined;
  total?: number | undefined;
  unit?: string | undefined;
}

/** Used/total meter, shared by LLM-usage and context-window displays. */
export default function UsageMeter(props: UsageMeterProps) {
  const known = () => props.used !== undefined && props.total !== undefined;
  const ratio = () => {
    const used = props.used;
    const total = props.total;
    if (used === undefined || total === undefined || total <= 0) return 0;
    return used / total;
  };

  return (
    <RatioBar
      label={props.label}
      ratio={known() ? ratio() : undefined}
      value={`${props.used?.toLocaleString()} / ${props.total?.toLocaleString()}${
        props.unit !== undefined ? ` ${props.unit}` : ""
      }`}
    />
  );
}
