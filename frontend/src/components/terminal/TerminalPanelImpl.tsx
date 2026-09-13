import { createEffect, getOwner, onCleanup, onMount, runWithOwner } from "solid-js";
import type { Terminal as XTerm } from "@xterm/xterm";
import type { SessionRelay } from "../../api/relay";
import styles from "./TerminalPanel.module.css";

export interface TerminalPanelImplProps {
  sessionId: string;
  relay: SessionRelay;
  /** Where a refused keystroke or resize is reported; the page owns the banner. */
  onError: (failure: unknown) => void;
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
      // The pane only mounts while the machine is connected — the drawer
      // shows its own notice otherwise — so the greeting is always this.
      term.writeln(`Connected to ${props.sessionId}'s fish shell.`);

      const inputSubscription = term.onData((data) => {
        if (props.relay.state() !== "live") {
          return;
        }
        // A lost keystroke is its own notice — the echo never comes — and
        // the refusal is reported rather than swallowed: a `terminal_input`
        // that the room turned away needs no retry, but the user is owed
        // the fact that it never landed.
        void props.relay
          .send({ type: "terminal_input", data })
          .catch(props.onError);
      });

      function tellSize(): void {
        if (props.relay.state() !== "live") {
          return;
        }
        void props.relay
          .send({ type: "terminal_resize", cols: term.cols, rows: term.rows })
          .catch(props.onError);
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
