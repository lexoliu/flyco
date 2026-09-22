/**
 * The session switcher that lives in the rail (docs/ux.md §3).
 *
 * The list belongs beside the work rather than on a page of its own: the
 * thing a person does most often is move between running sessions, and a
 * layout that makes them go home first puts a navigation step in front of
 * every one of those moves.
 *
 * Grouped by repository, the way the official apps group by project,
 * because that is how a person remembers a session: "the helios one", not
 * "the idle one". Within a repository the newest is first. What a session
 * is doing is a dot, and only when it matters — a session that is working,
 * building its machine, or failed carries one; a session at rest carries
 * nothing, and neither does one whose agent has answered and is waiting
 * for a reply, because after a day's work that is every session there is.
 * A rail of forty coloured dots is a rail that says nothing.
 */
import { For, Show, createMemo, createSignal, onCleanup } from "solid-js";
import { A, useMatch } from "@solidjs/router";
import { createQuery } from "../lib/query";
import { listSessions, type SessionSummary } from "../api/client";
import { cx } from "../lib/cx";
import Skeleton from "./Skeleton";
import { deriveStatus, isArchived, type StatusView } from "../lib/status";
import styles from "./SessionNav.module.css";

/**
 * How often the rail re-reads the clock.
 *
 * Slower than the session page's own tick: no row prints a duration, so
 * this exists only so a session that changes state moves its dot without a
 * reload.
 */
const TICK_MS = 5000;

/** One repository's sessions, newest first. */
interface RepoGroup {
  repo: string;
  rows: { session: SessionSummary; status: StatusView }[];
}

/**
 * Divides sessions by repository, ordered by each repository's most recent
 * activity, so the project being worked on today is at the top.
 */
export function groupByRepo(
  rows: readonly { session: SessionSummary; status: StatusView }[],
): RepoGroup[] {
  const byRepo = new Map<string, RepoGroup>();
  for (const row of rows) {
    // The group is the primary repository — `repos[0]` — because that is
    // what the session's own header names, and a rail that grouped by
    // anything else would disagree with the page it opens.
    const repo = row.session.repos[0]?.slug ?? "";
    const held = byRepo.get(repo);
    if (held === undefined) {
      byRepo.set(repo, { repo, rows: [row] });
    } else {
      held.rows.push(row);
    }
  }
  const latest = (group: RepoGroup) =>
    Math.max(...group.rows.map((row) => row.session.last_active_unix));
  return [...byRepo.values()]
    .map((group) => ({
      ...group,
      rows: [...group.rows].sort(
        (left, right) => right.session.last_active_unix - left.session.last_active_unix,
      ),
    }))
    .sort((left, right) => latest(right) - latest(left));
}

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
          session.repos.some((repo) => repo.slug.toLowerCase().includes(needle)),
      )
      .map((session) => ({
        session,
        status: deriveStatus(session, now()),
      }));
  });

  const visible = createMemo(() =>
    matching().filter(({ status }) => isArchived(status.status) === showArchived()),
  );

  const groups = createMemo(() => groupByRepo(visible()));

  const archivedCount = createMemo(
    () => matching().filter(({ status }) => isArchived(status.status)).length,
  );

  return (
    <div class={styles.nav}>
      <input
        class={styles.search}
        type="search"
        placeholder="Search"
        aria-label="Search sessions by title or repository"
        value={query()}
        onInput={(event) => setQuery(event.currentTarget.value)}
      />

      <div class={styles.scroller}>
        <Show
          when={visible().length > 0}
          fallback={
            /*
              Nothing is said about an empty list until the list has been
              read: the rail used to render "No sessions yet" in the
              moment before the first response and correct itself a frame
              later, which is the flash a person sees on every load.
            */
            <Show when={sessions.latest !== undefined} fallback={<Skeleton lines={6} />}>
              <p class={styles.empty}>
                {showArchived() ? "Nothing archived." : "No sessions yet."}
              </p>
            </Show>
          }
        >
          <For each={groups()}>
            {(group) => (
              <div class={styles.group}>
                <p class={styles.groupLabel} title={group.repo}>
                  {group.repo}
                </p>
                <ul class={styles.list}>
                  <For each={group.rows}>
                    {({ session, status }) => (
                      <li>
                        <A
                          href={`/sessions/${session.id}`}
                          class={cx(
                            styles.row,
                            current()?.params.id === session.id && styles.rowCurrent,
                          )}
                          onClick={() => props.onNavigate?.()}
                          title={`${session.title} — ${status.detail === undefined ? status.label : `${status.label} · ${status.detail}`}`}
                        >
                          {/*
                            A colour, and no motion. The rail sits in the
                            corner of the eye for the whole of a session,
                            and a row of pulsing dots out there is the
                            page competing with the work being read in the
                            middle of it. A session at rest, or waiting
                            on a reply, has no dot at all: the slot is
                            kept so titles line up.
                          */}
                          <span
                            class={styles.dot}
                            data-tone={status.tone}
                            aria-hidden="true"
                          />
                          <span class={styles.title}>{session.title}</span>
                        </A>
                      </li>
                    )}
                  </For>
                </ul>
              </div>
            )}
          </For>
        </Show>
      </div>

      {/*
        Archived sessions are the same list read for a different reason,
        and one quiet line at the foot is all the way in it needs: a tab
        row above the list was a second thing to read before the first
        session, on every page.
      */}
      <Show when={archivedCount() > 0 || showArchived()}>
        <button
          type="button"
          class={styles.archivedToggle}
          aria-pressed={showArchived()}
          onClick={() => setShowArchived((was) => !was)}
        >
          {showArchived() ? "Back to sessions" : `Archived · ${archivedCount()}`}
        </button>
      </Show>
    </div>
  );
}
