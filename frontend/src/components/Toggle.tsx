/**
 * A switch: on or off, applied the moment it is pressed.
 *
 * `role="switch"` rather than a checkbox in a form, because nothing here is
 * submitted — flipping an MCP server on is a `PATCH`, and the control is the
 * action. It is disabled while that request is in flight so the state on
 * screen is never ahead of the state on the server.
 */
import { cx } from "../lib/cx";
import styles from "./Toggle.module.css";

export interface ToggleProps {
  /** What the switch controls, for assistive technology. */
  label: string;
  checked: boolean;
  disabled?: boolean | undefined;
  onChange: (next: boolean) => void;
}

export default function Toggle(props: ToggleProps) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={props.checked}
      aria-label={props.label}
      disabled={props.disabled === true}
      class={cx(styles.track, props.checked && styles.on)}
      onClick={() => props.onChange(!props.checked)}
    >
      <span class={styles.knob} aria-hidden="true" />
    </button>
  );
}
