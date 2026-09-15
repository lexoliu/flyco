/**
 * The live desktop: an AV1 stream decoded by WebCodecs onto a canvas,
 * with a takeover that turns the same surface into the machine's input.
 *
 * Watching and driving are separate claims. Watching is the SSE stream —
 * it opens the moment the panel mounts and the watcher lease it mints is
 * what turns the daemon's encoder on. Driving is a `POST /desktop/takeover`
 * naming that lease; while it holds, pointer, wheel and key events on the
 * canvas are batched into `POST /desktop/input` calls. The room enforces
 * single-driver — a takeover clears every other watcher's flag — so this
 * panel never has to negotiate; a refused input is how it learns the
 * wheel moved.
 *
 * Pointer coordinates are CSS pixels over a letterboxed canvas; the
 * desktop is fixed-resolution, so every event is scaled by the ratio of
 * the rendered box to the decoded frame size before it leaves.
 */
import {
  createEffect,
  createSignal,
  onCleanup,
  onMount,
  Show,
} from "solid-js";
import { desktopInput, desktopTakeover } from "../../api/client";
import { createDesktopFeed } from "../../api/desktop";
import { ApiProblem } from "../../api/problem";
import type { SessionRelay } from "../../api/relay";
import type { DesktopInputEvent, DesktopStatus } from "../../api/wire";
import {
  DesktopDecoder,
  desktopDecodeSupported,
} from "../../lib/desktopDecoder";
import { cx } from "../../lib/cx";
import styles from "./ScreenPanel.module.css";

export interface ScreenPanelImplProps {
  sessionId: string;
  relay: SessionRelay;
  onError: (error: unknown) => void;
}

/** How long pointer positions sit before they are sent as one batch. */
const INPUT_FLUSH_MS = 50;

/** A `MouseEvent.button`/`buttons` index that names a wire button. */
const WIRE_BUTTONS = ["left", "middle", "right", "back", "forward"] as const;

interface DesktopGeometry {
  width: number;
  height: number;
}

