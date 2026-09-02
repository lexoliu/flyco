/**
 * `/connect/harness` — where the Harness chip goes when nothing is linked.
 *
 * The route exists now so the chip is a real entry point rather than a dead
 * label; the two-card chooser with Claude's OAuth flow (docs/ux.md §8) is
 * issue #60 and replaces the body of this page, not the page itself.
 */
import { Show } from "solid-js";
import { A, useNavigate } from "@solidjs/router";
import { ArrowLeft, Check } from "lucide-solid";
import Logomark, { ANTHROPIC_MARK, OPENAI_MARK } from "../../components/Logomark";
import HarnessLinkForm from "../../components/link/HarnessLinkForm";
import ProblemNotice from "../../components/ProblemNotice";
import { useReadiness } from "../../components/Readiness";
import styles from "./Connect.module.css";

export default function ConnectHarness() {
  const readiness = useReadiness();
  const navigate = useNavigate();

  async function onLinked(): Promise<void> {
    await readiness.refresh();
    navigate("/");
  }

  return (
    <section class={styles.page}>
      <A href="/" class={styles.back}>
        <ArrowLeft size={14} aria-hidden="true" />
        Home
      </A>

      <header class={styles.heading}>
        <h1>Connect an agent</h1>
        <p class={styles.lede}>
          Flyco runs the official Claude Code and Codex, on your own account. Link one and every
          session you start is billed to your Claude or OpenAI plan — flyco never resells tokens.
        </p>
        <div class={styles.marks}>
          <Logomark mark={ANTHROPIC_MARK} size={18} labelled />
          <Logomark mark={OPENAI_MARK} size={18} labelled />
        </div>
      </header>

      <Show when={readiness.harness().length > 0}>
        <p class={styles.linked}>
          <Check size={15} aria-hidden="true" />
          {readiness.harness().length === 1
            ? "One agent is linked."
            : `${readiness.harness().length} agents are linked.`}
          <A href="/settings/harness-accounts">Manage</A>
        </p>
      </Show>

      <ProblemNotice error={readiness.error()} />
      <HarnessLinkForm onLinked={() => void onLinked()} />
    </section>
  );
}
