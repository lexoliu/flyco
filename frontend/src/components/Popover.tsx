/**
 * The one popover in the app.
 *
 * Every transient panel anchored to a control — the composer's chips, the
 * account menu in the top bar — is this component, so focus, dismissal and
 * the escape key behave identically everywhere instead of being reinvented
 * per call site.
 *
 * There is no Solid port of Floating UI's behaviour hooks worth the
 * dependency here, so the mechanics are the platform's own: the panel is
 * positioned by ordinary CSS against a `position: relative` anchor, and it
 * closes on `Escape`, on a pointer press outside it, and when focus leaves
 * it. Opening moves focus into the panel, which is what makes the keyboard
 * path work and what makes "focus left" a usable close condition.
 */
import { type JSX, Show, createEffect, createSignal, createUniqueId, on, onCleanup } from "solid-js";
import { cx } from "../lib/cx";
import styles from "./Popover.module.css";

/** What a caller needs to wire onto the control that opens the panel. */
export interface TriggerAttrs {
  /** Element id, which the panel is labelled by. */
  id: string;
  /**
   * Whether the panel is open, as an accessor.
   *
   * Deliberately not a plain boolean: the trigger is built once, and a
   * `trigger` callback that *read* a signal would be re-invoked on every
   * toggle, replacing the button element mid-click — which then makes the
   * outside-press check compare against a node that is no longer in the
   * document and close the panel it just opened.
   */
  expanded: () => boolean;
  /** Toggles the panel. */
  onClick: () => void;
}

export interface PopoverProps {
  /** The control that opens the panel. */
  trigger: (attrs: TriggerAttrs) => JSX.Element;
  /** The panel's contents. `close` dismisses it — pass it to any action. */
  children: (close: () => void) => JSX.Element;
  /** What the panel is, for assistive technology. */
  label: string;
  /** Which edge of the trigger the panel lines up with. Defaults to `start`. */
  align?: "start" | "end" | undefined;
  /**
   * Which side of the trigger the panel opens on.
   *
   * Unset, the panel picks for itself: it opens downward when it fits, and
   * upward when the trigger sits so near the foot of the window — every
   * composer chip — that the room below cannot hold it. Set it only where
   * the caller knows better than the viewport.
   */
  side?: "bottom" | "top" | undefined;
  /**
   * A request to open the panel from outside its trigger — a `⋯` item or a
   * `/` command that names the panel it wants.
   *
   * Carries the instant it was made rather than a plain flag so that asking
   * twice is two requests: a user who closes the panel and picks `Resize`
   * again would otherwise set an unchanged signal and see nothing happen.
   */
  openAt?: number | undefined;
  /** Extra class on the panel, for callers that need a width. */
  panelClass?: string | undefined;
  /**
   * Extra class on the anchor, for a trigger that has to be allowed to
   * shrink inside its row instead of taking its content's width.
   */
  anchorClass?: string | undefined;
}

/** Room left between an open panel and the bottom of the viewport. */
const VIEWPORT_MARGIN_PX = 16;

/** Below this a panel is unusable however little room there is. */
const MIN_PANEL_PX = 160;

