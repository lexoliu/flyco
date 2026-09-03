/**
 * The drawer's `Diff` tab: everything this session has changed.
 *
 * One collapsible section per file, each headed by the path and by what it
 * cost in lines, so the shape of a change is readable before a single hunk
 * is. That header is the answer to the question a user actually opens this
 * tab with — "what did it touch?" — and the hunks are there for when the
 * answer is not enough.
 *
 * # The gate
 *
 * A file whose diff runs past `LARGE_DIFF_LINES` opens as a button rather
 * than as ten thousand rows (docs/ux.md §9). A regenerated lockfile is not
 * a review, and rendering one costs the browser more than the reader got
 * out of it — so the count is stated and the reader decides.
 */
import { For, Show, createMemo, createSignal } from "solid-js";
import { createQuery } from "../lib/query";
import { ChevronRight } from "lucide-solid";

import DiffView from "./DiffView";
import ProblemNotice from "./ProblemNotice";
import RepoStatusPanel from "./RepoStatusPanel";
import { getSessionDiff, type FileDiff } from "../api/client";
import { cx } from "../lib/cx";
import { LARGE_DIFF_LINES, patchRows } from "../lib/patch";
import styles from "./DiffPanel.module.css";

/** What each kind of change is called in the file's header. */
const CHANGE_LABEL: Readonly<Record<FileDiff["change"], string>> = {
  added: "Added",
  modified: "Modified",
  deleted: "Deleted",
  renamed: "Renamed",
};

export interface DiffPanelProps {
  sessionId: string;
  /** Latest `repo_dirty` summary from the relay, when one has arrived. */
  liveRepoSummary?: string | null;
}

export default function DiffPanel(props: DiffPanelProps) {
  const [diff] = createQuery(() => props.sessionId, getSessionDiff);

  return (
    <section class={styles.panel} aria-label="Diff">
      <RepoStatusPanel sessionId={props.sessionId} liveSummary={props.liveRepoSummary ?? null} />

      <ProblemNotice error={diff.error} />
      <Show when={diff.loading}>
        <p class={styles.status}>Diffing the working tree on the machine…</p>
      </Show>
      <Show when={diff()}>
        {(tree) => (
          <Show
            when={tree().files.length > 0}
            fallback={
              <p class={styles.empty}>
                Nothing has changed since this session branched from {tree().base}.
              </p>
            }
          >
            <p class={styles.summary}>
              <span class={styles.files}>
                {tree().files.length} {tree().files.length === 1 ? "file" : "files"} against{" "}
                <span class={styles.base}>{tree().base}</span>
              </span>
              <Counts added={tree().added_lines} removed={tree().removed_lines} />
            </p>
            <ul class={styles.list}>
              <For each={tree().files}>{(file) => <FileSection file={file} />}</For>
            </ul>
            <Show when={tree().truncated}>
              <p class={styles.status}>
                This diff is too large to send whole; the files without a patch below changed by the
                line counts shown. Open the terminal to read them.
              </p>
            </Show>
          </Show>
        )}
      </Show>
    </section>
  );
}

/** The `+n −m` pair, which is the same shape everywhere it appears. */
function Counts(props: { added: number; removed: number }) {
  return (
    <span class={styles.counts}>
      <span class={styles.added}>+{props.added}</span>
      <span class={styles.removed}>−{props.removed}</span>
    </span>
  );
}

function FileSection(props: { file: FileDiff }) {
  const [open, setOpen] = createSignal(false);
  const [loaded, setLoaded] = createSignal(false);
  const rows = createMemo(() => {
    const patch = props.file.patch;
    return patch === null || patch === undefined ? [] : patchRows(patch);
  });
  const large = () => rows().length > LARGE_DIFF_LINES;

  return (
    <li class={styles.file}>
      <button
        type="button"
        class={styles.fileHeader}
        aria-expanded={open()}
        onClick={() => setOpen((was) => !was)}
      >
        <ChevronRight
          size={13}
          aria-hidden="true"
          class={cx(styles.chevron, open() && styles.chevronOpen)}
        />
        <span class={styles.path} title={props.file.path}>
          {props.file.path}
        </span>
        <span class={styles.change}>{CHANGE_LABEL[props.file.change]}</span>
        <Counts added={props.file.added_lines} removed={props.file.removed_lines} />
      </button>

      <Show when={open()}>
        <div class={styles.body}>
          {/*
            * The old path goes here rather than in the header: both paths on
            * one row in a 420px drawer squeezes the one that matters — the
            * file as it is now — down to nothing.
            */}
          <Show when={props.file.previous_path}>
            {(previous) => <p class={styles.previous}>Renamed from {previous()}</p>}
          </Show>
          <Show
            when={rows().length > 0}
            fallback={
              <p class={styles.status}>
                {props.file.binary
                  ? "Binary file — there is nothing to show as text."
                  : "This file's patch was left out of the diff for size."}
              </p>
            }
          >
            <Show
              when={!large() || loaded()}
              fallback={
                <div class={styles.gate}>
                  <span>
                    Large diff · {rows().length.toLocaleString()} lines
                  </span>
                  <button type="button" class={styles.load} onClick={() => setLoaded(true)}>
                    Load diff
                  </button>
                </div>
              }
            >
              <DiffView rows={rows()} label={`Diff of ${props.file.path}`} />
            </Show>
          </Show>
        </div>
      </Show>
    </li>
  );
}
