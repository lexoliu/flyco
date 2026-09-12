import { createEffect, getOwner, onCleanup, onMount, runWithOwner } from "solid-js";
import type { Terminal as XTerm } from "@xterm/xterm";
import type { SessionRelay } from "../../api/relay";
import styles from "./TerminalPanel.module.css";

export interface TerminalPanelImplProps {
  sessionId: string;
  relay: SessionRelay;
  /**
   * Whether the session's daemon is there to take a keystroke.
   *
   * The pane draws what the daemon's PTY sends and nothing else, so with
   * no daemon connected the honest thing is to say the shell is closed —
   * a pane that swallowed input silently would look like a shell that had
   * stopped echoing.
   */
  machineUp: boolean;
}

/**
 * The real terminal pane: an xterm.js view attached to the session's
 * `terminal_output` relay events, sending keystrokes back as
 * `terminal_input` commands. Only reachable through `TerminalPanel`'s
 * `lazy()` seam, so this module (and xterm's CSS) is never fetched until a
 * user opens the pane.
 *
 * The pane is fitted to the space it has, and the daemon is told the size
 * (`terminal_resize`) so the PTY wraps where the view does: on open, on
 * every resize of the pane, and again whenever the relay comes back live,
 * because a daemon that reconnected opened its PTY at a default size.
 */
export default function TerminalPanelImpl(props: TerminalPanelImplProps) {
  let container: HTMLDivElement | undefined;
  const owner = getOwner();
  let unmounted = false;
  onCleanup(() => {
    unmounted = true;
  });

  onMount(async () => {
    const [{ Terminal }, { FitAddon }] = await Promise.all([
      import("@xterm/xterm"),
      import("@xterm/addon-fit"),
      import("@xterm/xterm/css/xterm.css"),
    ]);
    if (unmounted || container === undefined || owner === null) {
      return;
    }

    // Everything below resumed after an `await`; Solid only threads the
    // reactive owner through synchronous continuations, so `onCleanup` and
    // `createEffect` are wrapped in `runWithOwner` against the owner
    // captured before the await, rather than the (by-now-detached) implicit
    // one.
    runWithOwner(owner, () => {
      const term: XTerm = new Terminal({
        convertEol: true,
        fontFamily: "var(--font-mono)",
        fontSize: 13,
      });
      const fit = new FitAddon();
      term.loadAddon(fit);
      term.open(container);
      // What the pane says before the first byte arrives is the truth of
      // the moment, not of a socket that may already be gone.
      if (props.machineUp) {
        term.writeln(`Connected to ${props.sessionId}'s fish shell.`);
      } else {
        term.writeln("The machine is not connected — the shell opens when it is back.");
      }

      const inputSubscription = term.onData((data) => {
        if (props.relay.state() !== "live" || !props.machineUp) {
          return;
        }
        props.relay.send({ type: "terminal_input", data });
      });

      function tellSize(): void {
        if (props.relay.state() !== "live" || !props.machineUp) {
          return;
        }
        props.relay.send({ type: "terminal_resize", cols: term.cols, rows: term.rows });
      }
      const resizeSubscription = term.onResize(tellSize);
      fit.fit();
      const observer = new ResizeObserver(() => fit.fit());
      observer.observe(container);
      createEffect(() => {
        if (props.relay.state() === "live") {
          tellSize();
        }
      });

      // A machine leaving or returning mid-session is written into the
      // scrollback, where the keystroke it just swallowed — or the shell
      // it is about to open — is otherwise a silent change of behaviour.
      let wasUp = props.machineUp;
      createEffect(() => {
        const up = props.machineUp;
        if (up === wasUp) {
          return;
        }
        wasUp = up;
        term.writeln(
          up
            ? "\r\nThe machine is back — a fresh shell opens."
            : "\r\nLost the machine — the shell is closed until it is back.",
        );
      });

      onCleanup(() => {
        observer.disconnect();
        resizeSubscription.dispose();
        inputSubscription.dispose();
        term.dispose();
      });

      // Writes every not-yet-seen `terminal_output` event. `processed`
      // tracks how far this has read so a re-run — the relay's `events`
      // signal changes on every new frame, not just terminal ones — never
      // rewrites history the terminal has already shown.
      let processed = 0;
      createEffect(() => {
        const events = props.relay.events();
        for (let i = processed; i < events.length; i += 1) {
          const entry = events[i];
          if (entry !== undefined && entry.event.type === "terminal_output") {
            term.write(entry.event.data);
          }
        }
        processed = events.length;
      });
    });
  });

  return <div class={styles.terminal} ref={container} />;
}
