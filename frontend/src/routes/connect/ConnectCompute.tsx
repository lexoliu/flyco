/**
 * `/connect/compute` — where the Compute chip goes when nothing is linked.
 *
 * The page is a heading and the chooser (docs/ux.md §7). The chooser itself
 * is a component, because the welcome flow's third screen shows the same one
 * inline and two copies of three cloud wizards would drift apart the first
 * time a console page moved.
 */
import { A } from "@solidjs/router";
import { ArrowLeft } from "lucide-solid";
import ComputeChooser from "../../components/link/ComputeChooser";
import ProblemNotice from "../../components/ProblemNotice";
import { useReadiness } from "../../components/Readiness";
import styles from "./Connect.module.css";

export default function ConnectCompute() {
  const readiness = useReadiness();

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
      </header>

      <ProblemNotice error={readiness.error()} />
      <ComputeChooser />
    </section>
  );
}
