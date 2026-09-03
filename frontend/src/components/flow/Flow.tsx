/**
 * The frame every page of the first run is shown in (docs/ux.md §4).
 *
 * One card: a bar per stage, the page's title, its body, and a footer that
 * is the only navigation — `Back` on the left, the page's one primary on
 * the right. The frame is the only thing that renders a primary. A page
 * hands its button up as data (`PageView.primary`) and the frame draws it,
 * disabled with a `title` naming what is missing until the page's
 * prerequisite is met.
 *
 * The frame holds one `FlowState` and nothing else: which page is on screen
 * is `lib/flow.ts`'s answer to the answers so far, and `Back` is the
 * reducer's `back`, which keeps them.
 */
import { For, Show, createMemo, createSignal, on, untrack } from "solid-js";
import ProblemNotice from "../ProblemNotice";
import {
  advance as advanceFlow,
  back as backFlow,
  currentPage,
  finishLink,
  isFinished,
  progress,
  record as recordAnswers,
  startFlow,
  type FlowStart,
  type FlowAnswers,
  type Page,
} from "../../lib/flow";
import type { HarnessAccountView, HarnessKind } from "../../api/client";
import { cx } from "../../lib/cx";
import { PAGES } from "./pages";
import type { PageComponent, PageView } from "./page";
import styles from "./Flow.module.css";

export interface FlowProps extends FlowStart {
  /** The last page's primary was pressed. */
  readonly onDone: () => void;
  /**
   * `Back` on the first page, where there is no page to go back to.
   *
   * A flow opened from settings goes back to settings; the first run has
   * nowhere to go, offers no `Back` there, and leaves this unset.
   */
  readonly onLeave?: (() => void) | undefined;
}

export default function Flow(props: FlowProps) {
  const [state, setState] = createSignal(
    startFlow({
      stages: props.stages,
      agents: props.agents,
      answers: props.answers,
      position: props.position,
    }),
  );
  const [pending, setPending] = createSignal(false);
  const [failure, setFailure] = createSignal<unknown>(null);

  const page = createMemo(() => currentPage(state()));
  const bars = createMemo(() => progress(state()));

  function advance(answers?: Partial<FlowAnswers>): void {
    const next = advanceFlow(state(), answers);
    if (isFinished(next)) {
      props.onDone();
    } else {
      setState(next);
    }
  }

  function record(answers: Partial<FlowAnswers>): void {
    setState(recordAnswers(state(), answers));
  }

  function linked(agent: HarnessKind, account: HarnessAccountView): void {
    const next = finishLink(state(), agent, account);
    if (isFinished(next)) {
      props.onDone();
    } else {
      setState(next);
    }
  }

  function back(): void {
    setFailure(null);
    if (state().position === 0) {
      props.onLeave?.();
    } else {
      setState(backFlow(state()));
    }
  }

  // One page component per visit: the memo re-runs when the page changes,
  // disposing the previous page's signals and timers with it. The call is
  // untracked so that only the page identity — not every signal the page
  // reads while mounting — decides when that happens.
  const view = createMemo(
    on(
      () => `${state().position}:${page().id}`,
      () => {
        setFailure(null);
        return untrack(() => mount(page(), { state, advance, record, linked }));
      },
    ),
  );
  const primary = () => view().primary();
  const disabled = () => primary().disabled !== null || pending();

  async function press(): Promise<void> {
    const button = primary();
    if (button.disabled !== null || pending()) {
      return;
    }
    setFailure(null);
    setPending(true);
    try {
      await button.onClick();
    } catch (error) {
      setFailure(error);
    } finally {
      setPending(false);
    }
  }

  return (
    <div class={styles.page}>
      <section class={styles.card} aria-labelledby="flow-title">
        <div class={styles.bars} aria-hidden="true">
          <For each={bars()}>
            {(bar) => (
              <span class={cx(styles.bar, bar.current && styles.barCurrent)}>
                <span
                  class={styles.fill}
                  style={{ width: `${Math.round(bar.fill * 100)}%` }}
                />
              </span>
            )}
          </For>
        </div>

        <div class={styles.body}>
          <h1 id="flow-title" class={styles.title}>
            {view().title}
          </h1>
          {view().body}
        </div>

        <ProblemNotice error={failure()} />

        <div class={styles.footer}>
          <Show when={state().position > 0 || props.onLeave !== undefined}>
            <button type="button" class={styles.back} onClick={back}>
              Back
            </button>
          </Show>
          <button
            type="button"
            class={styles.primary}
            disabled={disabled()}
            aria-busy={pending() ? "true" : undefined}
            title={primary().disabled ?? undefined}
            onClick={() => void press()}
          >
            {pending() ? (primary().busy ?? primary().label) : primary().label}
          </button>
        </div>
      </section>
    </div>
  );
}

/**
 * Calls the page's component.
 *
 * The registry is typed per page id, and this is the one place the union
 * is narrowed by hand: TypeScript cannot see that `PAGES[page.id]` takes
 * exactly `page`, so it is told.
 */
function mount(
  page: Page,
  props: Omit<Parameters<PageComponent<Page>>[0], "page">,
): PageView {
  const component = PAGES[page.id] as PageComponent<Page>;
  return component({ page, ...props });
}
