import { Show, createSignal, lazy } from "solid-js";
import type { SessionRelay } from "../../api/relay";
import styles from "./TerminalPanel.module.css";

/**
 * Lazily-loaded seam for the session terminal.
 *
 * xterm.js (and its CSS) only enters the bundle once a user actually opens
 * the pane: `TerminalPanelImpl` is behind `lazy()`, and this shell renders
 * it only after the "Open terminal" toggle is clicked — not merely once the
 * session view itself mounts.
 */
const TerminalPanelImpl = lazy(() => import("./TerminalPanelImpl"));

export interface TerminalPanelProps {
  sessionId: string;
  relay: SessionRelay;
}

export default function TerminalPanel(props: TerminalPanelProps) {
  const [open, setOpen] = createSignal(false);

  return (
    <div class={styles.wrapper}>
      <button type="button" class={styles.toggle} onClick={() => setOpen((was) => !was)}>
        {open() ? "Hide terminal" : "Open terminal"}
      </button>
      <Show when={open()}>
        <TerminalPanelImpl sessionId={props.sessionId} relay={props.relay} />
      </Show>
    </div>
  );
}
