import { lazy } from "solid-js";
import type { SessionRelay } from "../../api/relay";
import styles from "./TerminalPanel.module.css";

/**
 * Lazily-loaded seam for the session terminal.
 *
 * xterm.js (and its CSS) only enters the bundle once the terminal tab is
 * shown: `TerminalPanelImpl` is behind `lazy()`, so the session page itself
 * pays nothing for it. There is no "open" step past that — a terminal tab
 * whose whole content was a button to open a terminal was a tab that asked
 * the user to say twice what they had already said by choosing it.
 */
const TerminalPanelImpl = lazy(() => import("./TerminalPanelImpl"));

export interface TerminalPanelProps {
  sessionId: string;
  relay: SessionRelay;
}

export default function TerminalPanel(props: TerminalPanelProps) {
  return (
    <div class={styles.wrapper}>
      <TerminalPanelImpl sessionId={props.sessionId} relay={props.relay} />
    </div>
  );
}
