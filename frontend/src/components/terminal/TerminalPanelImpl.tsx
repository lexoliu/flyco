import { createEffect, getOwner, onCleanup, onMount, runWithOwner } from "solid-js";
import type { Terminal as XTerm } from "@xterm/xterm";
import type { SessionRelay } from "../../api/relay";
import styles from "./TerminalPanel.module.css";

export interface TerminalPanelImplProps {
  sessionId: string;
  relay: SessionRelay;
}

/**
 * The real terminal pane: an xterm.js view attached to the session's
 * `terminal_output` relay events, sending keystrokes back as
 * `terminal_input` commands. Only reachable through `TerminalPanel`'s
 * `lazy()` seam, so this module (and xterm's CSS) is never fetched until a
 * user opens the pane.
 */
export default function TerminalPanelImpl(props: TerminalPanelImplProps) {
  let container: HTMLDivElement | undefined;
  const owner = getOwner();
  let unmounted = false;
  onCleanup(() => {
    unmounted = true;
  });

  onMount(async () => {
    const [{ Terminal }] = await Promise.all([import("@xterm/xterm"), import("@xterm/xterm/css/xterm.css")]);
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
      term.open(container);
      term.writeln(`Connected to ${props.sessionId}'s fish shell.`);

      const inputSubscription = term.onData((data) => {
        if (props.relay.state() !== "live") {
          return;
        }
        props.relay.send({ type: "terminal_input", data });
      });

      onCleanup(() => {
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
          const event = events[i];
          if (event !== undefined && event.type === "terminal_output") {
            term.write(event.data);
          }
        }
        processed = events.length;
      });
    });
  });

  return <div class={styles.terminal} ref={container} />;
}
