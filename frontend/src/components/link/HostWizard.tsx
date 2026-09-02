/**
 * Enrolling a machine the user owns (docs/ux.md §7.5).
 *
 * There is nothing to paste here, because there is no credential to paste.
 * The Worker has no TCP sockets and never dials anybody's hardware: flyco
 * mints one single-use command, the user runs it on the machine, and the
 * machine opens the connection and says what it is. So the wizard is one
 * command, one wait, and then the compute card for whatever arrived.
 *
 * The wait is a state machine rather than a spinner
 * (`src/lib/hostEnrollment.ts`), because it has real outcomes: the machine
 * arrives, or the command goes stale ten minutes after it was minted and
 * the honest thing to offer is a new one rather than a command the control
 * plane will refuse.
 */
import { Match, Switch, createEffect, createSignal, onCleanup, onMount } from "solid-js";
import { RefreshCw } from "lucide-solid";
import CommandBlock from "../../components/CommandBlock";
import HostCard from "../../components/HostCard";
import ProblemNotice from "../../components/ProblemNotice";
import { getEnrollment, mintEnrollmentToken, type HostView } from "../../api/client";
import { formatDuration } from "../../lib/duration";
import {
  IDLE,
  type Enrollment,
  type EnrollmentEvent,
  hasExpired,
  isRunning,
  nextEnrollment,
  pollDelayMs,
  secondsUntilExpiry,
} from "../../lib/hostEnrollment";
import styles from "../../routes/connect/Connect.module.css";

/** How often the countdown redraws. The last seconds are the interesting ones. */
const TICK_MS = 1000;

export interface HostWizardProps {
  /** Called once a machine has enrolled, so readiness can be reloaded. */
  onEnrolled: (host: HostView) => unknown;
}

export default function HostWizard(props: HostWizardProps) {
  const [state, setState] = createSignal<Enrollment>(IDLE);
  const [now, setNow] = createSignal(Date.now());

  let timer: ReturnType<typeof setTimeout> | undefined;
  let closed = false;

  // A poll already in flight still resolves after the wizard goes away; the
  // flag is what stops it writing to a signal nobody is reading.
  onCleanup(() => {
    closed = true;
    clearTimeout(timer);
  });

  function advance(event: EnrollmentEvent): Enrollment {
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
        advance({ kind: "arrived", host: enrollment.host });
        await props.onEnrolled(enrollment.host);
        return;
      }
      schedule(advance({ kind: "pending" }), id);
    } catch (failure) {
      if (!closed) {
        advance({ kind: "failed", error: failure });
      }
    }
  }

  /** Mints a command and starts waiting on it. */
  async function mint(): Promise<void> {
    clearTimeout(timer);
    advance({ kind: "mint" });
    try {
      const token = await mintEnrollmentToken();
      if (closed) {
        return;
      }
      setNow(Date.now());
      schedule(advance({ kind: "minted", token }), token.id);
    } catch (failure) {
      if (!closed) {
        advance({ kind: "failed", error: failure });
      }
    }
  }

  // Minted as the wizard opens: the command *is* the wizard, and a button
  // whose only job is to reveal the one thing on the screen would be a step
  // asking the user to confirm they meant to be here.
  onMount(() => void mint());

  // One clock drives the countdown and the expiry, so the wizard cannot say
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
        advance({ kind: "expire" });
      }
    }, TICK_MS);
    onCleanup(() => clearInterval(ticking));
  });

  const waiting = () =>
    state().step === "waiting" ? (state() as Extract<Enrollment, { step: "waiting" }>) : null;
  const expired = () =>
    state().step === "expired" ? (state() as Extract<Enrollment, { step: "expired" }>) : null;
  const enrolled = () =>
    state().step === "enrolled" ? (state() as Extract<Enrollment, { step: "enrolled" }>) : null;
  const failed = () =>
    state().step === "failed" ? (state() as Extract<Enrollment, { step: "failed" }>) : null;

  return (
    <div class={styles.step}>
      <Switch>
        <Match when={waiting()}>
          {(live) => (
            <>
              <section class={styles.stage}>
                <p class={styles.stageTitle}>1 · Run this on the machine</p>
                {/* Wrapped, not scrolled: this one goes into `sudo sh`, and
                    nobody should run half a line they could not see. */}
                <CommandBlock value={live().token.command} label="Copy command" wrap />
                <p class={styles.hint}>
                  The machine has to be Linux, and it needs Podman — the installer installs Podman
                  when it is absent. Nothing dials in: the machine opens the connection to flyco
                  itself and keeps it open.
                </p>
                <p class={styles.hint}>
                  The command works once, and expires in{" "}
                  <strong>{formatDuration(secondsUntilExpiry(live().token, now()))}</strong>.
                </p>
              </section>

              <section class={styles.stage}>
                <p class={styles.stageTitle}>2 · Wait for it to arrive</p>
                <p class={styles.waiting} role="status">
                  <span class={styles.pulse} aria-hidden="true" />
                  Waiting for the machine…
                </p>
                <p class={styles.hint}>
                  It appears here the moment its daemon says hello. Leaving this page open is fine;
                  the machine is enrolled either way.
                </p>
              </section>
            </>
          )}
        </Match>

        <Match when={expired()}>
          <section class={styles.stage}>
            <p class={styles.stageTitle}>1 · Run this on the machine</p>
            <p class={styles.hint}>
              <strong>The command expired</strong> before any machine ran it. A command is good for
              ten minutes and works once.
            </p>
            <button type="button" class={styles.primary} onClick={() => void mint()}>
              <RefreshCw size={14} aria-hidden="true" />
              Mint a new command
            </button>
          </section>
        </Match>

        <Match when={enrolled()}>
          {(arrived) => (
            <section class={styles.stage}>
              <p class={styles.stageTitle}>Enrolled</p>
              {/* Renaming and removing live in Settings › Compute; this
                  machine has been on screen for two seconds. */}
              <HostCard host={arrived().host} onChanged={() => undefined} editable={false} />
            </section>
          )}
        </Match>

        <Match when={failed()}>
          {(failure) => (
            <section class={styles.stage}>
              <ProblemNotice error={failure().error} />
              <button
                type="button"
                class={styles.primary}
                disabled={isRunning(state())}
                onClick={() => void mint()}
              >
                <RefreshCw size={14} aria-hidden="true" />
                Try again
              </button>
            </section>
          )}
        </Match>

        {/* Minting: the command is one request away, so the step holds its
            own shape rather than collapsing and pushing the page around. */}
        <Match when={isRunning(state()) || state().step === "idle"}>
          <section class={styles.stage}>
            <p class={styles.stageTitle}>1 · Run this on the machine</p>
            <div class={styles.commandSkeleton} aria-label="Preparing the command" />
          </section>
        </Match>
      </Switch>
    </div>
  );
}
