/**
 * Home: the composer and the sessions it produced (docs/ux.md §5).
 *
 * The whole page is one column at 720px. Nothing here is a form the user
 * could submit and have refused: the readiness cards appear only while a
 * prerequisite is missing, and the composer's send button says which one is
 * in the way.
 */
import {
  For,
  Match,
  Show,
  Switch,
  createMemo,
  createSignal,
  onCleanup,
} from "solid-js";
import { createQuery } from "../lib/query";
import { A, Navigate, useNavigate } from "@solidjs/router";
import { ChevronRight, Cpu, Sparkles } from "lucide-solid";
import Composer from "../components/Composer";
import ProblemNotice from "../components/ProblemNotice";
import SessionRow from "../components/SessionRow";
import { useReadiness } from "../components/Readiness";
import { getMe, listSessions } from "../api/client";
import { requestNewSession, type NewSessionInput } from "../api/sessions";
import { cx } from "../lib/cx";
import { welcomeDismissed } from "../lib/localPreferences";
import { deriveStatus, groupSessions, isArchived } from "../lib/status";
import styles from "./Home.module.css";

/**
 * How often the clock the list renders against advances.
 *
 * A provisioning session counts its wait in the row, so the page needs a
 * tick — but a coarse one: the labels are in seconds, minutes and hours,
 * and re-rendering faster than the smallest unit is motion with no
 * information in it.
 */
const TICK_MS = 1000;

export default function Home() {
  const navigate = useNavigate();
  const readiness = useReadiness();
  const [sessions, { refetch }] = createQuery(listSessions);
  const [me] = createQuery(getMe);
  const [query, setQuery] = createSignal("");
  const [showArchived, setShowArchived] = createSignal(false);

  const [now, setNow] = createSignal(Date.now());
  const ticker = setInterval(() => setNow(Date.now()), TICK_MS);
  onCleanup(() => clearInterval(ticker));

  /** Sessions matching the search, with the status each one reads as. */
  const matching = createMemo(() => {
    const needle = query().trim().toLowerCase();
    return (sessions() ?? [])
      .filter(
        (session) =>
          needle === "" ||
          session.title.toLowerCase().includes(needle) ||
          session.repo.toLowerCase().includes(needle),
      )
      .map((session) => ({
        session,
        status: deriveStatus(session, now()).status,
      }));
  });

  const visible = createMemo(() =>
    matching().filter(({ status }) => isArchived(status) === showArchived()),
  );

  /** The visible sessions, under the headings docs/ux.md §5 divides them by. */
  const groups = createMemo(() => groupSessions(visible()));

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

          <div class={styles.sessions}>
            <div class={styles.sessionsHeader}>
              <div
                class={styles.tabs}
                role="group"
                aria-label="Filter sessions"
              >
                <button
                  type="button"
                  class={cx(styles.tab, !showArchived() && styles.tabActive)}
                  aria-pressed={!showArchived()}
                  onClick={() => setShowArchived(false)}
                >
                  Sessions
                </button>
                <button
                  type="button"
                  class={cx(styles.tab, showArchived() && styles.tabActive)}
                  aria-pressed={showArchived()}
                  onClick={() => setShowArchived(true)}
                >
                  Archived
                </button>
              </div>
              <input
                class={styles.search}
                type="search"
                placeholder="Search"
                aria-label="Search sessions by title or repository"
                value={query()}
                onInput={(event) => setQuery(event.currentTarget.value)}
              />
            </div>

            <Show
              when={visible().length > 0}
              fallback={
                <p class={styles.empty}>
                  <Show
                    when={sessions.loading}
                    fallback={
                      showArchived()
                        ? "No archived sessions."
                        : "No sessions yet. Describe a task above to start one."
                    }
                  >
                    Loading sessions…
                  </Show>
                </p>
              }
            >
              <For each={groups()}>
                {(group) => (
                  <div class={styles.group}>
                    <Show when={group.heading}>
                      {(heading) => (
                        <p class={styles.groupLabel}>{heading()}</p>
                      )}
                    </Show>
                    <ul class={styles.list}>
                      <For each={group.rows}>
                        {(session) => (
                          <li>
                            <SessionRow session={session} now={now()} />
                          </li>
                        )}
                      </For>
                    </ul>
                  </div>
                )}
              </For>
            </Show>

            {/* The cap is only worth saying when it is about to bite. */}
            <Show when={nearCap()}>
              <p class={styles.cap}>
                {live()} of {me()?.session_cap} sessions in use.
              </p>
            </Show>
          </div>
        </section>
      </Match>
    </Switch>
  );
}