export default function Popover(props: PopoverProps) {
  const [open, setOpen] = createSignal(false);
  /**
   * Whether the panel opened upward on its own judgement.
   *
   * Only consulted when `side` is unset — an explicit `side` is the
   * caller's decision and stays where it was put.
   */
  const [flipped, setFlipped] = createSignal(false);
  const triggerId = createUniqueId();
  let anchor: HTMLDivElement | undefined;
  let panel: HTMLDivElement | undefined;

  function close(): void {
    setOpen(false);
  }

  createEffect(
    on(
      () => props.openAt,
      (at) => {
        if (at !== undefined) {
          setOpen(true);
        }
      },
    ),
  );

  createEffect(() => {
    if (!open()) {
      return;
    }
    // Focus lands inside the panel so the keyboard can reach its contents
    // and so blurring out of it is a meaningful "done here". `preventScroll`
    // because the panel is not yet clamped: a focused element that overhangs
    // the viewport is scrolled into view by the browser, and that scroll
    // moves the anchor the fit below is about to measure — the page jumps
    // and the side the panel then picks is the wrong one.
    panel?.focus({ preventScroll: true });

    // What the panel may not do is hang off the viewport: a picker whose
    // bottom half is off screen, on a page that does not scroll, is a
    // control nobody can reach. So the panel gets a side and a height from
    // the room that is actually there — re-measured when the window
    // changes size.
    function fit(): void {
      if (panel === undefined || anchor === undefined) {
        return;
      }
      // On a phone the stylesheet makes the panel a bottom sheet, fixed to
      // the viewport with its own height rule, and a measured maximum from
      // its top would fight it. The stylesheet decides where that starts;
      // this reads the decision rather than repeating the breakpoint.
      if (getComputedStyle(panel).position === "fixed") {
        panel.style.maxHeight = "";
        return;
      }

      // `scrollHeight` answers "how tall would the panel be unclamped"
      // without touching it. Measuring the cleared `maxHeight` instead
      // would briefly grow the panel — and a focused panel that grows is
      // scrolled into view by the browser, moving the anchor under the
      // measurement it interrupted.
      const wanted = panel.scrollHeight;

      if (props.side !== undefined) {
        const box = panel.getBoundingClientRect();
        const room =
          props.side === "top"
            ? box.bottom - VIEWPORT_MARGIN_PX
            : window.innerHeight - box.top - VIEWPORT_MARGIN_PX;
        panel.style.maxHeight = `${Math.max(room, MIN_PANEL_PX)}px`;
        return;
      }

      // The room either side of the trigger, and the panel goes where the
      // room is: down when it fits, up when the room below cannot hold it
      // and the room above can hold more.
      const anchorBox = anchor.getBoundingClientRect();
      const below = window.innerHeight - anchorBox.bottom - VIEWPORT_MARGIN_PX;
      const above = anchorBox.top - VIEWPORT_MARGIN_PX;
      const flip = wanted > below && above > below;
      setFlipped(flip);
      const room = flip ? above : below;
      panel.style.maxHeight = `${Math.max(room, MIN_PANEL_PX)}px`;
    }
    fit();
    // The panel's own contents settle after it opens — a breakdown answer
    // arriving, a disclosure opening — and the anchor can move under it
    // while the page reflows around it, because the panel hangs off the
    // anchor's place, not its own: a controls row wrapping, a text field
    // growing, a smooth scroll still landing, a transition on a parent.
    // No observer hears all of those, so the check is the input itself —
    // the anchor's rect and the panel's wanted height, each frame while
    // the panel is open, and a re-fit when either has moved. The reads
    // are cheap against a settled layout, and silent while nothing moves.
    let lastTop = NaN;
    let lastBottom = NaN;
    let lastWanted = NaN;
    let lastViewport = NaN;
    let frame = 0;
    const watch = (): void => {
      if (panel !== undefined && anchor !== undefined) {
        const box = anchor.getBoundingClientRect();
        const wanted = panel.scrollHeight;
        const viewport = window.innerHeight;
        if (
          box.top !== lastTop ||
          box.bottom !== lastBottom ||
          wanted !== lastWanted ||
          viewport !== lastViewport
        ) {
          lastTop = box.top;
          lastBottom = box.bottom;
          lastWanted = wanted;
          lastViewport = viewport;
          fit();
        }
      }
      frame = requestAnimationFrame(watch);
    };
    frame = requestAnimationFrame(watch);
    onCleanup(() => cancelAnimationFrame(frame));

    function onPointerDown(event: PointerEvent): void {
      if (anchor !== undefined && !anchor.contains(event.target as Node)) {
        close();
      }
    }
    function onKeyDown(event: KeyboardEvent): void {
      if (event.key === "Escape") {
        close();
        // Escape returns the user to where they were, not to the top of
        // the document — and never scrolls there either.
        anchor?.querySelector("button")?.focus({ preventScroll: true });
      }
    }

    document.addEventListener("pointerdown", onPointerDown);
    document.addEventListener("keydown", onKeyDown);
    onCleanup(() => {
      document.removeEventListener("pointerdown", onPointerDown);
      document.removeEventListener("keydown", onKeyDown);
    });
  });

  return (
    <div
      class={cx(styles.anchor, props.anchorClass)}
      ref={anchor}
      onFocusOut={() => {
        // A blur can mean focus left the popover — or that the element
        // holding it was removed mid-render, when a view inside the
        // panel swaps (the model picker's rail trading places with its
        // list under a click). The distinction only settles once the
        // arriving view has had its chance to take focus, so the check
        // runs a frame later against where focus actually ended up.
        requestAnimationFrame(() => {
          if (anchor !== undefined && !anchor.contains(document.activeElement)) {
            close();
          }
        });
      }}
    >
      {props.trigger({
        id: triggerId,
        expanded: open,
        onClick: () => setOpen(!open()),
      })}
      <Show when={open()}>
        <div
          ref={panel}
          class={cx(
            styles.panel,
            props.align === "end" && styles.alignEnd,
            (props.side === "top" || (props.side === undefined && flipped())) &&
              styles.sideTop,
            props.panelClass,
          )}
          role="dialog"
          aria-label={props.label}
          aria-labelledby={triggerId}
          tabindex="-1"
        >
          {props.children(close)}
        </div>
      </Show>
    </div>
  );
}
