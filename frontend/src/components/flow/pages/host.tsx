/**
 * Enrolling a machine the user owns (docs/ux.md §4 C2‴, §7.5), on one page.
 *
 * There is nothing to paste, because there is no credential to paste. The
 * Worker has no TCP sockets and never dials anybody's hardware: flyco mints
 * one single-use command as the page opens, the user runs it on the
 * machine, and the machine opens the connection and says what it is. The
 * page is the command and the wait; the machine arriving advances the flow
 * by itself.
 *
 * The wait is the state machine in `src/lib/hostEnrollment.ts`, because it
 * has real outcomes: the machine arrives, or the command goes stale ten
 * minutes after it was minted and the honest thing to offer is a new one
 * rather than a command the control plane will refuse.
 */
import { Show, createEffect, createSignal, onCleanup, onMount } from "solid-js";
import CommandBlock from "../../CommandBlock";
import ProblemNotice from "../../ProblemNotice";
import { useReadiness } from "../../Readiness";
import { getEnrollment, mintEnrollmentToken } from "../../../api/client";
import { formatDuration } from "../../../lib/duration";
import {
  IDLE,
  hasExpired,
  nextEnrollment,
  pollDelayMs,
  secondsUntilExpiry,
  type Enrollment,
  type EnrollmentEvent,
} from "../../../lib/hostEnrollment";
import type { PageComponent, Primary } from "../page";
import { Waiting } from "./shared";
import styles from "./pages.module.css";

/** How often the countdown redraws. The last seconds are the interesting ones. */
const TICK_MS = 1000;

export const HostEnroll: PageComponent<{ id: "host-enroll" }> = (props) => {
  const readiness = useReadiness();
  const [state, setState] = createSignal<Enrollment>(IDLE);
  const [now, setNow] = createSignal(Date.now());

  let timer: ReturnType<typeof setTimeout> | undefined;
  let closed = false;

  // A poll already in flight still resolves after the page goes away; the
  // flag is what stops it writing to a signal nobody is reading.
  onCleanup(() => {
    closed = true;
    clearTimeout(timer);
  });

  function transition(event: EnrollmentEvent): Enrollment {
    const next = nextEnrollment(state(), event);
    setState(next);
    return next;
  }

  function schedule(current: Enrollment, id: string): void {
    const delay = pollDelayMs(current);
    if (delay === null || closed) {
      return;
    }
    timer = setTimeout(() => void ask(id), delay);
  }

  async function ask(id: string): Promise<void> {
    try {
      const enrollment = await getEnrollment(id);
      if (closed) {
        return;
      }
      if (enrollment.status === "enrolled") {
        transition({ kind: "arrived", host: enrollment.host });
        await readiness.refresh();
        props.advance();
        return;
      }
      schedule(transition({ kind: "pending" }), id);
    } catch (failure) {
      if (!closed) {
        transition({ kind: "failed", error: failure });
      }
    }
  }

  /** Mints a command and starts waiting on it. */
  async function mint(): Promise<void> {
    clearTimeout(timer);
    transition({ kind: "mint" });
    try {
      const token = await mintEnrollmentToken();
      if (closed) {
        return;
      }
      setNow(Date.now());
      schedule(transition({ kind: "minted", token }), token.id);
    } catch (failure) {
      if (!closed) {
        transition({ kind: "failed", error: failure });
      }
    }
  }

  // Minted as the page opens: the command *is* the page, and a button
  // whose only job is to reveal the one thing on the screen would be a
  // step asking the user to confirm they meant to be here.
  onMount(() => void mint());

  // One clock drives the countdown and the expiry, so the page cannot say
  // "expires in 3s" while already treating the command as stale. It runs
  // only while there is something to count down.
  createEffect(() => {
    if (state().step !== "waiting") {
      return;
    }
    const ticking = setInterval(() => {
      const at = Date.now();
      setNow(at);
      if (hasExpired(state(), at)) {
        transition({ kind: "expire" });
      }
    }, TICK_MS);
    onCleanup(() => clearInterval(ticking));
  });

  const waiting = () =>
    state().step === "waiting"
      ? (state() as Extract<Enrollment, { step: "waiting" }>)
      : null;
  const failed = () =>
    state().step === "failed"
      ? (state() as Extract<Enrollment, { step: "failed" }>)
      : null;

  const primary = (): Primary => {
    switch (state().step) {
      case "expired":
        return {
          label: "Mint a new command",
          busy: "Minting…",
          disabled: null,
          onClick: mint,
        };
      case "failed":
        return {
          label: "Try again",
          busy: "Minting…",
          disabled: null,
          onClick: mint,
        };
      case "idle":
      case "minting":
        return {
          label: "Next",
          disabled: "Preparing the command…",
          onClick: () => undefined,
        };
      case "waiting":
      case "enrolled":
        return {
          label: "Next",
          disabled: "Run the command on the machine to continue",
          onClick: () => undefined,
        };
    }
  };

  return {
    title: "Run this on the machine",
    body: (
      <>
        <Show when={waiting()}>
          {(live) => (
            <>
              {/* Wrapped, not scrolled: this one goes into `sudo sh`, and
                  nobody should run half a line they could not see. */}
              <CommandBlock value={live().token.command} label="Copy" wrap />
              <p class={styles.hint}>
                The machine has to be Linux, and it needs Podman — the installer
                installs Podman when it is absent. Nothing dials in: the machine
                opens the connection to flyco itself and keeps it open.
              </p>
              <p class={styles.hint}>
                The command works once, and expires in{" "}
                <strong>
                  {formatDuration(secondsUntilExpiry(live().token, now()))}
                </strong>
                .
              </p>
              <Waiting>Waiting for the machine…</Waiting>
              <p class={styles.hint}>
                This page moves on the moment the machine's daemon says hello.
              </p>
            </>
          )}
        </Show>

        <Show when={state().step === "expired"}>
          <p class={styles.lede}>
            <strong>The command expired</strong> before any machine ran it. A
            command is good for ten minutes and works once.
          </p>
        </Show>

        <Show when={failed()}>
          {(failure) => <ProblemNotice error={failure().error} />}
        </Show>

        {/* Minting: the command is one request away, so the page holds its
            shape rather than collapsing and pushing the footer around. */}
        <Show when={state().step === "idle" || state().step === "minting"}>
          <div
            class={`${styles.skeleton} ${styles.skeletonCommand}`}
            aria-label="Preparing the command"
          />
        </Show>
      </>
    ),
    primary,
  };
};
