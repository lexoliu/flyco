/**
 * The session switcher that lives in the rail (docs/ux.md §5).
 *
 * The list belongs beside the work rather than on a page of its own: the
 * thing a person does most often is move between running sessions, and a
 * layout that makes them go home first puts a navigation step in front of
 * every one of those moves.
 *
 * It shows the same groups and the same order as the home page used to,
 * because they are the product's own idea of what is urgent — what needs
 * you, then what is running, then what is idle.
 */
import { For, Show, createMemo, createSignal, onCleanup } from "solid-js";
import { A, useMatch } from "@solidjs/router";
import { createQuery } from "../lib/query";
import { listSessions } from "../api/client";
import { cx } from "../lib/cx";
import { deriveStatus, groupSessions, isArchived } from "../lib/status";
import styles from "./SessionNav.module.css";

/**
 * How often the rail re-reads the clock.
 *
 * Slower than the session page's own tick: no row prints a duration, so
 * this exists only so a session that ages out of one group moves into the
 * next without a reload.
 */
const TICK_MS = 5000;

export interface SessionNavProps {
  /** Called when a session is chosen, so a mobile drawer can close. */
  onNavigate?: (() => void) | undefined;
}

export default function SessionNav(props: SessionNavProps) {
  const [sessions] = createQuery(listSessions);
  const [query, setQuery] = createSignal("");
  const [showArchived, setShowArchived] = createSignal(false);
  const current = useMatch(() => "/sessions/:id");

  const [now, setNow] = createSignal(Date.now());
  const ticker = setInterval(() => setNow(Date.now()), TICK_MS);
  onCleanup(() => clearInterval(ticker));

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
        status: deriveStatus(session, now()),
      }));
  });

  const visible = createMemo(() =>
    matching().filter(({ status }) => isArchived(status.status) === showArchived()),
  );

  const groups = createMemo(() =>
    groupSessions(visible().map(({ session, status }) => ({ session, status: status.status }))),
  );

  /** The status a session reads as, by id, so a row need not re-derive it. */
  const statusOf = createMemo(() => {
    const byId = new Map<string, ReturnType<typeof deriveStatus>>();
    for (const entry of matching()) {
      byId.set(entry.session.id, entry.status);
    }
    return byId;
  });

  const archivedCount = createMemo(
    () => matching().filter(({ status }) => isArchived(status.status)).length,
  );

  return (
    <div class={styles.nav}>
      <input
        class={styles.search}
        type="search"
        placeholder="Search sessions"
        aria-label="Search sessions by title or repository"
        value={query()}
        onInput={(event) => setQuery(event.currentTarget.value)}
      />

      {/* Archived is a tab rather than a page: it is the same list read for
          a different reason, and a route for it would be a second place to
          come back from. */}
      <div class={styles.tabs} role="group" aria-label="Filter sessions">
        <button
          type="button"
          class={cx(styles.tab, !showArchived() && styles.tabActive)}
          aria-pressed={!showArchived()}
          onClick={() => setShowArchived(false)}
        >
          Sessions
        </button>
        <Show when={archivedCount() > 0}>
          <button
            type="button"
            class={cx(styles.tab, showArchived() && styles.tabActive)}
            aria-pressed={showArchived()}
            onClick={() => setShowArchived(true)}
          >
            Archived
          </button>
        </Show>
      </div>

      <div class={styles.scroller}>
        <Show
          when={visible().length > 0}
          fallback={
            <p class={styles.empty}>
              <Show
                when={!sessions.loading}
                fallback="Loading…"
              >
                {showArchived() ? "Nothing archived." : "No sessions yet."}
              </Show>
            </p>
          }
        >
          <For each={groups()}>
            {(group) => (
              <div class={styles.group}>
                <Show when={group.heading}>
                  {(heading) => <p class={styles.groupLabel}>{heading()}</p>}
                </Show>
                <ul class={styles.list}>
                  <For each={group.rows}>
                    {(session) => {
                      const status = () => statusOf().get(session.id);
                      return (
                        <li>
                          <A
                            href={`/sessions/${session.id}`}
                            class={cx(
                              styles.row,
                              current()?.params.id === session.id && styles.rowCurrent,
                            )}
                            onClick={() => props.onNavigate?.()}
                            title={`${session.title} — ${status()?.label ?? ""}`}
                          >
                            {/*
                              A colour, and no motion. The rail sits in the
                              corner of the eye for the whole of a session,
                              and a row of pulsing dots out there is the
                              page competing with the work being read in
                              the middle of it.
                            */}
                            <span
                              class={styles.dot}
                              data-tone={status()?.tone}
                              aria-hidden="true"
                            />
                            <span class={styles.title}>{session.title}</span>
                          </A>
                        </li>
                      );
                    }}
                  </For>
                </ul>
              </div>
            )}
          </For>
        </Show>
      </div>
    </div>
  );
}
