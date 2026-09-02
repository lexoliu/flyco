/**
 * `/connect/compute` — where the Compute chip goes when nothing is linked.
 *
 * The route exists now so the chip is a real entry point rather than a dead
 * label; the four-card chooser and the per-provider wizards (docs/ux.md §7)
 * are issue #61 and replace the body of this page, not the page itself.
 */
import { Show } from "solid-js";
import { A, useNavigate } from "@solidjs/router";
import { ArrowLeft, Check } from "lucide-solid";
import Logomark, { AWS_MARK, AZURE_MARK, GOOGLE_CLOUD_MARK } from "../../components/Logomark";
import ProviderLinkForm from "../../components/link/ProviderLinkForm";
import ProblemNotice from "../../components/ProblemNotice";
import { useReadiness } from "../../components/Readiness";
import styles from "./Connect.module.css";

export default function ConnectCompute() {
  const readiness = useReadiness();
  const navigate = useNavigate();

  function onLinked(): void {
    void readiness.refresh().then(() => navigate("/"));
  }

  return (
    <section class={styles.page}>
      <A href="/" class={styles.back}>
        <ArrowLeft size={14} aria-hidden="true" />
        Home
      </A>

      <header class={styles.heading}>
        <h1>Connect compute</h1>
        <p class={styles.lede}>
          Sessions run on a machine in your own cloud account, so you keep the bill, the region and
          the data. Flyco takes spot capacity by default and handles eviction.
        </p>
        <div class={styles.marks}>
          <Logomark mark={AZURE_MARK} size={18} labelled />
          <Logomark mark={AWS_MARK} size={14} labelled />
          <Logomark mark={GOOGLE_CLOUD_MARK} size={18} labelled />
        </div>
      </header>

      <Show when={readiness.compute().length > 0}>
        <p class={styles.linked}>
          <Check size={15} aria-hidden="true" />
          {readiness.compute().length === 1
            ? "One compute account is linked."
            : `${readiness.compute().length} compute accounts are linked.`}
          <A href="/settings/providers">Manage</A>
        </p>
      </Show>

      <ProblemNotice error={readiness.error()} />
      <ProviderLinkForm onLinked={onLinked} />
    </section>
  );
}
