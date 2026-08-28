import { useParams } from "@solidjs/router";
import BudgetBar from "../components/BudgetBar";
import UsageMeter from "../components/UsageMeter";
import ApprovalsPanel from "../components/ApprovalsPanel";
import TerminalPanel from "../components/terminal/TerminalPanel";
import styles from "./SessionDetail.module.css";

/**
 * The session view shell. Nothing here is fetched yet — every meter and
 * panel renders its own "not loaded" state until GET /v1/sessions/{id}
 * and the relay socket are wired through the generated client.
 */
export default function SessionDetail() {
  const params = useParams<{ id: string }>();

  return (
    <section class={styles.page}>
      <header class={styles.header}>
        <h1>{params.id}</h1>
      </header>

      <div class={styles.meters}>
        <BudgetBar label="Budget" />
        <UsageMeter label="LLM usage" unit="tokens" />
        <UsageMeter label="Context window" unit="tokens" />
      </div>

      <div class={styles.body}>
        <div class={styles.transcript} aria-label="Transcript">
          <p class={styles.empty}>No turns yet. Send the first message to get started.</p>
        </div>
        <aside class={styles.side}>
          <ApprovalsPanel approvals={[]} />
          <TerminalPanel />
        </aside>
      </div>
    </section>
  );
}
