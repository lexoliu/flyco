/**
 * `/welcome` — three screens in one card (docs/ux.md §4).
 *
 * Shown after the first sign-in while readiness is incomplete, and never
 * again once it has been completed. It is a gate, not a tour: a session
 * cannot exist without an agent and a machine, so each step's `Next` stays
 * disabled until that step's prerequisite is actually met.
 */
import { Match, Show, Switch, createMemo, createSignal } from "solid-js";
import { useNavigate } from "@solidjs/router";
import Logomark, {
  ANTHROPIC_MARK,
  AWS_MARK,
  AZURE_MARK,
  GOOGLE_CLOUD_MARK,
  OPENAI_MARK,
} from "../components/Logomark";
import ComputeChooser from "../components/link/ComputeChooser";
import HarnessChooser from "../components/link/HarnessChooser";
import { useReadiness } from "../components/Readiness";
import { dismissWelcome } from "../lib/localPreferences";
import { cx } from "../lib/cx";
import styles from "./Welcome.module.css";

const LAST_STEP = 2;

export default function Welcome() {
  const navigate = useNavigate();
  const readiness = useReadiness();
  const [step, setStep] = createSignal(0);

  /** Ends the flow for good; the card never returns. */
  function finish(): void {
    dismissWelcome();
    navigate("/", { replace: true });
  }

  function advance(): void {
    if (step() === LAST_STEP) {
      finish();
    } else {
      setStep(step() + 1);
    }
  }

  /** Whether the current step's own prerequisite is met. */
  const satisfied = createMemo(() => {
    switch (step()) {
      case 1:
        return readiness.harness().length > 0;
      case 2:
        return readiness.compute().length > 0;
      default:
        return true;
    }
  });
  const missing = createMemo(() =>
    step() === 1 ? "Connect Claude Code or Codex to continue" : "Connect compute to continue",
  );

  return (
    <div class={styles.page}>
      <div class={styles.card}>
        <div class={styles.steps} aria-hidden="true">
          {[0, 1, 2].map((index) => (
            <span
              class={cx(
                styles.step,
                index < step() && styles.stepDone,
                index === step() && styles.stepCurrent,
              )}
            />
          ))}
        </div>

        <div class={styles.body}>
          <Switch>
            <Match when={step() === 0}>
              <h1>Meet flyco</h1>
              <p class={styles.lede}>
                Flyco runs the official Claude Code and Codex on a computer you own. You bring the
                agent and the machine; flyco runs the session, keeps the budget, and gets out of
                the way.
              </p>
              <div class={styles.marks}>
                <Logomark mark={ANTHROPIC_MARK} size={20} labelled />
                <Logomark mark={OPENAI_MARK} size={20} labelled />
                <Logomark mark={AZURE_MARK} size={20} labelled />
                <Logomark mark={AWS_MARK} size={15} labelled />
                <Logomark mark={GOOGLE_CLOUD_MARK} size={20} labelled />
              </div>
            </Match>

            <Match when={step() === 1}>
              <h1>Give it a brain</h1>
              <p class={styles.lede}>
                Link the agent you already pay for. Flyco never resells tokens — every turn is
                billed by Anthropic or OpenAI to your own account.
              </p>
              <HarnessChooser onLinked={() => void readiness.refresh()} />
            </Match>

            <Match when={step() === 2}>
              <h1>Give it a computer</h1>
              <p class={styles.lede}>
                Sessions run on a machine in your own cloud account, so you keep the bill, the
                region and the data. Spot capacity by default; flyco handles eviction.
              </p>
              <ComputeChooser />
            </Match>
          </Switch>
        </div>

        <div class={styles.actions}>
          <Show when={step() > 0}>
            <button type="button" class={styles.back} onClick={() => setStep(step() - 1)}>
              Back
            </button>
          </Show>
          <button
            type="button"
            class={cx(styles.next, step() === 0 && styles.nextAlone)}
            disabled={!satisfied()}
            title={satisfied() ? undefined : missing()}
            onClick={advance}
          >
            {step() === LAST_STEP ? "Start building" : "Next"}
          </button>
        </div>
      </div>
    </div>
  );
}
