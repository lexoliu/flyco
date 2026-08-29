import { For, Show, createMemo, createResource, createSignal } from "solid-js";
import ProblemNotice from "../../components/ProblemNotice";
import {
  createMemoryNode,
  deleteMemoryNode,
  listMemory,
  updateMemoryNode,
  type MemoryNode,
} from "../../api/client";
import styles from "../../components/Panel.module.css";

function NewNodeForm(props: { parent: string | null; onCreated: () => void }) {
  const [title, setTitle] = createSignal("");
  const [content, setContent] = createSignal("");
  const [submitting, setSubmitting] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);

  async function onSubmit(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    setSubmitting(true);
    setError(null);
    try {
      await createMemoryNode({ title: title(), content: content(), parent: props.parent });
      setTitle("");
      setContent("");
      props.onCreated();
    } catch (err) {
      setError(err);
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <form class={styles.form} onSubmit={(event) => void onSubmit(event)}>
      <div class={styles.field}>
        <label for="memory-new-title">Title</label>
        <input id="memory-new-title" value={title()} onInput={(event) => setTitle(event.currentTarget.value)} required />
      </div>
      <div class={styles.field}>
        <label for="memory-new-content">Content</label>
        <textarea
          id="memory-new-content"
          rows="3"
          value={content()}
          onInput={(event) => setContent(event.currentTarget.value)}
        />
      </div>
      <ProblemNotice error={error()} />
      <button type="submit" class={styles.primaryButton} disabled={submitting()}>
        {submitting() ? "Adding…" : props.parent === null ? "Add root note" : "Add child note"}
      </button>
    </form>
  );
}

function NodeRow(props: { node: MemoryNode; onOpen: () => void; onChanged: () => void }) {
  const [editing, setEditing] = createSignal(false);
  const [title, setTitle] = createSignal(props.node.title);
  const [content, setContent] = createSignal(props.node.content);
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);

  async function onSave(): Promise<void> {
    setSaving(true);
    setError(null);
    try {
      await updateMemoryNode(props.node.id, { title: title(), content: content() });
      setEditing(false);
      props.onChanged();
    } catch (err) {
      setError(err);
    } finally {
      setSaving(false);
    }
  }

  async function onDelete(): Promise<void> {
    setError(null);
    try {
      await deleteMemoryNode(props.node.id);
      props.onChanged();
    } catch (err) {
      setError(err);
    }
  }

  return (
    <li class={styles.listItem} style={{ "flex-direction": "column", "align-items": "stretch" }}>
      <Show
        when={editing()}
        fallback={
          <div style={{ display: "flex", "justify-content": "space-between", "align-items": "center", gap: "var(--space-3)" }}>
            <div>
              <strong style={{ cursor: "pointer" }} onClick={props.onOpen}>
                {props.node.title}
              </strong>
              <p class={styles.itemDetail}>
                {props.node.content.length > 80 ? `${props.node.content.slice(0, 80)}…` : props.node.content}
              </p>
            </div>
            <div class={styles.itemActions}>
              <button type="button" onClick={() => setEditing(true)}>
                Edit
              </button>
              <button type="button" class={styles.dangerButton} onClick={() => void onDelete()}>
                Delete
              </button>
            </div>
          </div>
        }
      >
        <div class={styles.field}>
          <input value={title()} onInput={(event) => setTitle(event.currentTarget.value)} />
          <textarea rows="3" value={content()} onInput={(event) => setContent(event.currentTarget.value)} />
        </div>
        <ProblemNotice error={error()} />
        <div class={styles.itemActions}>
          <button type="button" onClick={() => setEditing(false)} disabled={saving()}>
            Cancel
          </button>
          <button type="button" class={styles.primaryButton} onClick={() => void onSave()} disabled={saving()}>
            {saving() ? "Saving…" : "Save"}
          </button>
        </div>
      </Show>
    </li>
  );
}

/**
 * Tree browser for the shared memory the agent reads before starting a
 * turn. Each node's full content comes back from `list_memory` itself, so
 * opening a node for editing needs no extra fetch.
 */
export default function MemoryTab() {
  const [breadcrumb, setBreadcrumb] = createSignal<MemoryNode[]>([]);
  const currentParent = createMemo(() => breadcrumb().at(-1)?.id ?? null);
  const [nodes, { refetch }] = createResource(currentParent, (parent) => listMemory({ parent: parent ?? undefined }));

  return (
    <div class={styles.tab}>
      <div class={styles.tabHeader}>
        <h2>Memory</h2>
        <p class={styles.tabDescription}>
          Notes an agent reads before it starts working, organized as a tree. Memory scoped to a
          repository only applies there; everything else applies wherever an agent runs.
        </p>
      </div>

      <nav aria-label="Memory path" style={{ display: "flex", gap: "var(--space-2)", "flex-wrap": "wrap", "font-size": "var(--text-sm)" }}>
        <button type="button" onClick={() => setBreadcrumb([])} disabled={breadcrumb().length === 0}>
          Root
        </button>
        <For each={breadcrumb()}>
          {(node, index) => (
            <>
              <span>/</span>
              <button type="button" onClick={() => setBreadcrumb(breadcrumb().slice(0, index() + 1))}>
                {node.title}
              </button>
            </>
          )}
        </For>
      </nav>

      <NewNodeForm parent={currentParent()} onCreated={() => void refetch()} />

      <ProblemNotice error={nodes.error} />
      <Show when={!nodes.loading}>
        <Show
          when={nodes.error !== undefined || (nodes() ?? []).length > 0}
          fallback={<p class={styles.empty}>Nothing here yet.</p>}
        >
          <ul class={styles.list}>
            <For each={nodes()}>
              {(node) => (
                <NodeRow
                  node={node}
                  onOpen={() => setBreadcrumb([...breadcrumb(), node])}
                  onChanged={() => void refetch()}
                />
              )}
            </For>
          </ul>
        </Show>
      </Show>
    </div>
  );
}
