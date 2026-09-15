import { For, Show, createMemo } from "solid-js";
import { createQuery } from "../lib/query";
import ProblemNotice from "./ProblemNotice";
import { getRepoStatus, type SessionRepo } from "../api/client";
import styles from "./MachinePanel.module.css";

/**
 * Working-tree status of a session's checkouts. Dirtiness is load-bearing
 * on the backend (an agent may not stop while a tree is dirty), so this is
 * shown plainly rather than folded into another panel — one line per
 * checkout, because a session can work across several repositories and
 * `dirty` without *which* would be half the answer.
 */
export default function RepoStatusPanel(props: {
  sessionId: string;
  /**
   * The session's checkouts, as `SessionSummary.repos` lists them — what a
   * `dir` is *called*. The daemon reports by `dir`, and a bare directory
   * name is not what a reader opened the session for.
   */
  repos: readonly SessionRepo[];
  /**
   * Whether the session's machine is a developer's own (`host`) machine —
   * the shape whose workdir is itself the checkout, reported under no dir.
   */
  devMachine: boolean;
  /** Latest `repo_dirty` summary per checkout `dir`, from the relay. */
  liveSummaries?: ReadonlyMap<string, string> | undefined;
}) {
  const [status] = createQuery(() => props.sessionId, getRepoStatus);

  /**
   * One row per checkout the daemon has reported on, with the live relay's
   * latest summary laid over the stored one — a tree that just went dirty
   * should not wait for a refetch to say so.
   */
  const checkouts = createMemo(() =>
    (status()?.checkouts ?? []).map((checkout) => {
      const dir = checkout.dir ?? "";
      const live = props.liveSummaries?.get(dir);
      if (live === undefined) {
        return checkout;
      }
      return { ...checkout, dirty: live.trim() !== "", summary: live };
    }),
  );

  /** What the session row calls a `dir` — `flyco/` reads better bare. */
  const nameOf = (dir: string | null | undefined): string =>
    dir === null || dir === undefined
      ? "working tree"
      : (props.repos.find((repo) => repo.dir === dir)?.slug ?? dir);

  return (
    <section class={styles.panel} aria-label="Repository status">
      <h2>{props.devMachine ? "Repository" : "Repositories"}</h2>
      <ProblemNotice error={status.error} />
      <Show when={checkouts().length > 0}>
        <ul class={styles.facts}>
          <For each={checkouts()}>
            {(checkout) => (
              <li>
                <span class={styles.state} data-state={checkout.dirty ? "dirty" : "running"}>
                  {nameOf(checkout.dir)} · {checkout.dirty ? "Dirty" : "Clean"}
                </span>
                <Show when={checkout.summary !== ""}>
                  <pre class={styles.facts}>{checkout.summary}</pre>
                </Show>
              </li>
            )}
          </For>
        </ul>
      </Show>
    </section>
  );
}
