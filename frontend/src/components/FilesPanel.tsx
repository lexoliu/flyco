/**
 * The drawer's `Files` tab: the session's checkout, read-only.
 *
 * Master then detail rather than side by side. The drawer is one narrow
 * column, and a tree and a code view sharing it would give neither enough
 * room to be read — so opening a file replaces the tree, and the path in
 * the header is the way back.
 *
 * Read-only is the product decision, not a limitation: the working tree
 * belongs to the agent, and two writers on one disk is how a session loses
 * work. What this is for is *seeing* what the agent did.
 */
import { Show, createSignal } from "solid-js";
import { createQuery } from "../lib/query";
import { ChevronLeft } from "lucide-solid";

import ProblemNotice from "./ProblemNotice";
import { readSessionFile } from "../api/client";
import { highlightHtml, languageOf } from "../lib/highlight";
import styles from "./FilesPanel.module.css";
import FileTree from "./FileTree";

export default function FilesPanel(props: { sessionId: string }) {
  const [open, setOpen] = createSignal<string | null>(null);

  return (
    <section class={styles.panel} aria-label="Files">
      <Show
        when={open()}
        fallback={
          <FileTree sessionId={props.sessionId} openPath={null} onOpen={(path) => setOpen(path)} />
        }
      >
        {(path) => (
          <FileView sessionId={props.sessionId} path={path()} onClose={() => setOpen(null)} />
        )}
      </Show>
    </section>
  );
}

function FileView(props: { sessionId: string; path: string; onClose: () => void }) {
  const [file] = createQuery(
    () => ({ session: props.sessionId, path: props.path }),
    (key) => readSessionFile(key.session, key.path),
  );
  const language = () => languageOf(props.path);

  return (
    <div class={styles.file}>
      <div class={styles.fileHeader}>
        <button type="button" class={styles.back} onClick={props.onClose}>
          <ChevronLeft size={13} aria-hidden="true" />
          Files
        </button>
        <span class={styles.path} title={props.path}>
          {props.path}
        </span>
      </div>

      <ProblemNotice error={file.error} />
      <Show when={file.loading}>
        <p class={styles.loading}>Reading {props.path} from the machine…</p>
      </Show>
      <Show when={file()}>
        {(content) => (
          <>
            <pre class={styles.code}>
              <Show
                when={language()}
                fallback={<code class={styles.plain}>{content().text}</code>}
              >
                {(known) => (
                  // Highlighted markup rather than a text node: the library
                  // escapes the source and the result is sanitized again in
                  // `src/lib/highlight.ts` before it gets here.
                  // eslint-disable-next-line solid/no-innerhtml
                  <code class={styles.plain} innerHTML={highlightHtml(content().text, known())} />
                )}
              </Show>
            </pre>
            <p class={styles.meta}>
              {content().bytes.toLocaleString()} bytes
              <Show when={language()}>{(known) => <> · {known()}</>}</Show>
            </p>
          </>
        )}
      </Show>
    </div>
  );
}
