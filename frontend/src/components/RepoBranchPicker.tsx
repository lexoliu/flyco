/**
 * The branch one repository starts from.
 *
 * Shared by the composer's repository chip (per picked repository) and the
 * session header's add-a-repository row. A popover that can sit inside
 * another: its anchor is a DOM descendant of the outer panel, so the outer
 * popover's outside-click and focus rules leave it open while a branch is
 * chosen.
 *
 * `null` is "the repository's default", resolved by the control plane —
 * picking the default row keeps it `null`, so a repository that renames its
 * default carries the session with it.
 */
import { For, Show, createMemo } from "solid-js";
import { GitBranch } from "lucide-solid";
import Popover from "./Popover";
import ProblemNotice from "./ProblemNotice";
import { listBranches } from "../api/client";
import { beginGithubLogin, githubTokenRevoked } from "../api/auth";
import { createQuery } from "../lib/query";
import { cx } from "../lib/cx";
import styles from "./RepoBranchPicker.module.css";

export default function RepoBranchPicker(props: {
  slug: string;
  branch: string | null;
  onChoose: (branch: string | undefined) => void;
}) {
  const [page] = createQuery(
    () => props.slug,
    (slug: string) => listBranches(slug),
  );

  /** The branch a session would start on right now. */
  const effective = createMemo(
    () => props.branch ?? page()?.branches.find((candidate) => candidate.is_default)?.name ?? null,
  );

  return (
    <Popover
      label={`Branch for ${props.slug}`}
      trigger={(attrs) => (
        <button
          id={attrs.id}
          onClick={attrs.onClick}
          aria-expanded={attrs.expanded()}
          aria-haspopup="dialog"
          type="button"
          class={styles.trigger}
        >
          <GitBranch size={12} aria-hidden="true" />
          {effective() ?? "Branch…"}
        </button>
      )}
    >
      {(close) => (
        <div class={styles.popover}>
          <Show when={page.loading}>
            <p class={styles.note}>Reading branches…</p>
          </Show>
          <Show when={page()}>
            {(loaded) => (
              <ul class={styles.options}>
                <For each={loaded().branches}>
                  {(candidate) => (
                    <li>
                      <button
                        type="button"
                        class={cx(
                          styles.option,
                          effective() === candidate.name && styles.optionChosen,
                        )}
                        onClick={() => {
                          // Choosing the default is choosing *the default*,
                          // not pinning today's name.
                          props.onChoose(candidate.is_default ? undefined : candidate.name);
                          close();
                        }}
                      >
                        {candidate.name}
                        <Show when={candidate.is_default}>
                          <span class={styles.optionMeta}>default</span>
                        </Show>
                      </button>
                    </li>
                  )}
                </For>
              </ul>
            )}
          </Show>
          <ProblemNotice
            error={page.error}
            action={
              githubTokenRevoked(page.error)
                ? { label: "Reconnect GitHub", onClick: () => void beginGithubLogin() }
                : undefined
            }
          />
        </div>
      )}
    </Popover>
  );
}
