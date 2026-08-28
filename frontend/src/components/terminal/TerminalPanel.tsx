import { lazy } from "solid-js";

/**
 * Lazily-loaded seam for the session terminal.
 *
 * `TerminalPanelImpl` is a placeholder today. When the terminal ships, it
 * becomes the xterm.js-backed implementation; because it's behind
 * `lazy()`, xterm's JS/CSS only enters the bundle once a user actually
 * opens a session, and callers of `TerminalPanel` don't need to change.
 */
const TerminalPanel = lazy(() => import("./TerminalPanelImpl"));

export default TerminalPanel;
