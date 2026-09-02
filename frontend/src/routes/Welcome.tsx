/**
 * `/welcome` — three screens in one card (docs/ux.md §4).
 *
 * Shown after the first sign-in while readiness is incomplete, and never
 * again once it has been dismissed. Every step can be skipped: anything
 * skipped comes back as a readiness card on the home page, which is a
 * better place to be nagged than a wizard nobody can leave.
 */
import { Match, Show, Switch, createSignal } from "solid-js";
import { useNavigate } from "@solidjs/router";
import { Check } from "lucide-solid";
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
              <Show
                when={readiness.harness().length === 0}
                fallback={
                  <p class={styles.done}>
                    <Check size={15} aria-hidden="true" />
                    An agent is linked.
                  </p>
                }
              >
                <HarnessChooser onLinked={() => void readiness.refresh()} />
              </Show>
            </Match>

            <Match when={step() === 2}>
              <h1>Give it a computer</h1>
              <p class={styles.lede}>
                Sessions run on a machine in your own cloud account, so you keep the bill, the
                region and the data. Spot capacity by default; flyco handles eviction.
              </p>
              <Show
                when={readiness.compute().length === 0}
                fallback={
                  <p class={styles.done}>
                    <Check size={15} aria-hidden="true" />
                    Compute is linked.
                  </p>
                }
              >
                <ComputeChooser />
              </Show>
            </Match>
          </Switch>
        </div>

        <div class={styles.actions}>
          <Show when={step() > 0}>
            <button type="button" class={styles.back} onClick={() => setStep(step() - 1)}>
              Back
            </button>
          </Show>
          <Show when={step() > 0}>
            <button type="button" class={styles.skip} onClick={finish}>
              Skip for now
            </button>
          </Show>
          <button
            type="button"
            class={cx(styles.next, step() === 0 && styles.nextAlone)}
            onClick={advance}
          >
            {step() === LAST_STEP ? "Start building" : "Next"}
          </button>
        </div>
      </div>
    </div>
  );
}
