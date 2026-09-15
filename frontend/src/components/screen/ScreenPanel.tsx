import { lazy } from "solid-js";
import type { SessionRelay } from "../../api/relay";
import styles from "./ScreenPanel.module.css";

/**
 * Lazily-loaded seam for the desktop screen.
 *
 * WebCodecs setup and the SSE pump only enter the bundle once the screen
 * tab is shown: `ScreenPanelImpl` is behind `lazy()`, the same way the
 * terminal keeps xterm.js out of the session page's first paint.
 */
const ScreenPanelImpl = lazy(() => import("./ScreenPanelImpl"));

export interface ScreenPanelProps {
  /** The session whose desktop this panel watches. */
  sessionId: string;
  /**
   * The session's live relay — the panel folds `desktop_state` and
   * `desktop_takeover` events out of it rather than opening a second
   * event stream of its own.
   */
  relay: SessionRelay;
  /** Where a refused takeover or input batch is reported. */
  onError: (failure: unknown) => void;
}

export default function ScreenPanel(props: ScreenPanelProps) {
  return (
    <div class={styles.wrapper}>
      <ScreenPanelImpl
        sessionId={props.sessionId}
        relay={props.relay}
        onError={props.onError}
      />
    </div>
  );
}
