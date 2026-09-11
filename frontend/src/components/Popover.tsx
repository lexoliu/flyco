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
import { type JSX, Show, createEffect, createSignal, createUniqueId, onCleanup } from "solid-js";
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

  createEffect(() => {
    if (!open()) {
      return;
    }
    // Focus lands inside the panel so the keyboard can reach its contents
    // and so blurring out of it is a meaningful "done here".
    panel?.focus();

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

      // Measuring wants the panel's natural height: a max-height left over
      // from the last fit would report the clamp as the size it needs.
      panel.style.maxHeight = "";
      const wanted = panel.getBoundingClientRect().height;

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
    window.addEventListener("resize", fit);
    onCleanup(() => window.removeEventListener("resize", fit));

    function onPointerDown(event: PointerEvent): void {
      if (anchor !== undefined && !anchor.contains(event.target as Node)) {
        close();
      }
    }
    function onKeyDown(event: KeyboardEvent): void {
      if (event.key === "Escape") {
        close();
        // Escape returns the user to where they were, not to the top of
        // the document.
        anchor?.querySelector("button")?.focus();
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
      onFocusOut={(event) => {
        const next = event.relatedTarget;
        if (next === null || (anchor !== undefined && !anchor.contains(next as Node))) {
          close();
        }
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