export default function ScreenPanelImpl(props: ScreenPanelImplProps) {
  let canvas: HTMLCanvasElement | undefined;
  let stage: HTMLDivElement | undefined;

  // Whether this browser can decode the stream at all; `null` is "checking".
  const [supported, setSupported] = createSignal<boolean | null>(null);
  // The decoder's last-known frame size — input events scale against it.
  const [geometry, setGeometry] = createSignal<DesktopGeometry | null>(null);
  // Whether *this watcher* currently holds the wheel.
  const [driving, setDriving] = createSignal(false);
  // Whether the stream has delivered at least one frame since the last
  // reset — until then the canvas shows a status line, not a black box.
  const [painted, setPainted] = createSignal(false);
  // The decoder's complaint, surfaced inline until a keyframe clears it.
  const [decodeError, setDecodeError] = createSignal<string | null>(null);
  const [takeoverBusy, setTakeoverBusy] = createSignal(false);

  // ---- state folded out of the session's relay events ----

  const [status, setStatus] = createSignal<{
    status: DesktopStatus;
    detail: string | null;
  } | null>(null);
  /** Whether anyone drives — `desktop_takeover` is the room's truth. */
  const [held, setHeld] = createSignal(false);

  // `processed` tracks how far the fold has read — `events` changes on
  // every frame, not just desktop ones, so re-reading history is skipped.
  let processed = 0;
  createEffect(() => {
    const events = props.relay.events();
    for (let i = processed; i < events.length; i += 1) {
      const entry = events[i];
      if (entry === undefined) {
        continue;
      }
      const event = entry.event;
      if (event.type === "desktop_state") {
        setStatus({ status: event.status, detail: event.detail ?? null });
      } else if (event.type === "desktop_takeover") {
        setHeld(event.active);
        if (!event.active) {
          setDriving(false);
        }
      }
    }
    processed = events.length;
  });

  // ---- the stream and decoder ----

  const decoder = new DesktopDecoder({
    onFrame(frame) {
      const ctx = canvas?.getContext("2d");
      if (ctx === undefined || ctx === null || canvas === undefined) {
        frame.close();
        return;
      }
      const size = { width: frame.displayWidth, height: frame.displayHeight };
      const known = geometry();
      if (known === null || known.width !== size.width || known.height !== size.height) {
        canvas.width = size.width;
        canvas.height = size.height;
        setGeometry(size);
      }
      ctx.drawImage(frame, 0, 0);
      frame.close();
      setPainted(true);
      setDecodeError(null);
    },
    onError(error) {
      setDecodeError(
        error instanceof Error ? error.message : "the stream could not be decoded",
      );
      setPainted(false);
    },
  });

  const feed = createDesktopFeed(props.sessionId, {
    listener: {
      hello(hello) {
        // A reconnect minted a new watcher — the old lease's takeover died
        // with it, and the decoder restarts at the next keyframe anyway.
        // The hello's takeover flag is also the room's freshest word on
        // whether anyone drives, ahead of the relay's fold catching up.
        setDriving(false);
        setHeld(hello.takeover);
        decoder.reset();
        setPainted(false);
      },
      chunk(data, keyframe) {
        decoder.push(data, keyframe);
      },
      resync() {
        decoder.reset();
        setPainted(false);
      },
    },
  });

  onMount(() => {
    void desktopDecodeSupported().then(setSupported);
    // Wheel listeners default to passive, where preventDefault is a no-op —
    // while driving, the page scrolling under the cursor is exactly the
    // thing the wheel event is supposed to be replacing.
    stage?.addEventListener("wheel", onWheel, { passive: false });
  });

  onCleanup(() => {
    stage?.removeEventListener("wheel", onWheel);
    // Releasing the panel releases the wheel: the room drops takeover when
    // the watcher lapses anyway, but asking first keeps the screen honest
    // rather than waiting out the lease — and it has to happen before
    // `dispose`, which forgets the watcher id this request names.
    if (driving()) {
      const watcher = feed.watcher();
      if (watcher !== null) {
        void desktopTakeover(props.sessionId, { watcher, active: false }).catch(
          () => {},
        );
      }
    }
    feed.dispose();
    decoder.dispose();
    flushInput();
    if (flushTimer !== undefined) {
      clearTimeout(flushTimer);
    }
  });

  // ---- input ----

  const pending: DesktopInputEvent[] = [];
  let flushTimer: ReturnType<typeof setTimeout> | undefined;

  function flushInput(): void {
    if (flushTimer !== undefined) {
      clearTimeout(flushTimer);
      flushTimer = undefined;
    }
    if (pending.length === 0) {
      return;
    }
    const watcher = feed.watcher();
    if (watcher === null) {
      pending.length = 0;
      return;
    }
    const events = pending.splice(0, pending.length);
    void desktopInput(props.sessionId, { watcher, events }).catch(
      (error: unknown) => {
        if (error instanceof ApiProblem && (error.status === 409 || error.status === 410)) {
          // 409: another watcher drives — the wheel moved. 410: our lease
          // died — a reconnect is already minting a new one. Either way
          // this watcher is no longer driving.
          setDriving(false);
          return;
        }
        props.onError(error);
      },
    );
  }

  function pushInput(event: DesktopInputEvent): void {
    // A move is only worth its latest value — collapse a backlog of them.
    if (event.kind === "move") {
      for (let i = pending.length - 1; i >= 0; i -= 1) {
        if (pending[i]?.kind === "move") {
          pending.splice(i, 1);
        }
      }
    }
    pending.push(event);
    if (flushTimer === undefined) {
      flushTimer = setTimeout(flushInput, INPUT_FLUSH_MS);
    }
  }

  /** Maps a DOM pointer position onto desktop pixel coordinates. */
  function desktopPoint(event: MouseEvent): { x: number; y: number } | null {
    const size = geometry();
    if (size === null || canvas === undefined) {
      return null;
    }
    const box = canvas.getBoundingClientRect();
    if (box.width === 0 || box.height === 0) {
      return null;
    }
    const x = Math.round(((event.clientX - box.left) / box.width) * size.width);
    const y = Math.round(((event.clientY - box.top) / box.height) * size.height);
    return {
      x: Math.min(Math.max(x, 0), size.width - 1),
      y: Math.min(Math.max(y, 0), size.height - 1),
    };
  }

  function onPointerMove(event: PointerEvent): void {
    if (!driving()) {
      return;
    }
    const point = desktopPoint(event);
    if (point !== null) {
      pushInput({ kind: "move", x: point.x, y: point.y });
    }
  }

  function onPointerButton(event: PointerEvent, pressed: boolean): void {
    if (!driving()) {
      return;
    }
    event.preventDefault();
    const point = desktopPoint(event);
    const button = WIRE_BUTTONS[event.button];
    if (point === null || button === undefined) {
      return;
    }
    pushInput({ kind: "button", button, pressed, x: point.x, y: point.y });
    flushInput();
  }

  function onWheel(event: WheelEvent): void {
    if (!driving()) {
      return;
    }
    event.preventDefault();
    const point = desktopPoint(event);
    if (point === null) {
      return;
    }
    // deltaMode: 0 = pixels, 1 = lines, 2 = pages.
    const unit = event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? 400 : 1;
    const dx = Math.round(event.deltaX * unit);
    const dy = Math.round(event.deltaY * unit);
    if (dx !== 0 || dy !== 0) {
      pushInput({ kind: "scroll", x: point.x, y: point.y, delta_x: dx, delta_y: dy });
    }
  }

  function onKey(event: KeyboardEvent, pressed: boolean): void {
    if (!driving()) {
      return;
    }
    event.preventDefault();
    // The X server typematics a held key on its own; repeats would double it.
    if (event.repeat) {
      return;
    }
    pushInput({ kind: "key", code: event.code, key: event.key, pressed });
    flushInput();
  }

  // ---- takeover ----

  async function takeOver(): Promise<void> {
    const watcher = feed.watcher();
    if (watcher === null || takeoverBusy()) {
      return;
    }
    setTakeoverBusy(true);
    try {
      await desktopTakeover(props.sessionId, { watcher, active: true });
      setDriving(true);
      stage?.focus();
    } catch (error) {
      if (error instanceof ApiProblem && error.status === 410) {
        props.onError(
          "the screen connection dropped just as takeover was requested — try again once it reconnects",
        );
      } else {
        props.onError(error);
      }
    } finally {
      setTakeoverBusy(false);
    }
  }

  async function release(): Promise<void> {
    const watcher = feed.watcher();
    if (watcher === null) {
      setDriving(false);
      return;
    }
    try {
      await desktopTakeover(props.sessionId, { watcher, active: false });
    } catch (error) {
      // Releasing a lease that already lapsed is the success case.
      if (!(error instanceof ApiProblem && error.status === 410)) {
        props.onError(error);
      }
    }
    setDriving(false);
  }

  // ---- what the banner says ----

  function banner(): string | null {
    if (supported() === false) {
      return "This browser cannot decode AV1 — the screen needs a browser with WebCodecs.";
    }
    const lifecycle = status();
    if (lifecycle?.status === "failed") {
      return lifecycle.detail ?? "The desktop failed to start.";
    }
    const stream = feed.state();
    if (feed.failure() !== null || stream === "failed") {
      return "The screen stream failed.";
    }
    if (stream === "connecting") {
      return "Connecting to the screen…";
    }
    if (stream === "reconnecting") {
      return "Reconnecting to the screen…";
    }
    if (stream === "closed") {
      return "Paused while this tab is hidden.";
    }
    if (!painted()) {
      if (decodeError() !== null) {
        return `The stream could not be decoded (${decodeError()}) — waiting for the next keyframe.`;
      }
      if (lifecycle?.status === "starting") {
        return "The machine is bringing its desktop up.";
      }
      return "Waiting for the first frame…";
    }
    return null;
  }

  return (
    <div class={styles.wrapper}>
      <div class={styles.bar}>
        <span class={styles.state}>
          {driving()
            ? "You are driving"
            : held()
              ? "The screen is being driven"
              : "Watching"}
        </span>
        <Show
          when={driving()}
          fallback={
            <button
              type="button"
              class={styles.drive}
              disabled={takeoverBusy() || feed.watcher() === null}
              title={
                feed.watcher() === null
                  ? "The stream is reconnecting"
                  : "Take control of the machine's desktop"
              }
              onClick={() => void takeOver()}
            >
              Take over
            </button>
          }
        >
          <button
            type="button"
            class={styles.drive}
            onClick={() => void release()}
          >
            Release
          </button>
        </Show>
      </div>
      <div
        ref={stage}
        class={cx(styles.stage, driving() && styles.driving)}
        tabIndex={driving() ? 0 : -1}
        onPointerMove={onPointerMove}
        onPointerDown={(event) => {
          if (driving()) {
            // Capture keeps a drag honest: the release lands here even if
            // the pointer slips off the canvas mid-gesture.
            event.currentTarget.setPointerCapture(event.pointerId);
          }
          onPointerButton(event, true);
        }}
        onPointerUp={(event) => onPointerButton(event, false)}
        onKeyDown={(event) => onKey(event, true)}
        onKeyUp={(event) => onKey(event, false)}
        onContextMenu={(event) => {
          if (driving()) {
            event.preventDefault();
          }
        }}
      >
        <canvas ref={canvas} class={styles.canvas} />
        <Show when={banner() !== null}>
          <div class={styles.banner}>{banner()}</div>
        </Show>
      </div>
      <Show when={driving()}>
        <p class={styles.hint}>
          Clicks and keys go to the machine. Combos the browser keeps —
          ⌘W, ⌘T — still do.
        </p>
      </Show>
    </div>
  );
}
