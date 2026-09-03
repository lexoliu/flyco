/**
 * The session's checkout, one directory at a time (docs/ux.md §9.4).
 *
 * Lazy by construction rather than by an optimization: a level fetches when
 * it is opened, because the answer comes from the machine over the relay
 * and walking a whole repository to draw a tree nobody expanded would be
 * minutes of a session VM's time spent on a picture.
 *
 * Ignored files are shown and marked. A `target/`, a `node_modules/` or a
 * stray `.env` is exactly what a user goes looking for when something is
 * wrong, and a tree that quietly omitted them would be lying about the
 * disk.
 */
import { For, Show, createSignal } from "solid-js";
import { createQuery } from "../lib/query";
import { ChevronRight, File, Folder } from "lucide-solid";

import ProblemNotice from "./ProblemNotice";
import { listSessionFiles, type DirectoryEntry } from "../api/client";
import { cx } from "../lib/cx";
import styles from "./FileTree.module.css";

export interface FileTreeProps {
  sessionId: string;
  /** The directory this level lists. Empty is the checkout root. */
  path?: string;
  /** How deep this level sits, for indentation. */
  depth?: number;
  /** The file currently open in the viewer, if any. */
  openPath?: string | null;
  /** Opens a file in the viewer. */
  onOpen: (path: string) => void;
}

export default function FileTree(props: FileTreeProps) {
  const path = () => props.path ?? "";
  const depth = () => props.depth ?? 0;
  const [listing] = createQuery(
    () => ({ session: props.sessionId, path: path() }),
    (key) => listSessionFiles(key.session, key.path),
  );

  return (
    <div class={styles.level}>
      <ProblemNotice error={listing.error} />
      <Show when={listing.loading}>
        <ul class={styles.skeleton} aria-hidden="true">
          <For each={[0, 1, 2]}>{() => <li class={styles.skeletonRow} />}</For>
        </ul>
      </Show>
      <Show when={listing()}>
        {(directory) => (
          <Show
            when={directory().entries.length > 0}
            fallback={<p class={styles.empty}>This directory is empty.</p>}
          >
            <ul class={styles.list} role="group">
              <For each={directory().entries}>
                {(entry) => (
                  <Row
                    entry={entry}
                    depth={depth()}
                    sessionId={props.sessionId}
                    openPath={props.openPath ?? null}
                    onOpen={props.onOpen}
                  />
                )}
              </For>
            </ul>
            <Show when={directory().truncated}>
              <p class={styles.truncated}>
                Only the first {directory().entries.length} entries are shown. Open the terminal to
                read the rest.
              </p>
            </Show>
          </Show>
        )}
      </Show>
    </div>
  );
}

function Row(props: {
  entry: DirectoryEntry;
  depth: number;
  sessionId: string;
  openPath: string | null;
  onOpen: (path: string) => void;
}) {
  const [open, setOpen] = createSignal(false);
  const isDirectory = () => props.entry.kind === "directory";
  const indent = () => ({ "padding-left": `calc(${props.depth} * var(--space-4) + var(--space-2))` });

  return (
    <li>
      <button
        type="button"
        class={cx(styles.row, props.openPath === props.entry.path && styles.rowOpen)}
        style={indent()}
        data-ignored={props.entry.ignored ? "" : undefined}
        aria-expanded={isDirectory() ? open() : undefined}
        onClick={() => (isDirectory() ? setOpen((was) => !was) : props.onOpen(props.entry.path))}
      >
        <Show
          when={isDirectory()}
          fallback={<File size={13} aria-hidden="true" class={styles.icon ?? ""} />}
        >
          <ChevronRight
            size={13}
            aria-hidden="true"
            class={cx(styles.chevron, open() && styles.chevronOpen)}
          />
          <Folder size={13} aria-hidden="true" class={styles.icon ?? ""} />
        </Show>
        <span class={styles.name}>{props.entry.name}</span>
        <Show when={props.entry.ignored}>
          <span class={styles.ignored}>ignored</span>
        </Show>
      </button>
      <Show when={isDirectory() && open()}>
        <FileTree
          sessionId={props.sessionId}
          path={props.entry.path}
          depth={props.depth + 1}
          openPath={props.openPath}
          onOpen={props.onOpen}
        />
      </Show>
    </li>
  );
}
