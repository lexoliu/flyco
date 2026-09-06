/**
 * Home: one question, and what has to be true before it can be answered
 * (docs/ux.md §5).
 *
 * The whole page is one column at 720px, and the only thing on it is the
 * composer that opens a session. The list of sessions lives in the rail,
 * where it is reachable from inside a session too; a page that repeated it
 * would be a second copy of the same list, one navigation further away.
 *
 * Nothing here is a form the user could submit and have refused: the
 * readiness cards appear only while a prerequisite is missing, and the
 * composer's send button says which one is in the way.
 */
import { Match, Show, Switch, createMemo } from "solid-js";
import { createQuery } from "../lib/query";
import { A, Navigate, useNavigate } from "@solidjs/router";
import { ChevronRight, Cpu, Sparkles } from "lucide-solid";
import Composer from "../components/Composer";
import ProblemNotice from "../components/ProblemNotice";
import { useReadiness } from "../components/Readiness";
import { getMe, listSessions } from "../api/client";
import { requestNewSession, type NewSessionInput } from "../api/sessions";
import { welcomeDismissed } from "../lib/localPreferences";
import styles from "./Home.module.css";

export default function Home() {
  const navigate = useNavigate();
  const readiness = useReadiness();
  const [sessions, { refetch }] = createQuery(listSessions);
  const [me] = createQuery(getMe);

  /** Sessions still holding a machine, which is what the cap counts. */
  const live = createMemo(
    () =>
      (sessions() ?? []).filter(
        (session) => session.state !== "archived" && session.state !== "failed",
      ).length,
  );

  const nearCap = createMemo(() => {
    const cap = me()?.session_cap;
    return cap !== undefined && live() >= cap - 1;
  });

  async function onSend(input: NewSessionInput): Promise<void> {
    const created = await requestNewSession(input);
    await refetch();
    navigate(`/sessions/${created.id}`);
  }

  /**
   * Where a first visit goes, decided only once readiness is known.
   *
   * The home page must not paint while that is still being read: a signed-in
   * account with nothing linked would see the composer for a frame and then
   * be thrown to `/welcome`, which reads as a glitch. A readiness read that
   * failed is not "not ready" — it is a page that could not tell — so it
   * shows the home page with the failure rather than the first run.
   */
  const gate = createMemo((): "undecided" | "home" | "welcome" => {
    if (
      readiness.ready() ||
      welcomeDismissed() ||
      readiness.error() !== undefined
    ) {
      return "home";
    }
    return readiness.loading() ? "undecided" : "welcome";
  });

  return (
    <Switch>
      <Match when={gate() === "welcome"}>
        <Navigate href="/welcome" />
      </Match>
      <Match when={gate() === "undecided"}>
        <section class={styles.page} aria-busy="true" aria-label="Loading" />
      </Match>
      <Match when={gate() === "home"}>
        <section class={styles.page}>
          <h1 class={styles.greeting}>
            <Show when={me()} fallback="What should we build?">
              {(user) => `Welcome back, ${user().login}`}
            </Show>
          </h1>

          <Composer onSend={onSend} />

          <Show when={!readiness.loading() && !readiness.ready()}>
            <div class={styles.readiness}>
              <Show when={readiness.harness().length === 0}>
                <A href="/connect/harness" class={styles.readinessCard}>
                  <Sparkles size={18} aria-hidden="true" />
                  <span class={styles.readinessText}>
                    <span class={styles.readinessTitle}>Give it a brain</span>
                    <span class={styles.readinessDetail}>
                      Link Claude Code or Codex. Sessions run on your own plan.
                    </span>
                  </span>
                  <span class={styles.readinessAction}>
                    Connect an agent
                    <ChevronRight size={14} aria-hidden="true" />
                  </span>
                </A>
              </Show>
              <Show when={readiness.compute().length === 0}>
                <A href="/connect/compute" class={styles.readinessCard}>
                  <Cpu size={18} aria-hidden="true" />
                  <span class={styles.readinessText}>
                    <span class={styles.readinessTitle}>
                      Give it a computer
                    </span>
                    <span class={styles.readinessDetail}>
                      Link Azure, AWS, Google Cloud or a machine you own. The
                      machine stays yours.
                    </span>
                  </span>
                  <span class={styles.readinessAction}>
                    Add compute
                    <ChevronRight size={14} aria-hidden="true" />
                  </span>
                </A>
              </Show>
            </div>
          </Show>

          <ProblemNotice
            error={sessions.error ?? me.error ?? readiness.error()}
          />

          {/* The cap is only worth saying when it is about to bite. */}
          <Show when={nearCap()}>
            <p class={styles.cap}>
              {live()} of {me()?.session_cap} sessions in use.
            </p>
          </Show>
        </section>
      </Match>
    </Switch>
  );
}
