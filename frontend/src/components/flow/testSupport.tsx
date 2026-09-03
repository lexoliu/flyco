/**
 * What the flow's component tests share: a mounted frame, a way to answer
 * one route of the in-memory control plane, and the readers that check the
 * frame's own rules.
 */
import { expect, vi } from "vitest";
import { fireEvent, render } from "@solidjs/testing-library";
import Flow from "./Flow";
import { ReadinessProvider } from "../Readiness";
import type { HarnessKind } from "../../api/client";
import type { FlowAnswers, Stage } from "../../lib/flow";
import styles from "./Flow.module.css";

export function renderFlow(
  stages: readonly Stage[],
  options: {
    agents?: readonly HarnessKind[];
    answers?: Partial<FlowAnswers>;
    position?: number;
    onLeave?: () => void;
  } = {},
) {
  const onDone = vi.fn();
  const rendered = render(() => (
    <ReadinessProvider enabled={() => true}>
      <Flow
        stages={stages}
        agents={options.agents}
        answers={options.answers}
        position={options.position}
        onDone={onDone}
        onLeave={options.onLeave}
      />
    </ReadinessProvider>
  ));
  return { ...rendered, onDone };
}

/** A problem document, in the shape the API answers with. */
export function problem(
  status: number,
  slug: string,
  detail: string,
): Response {
  return new Response(
    JSON.stringify({
      type: `https://flyco.dev/problems/${slug}`,
      title: "Problem",
      status,
      detail,
    }),
    { status, headers: { "content-type": "application/problem+json" } },
  );
}

export function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

/**
 * Answers one route with `respond`, leaving every other route to the
 * in-memory control plane the test setup installs.
 */
export function route(
  matches: (path: string, method: string) => boolean,
  respond: (request: RequestInit | undefined) => Response,
): void {
  const fallback = vi.mocked(fetch).getMockImplementation();
  if (fallback === undefined) {
    throw new Error("the test setup installs a fetch mock before every test");
  }
  vi.mocked(fetch).mockImplementation((input, init) => {
    const url = new URL(
      typeof input === "string"
        ? input
        : input instanceof URL
          ? input.href
          : input.url,
    );
    if (matches(url.pathname, (init?.method ?? "GET").toUpperCase())) {
      return Promise.resolve(respond(init));
    }
    return fallback(input, init);
  });
}

/** The body of the one `POST` the flow made to `path`, as JSON. */
export function postedTo(path: string): unknown {
  const call = vi
    .mocked(fetch)
    .mock.calls.find(
      ([input, init]) =>
        String(input).endsWith(path) && init?.method === "POST",
    );
  expect(call, `no POST to ${path}`).toBeDefined();
  return JSON.parse(String(call?.[1]?.body));
}

/** Types `value` into a field, the way an input event carries it. */
export function type(field: HTMLElement, value: string): void {
  fireEvent.input(field, { target: { value } });
}

/** The frame's one primary, which is the only element of its class. */
export function primary(container: HTMLElement): HTMLButtonElement {
  const found = container.querySelectorAll<HTMLButtonElement>(
    `.${styles.primary}`,
  );
  expect(found, "exactly one primary on the page").toHaveLength(1);
  return found[0] as HTMLButtonElement;
}

/**
 * What a screen reader would call `button`, by the two routes the flow
 * uses to name one: an explicit `aria-label`, or the text inside it.
 */
export function accessibleName(button: HTMLElement): string {
  return (button.getAttribute("aria-label") ?? button.textContent ?? "").trim();
}

/** Every button on the page answers to something. */
export function expectEveryButtonNamed(container: HTMLElement): void {
  const buttons = [...container.querySelectorAll("button")];
  expect(buttons.length).toBeGreaterThan(0);
  expect(
    buttons
      .filter((button) => accessibleName(button) === "")
      .map((button) => button.outerHTML),
  ).toEqual([]);
}
