/**
 * What a page of the first run is, from the frame's side.
 *
 * A page component is a plain function the frame calls once per visit. It
 * gets the flow state and `advance`, and returns its title, its body, and
 * the one primary button — which the frame renders in the footer, the only
 * place a primary exists (docs/ux.md §4). A page has no button of its own
 * beyond quiet links and `Copy` pills, and the type makes that the easy
 * path: there is nowhere in a `PageView` to put a second one.
 */
import type { JSX } from "solid-js";
import type { FlowAnswers, FlowState, Page } from "../../lib/flow";

/** The footer button, as the page describes it on every render. */
export interface Primary {
  /** The page's verb: `Next`, `Sign in with Claude`, `Link Azure`. */
  readonly label: string;
  /**
   * What is missing, or `null` when the button is live.
   *
   * The sentence becomes the button's `title`, so a disabled primary says
   * why rather than simply refusing.
   */
  readonly disabled: string | null;
  /** The label while `onClick` is still running, when it runs long. */
  readonly busy?: string | undefined;
  /**
   * What the button does. A rejection is shown above the footer by the
   * frame; a page that wants the error somewhere specific catches it there.
   */
  readonly onClick: () => void | Promise<void>;
}

export interface PageProps<P extends Page> {
  /** The page, with whatever the sequence computed for it. */
  readonly page: P;
  /** The whole flow, live. */
  readonly state: () => FlowState;
  /** Records answers and moves to the next page (or finishes the flow). */
  readonly advance: (answers?: Partial<FlowAnswers>) => void;
  /** Records answers and stays: for what the page must keep across `Back`. */
  readonly record: (answers: Partial<FlowAnswers>) => void;
}

export interface PageView {
  readonly title: string;
  readonly body: JSX.Element;
  /** Read on every render, so it follows the page's own signals. */
  readonly primary: () => Primary;
}

export type PageComponent<P extends Page> = (props: PageProps<P>) => PageView;

/** Every page id, resolved to the component that draws it. */
export type PageRegistry = {
  readonly [K in Page["id"]]: PageComponent<Extract<Page, { id: K }>>;
};

/** The primary every plain page shares: a live `Next`. */
export const NEXT = (advance: () => void): Primary => ({
  label: "Next",
  disabled: null,
  onClick: advance,
});
