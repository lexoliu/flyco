/**
 * Memory, as an outliner.
 *
 * The notes an agent reads before it starts working are a tree, and the
 * breadcrumb browser this replaces made the reader navigate *into* a note
 * to see what was under it — which meant the shape of their own memory was
 * never on screen at once. An outliner shows the shape: everything opened
 * stays open, a child sits under its parent, and adding one does not move
 * the page.
 *
 * `GET /v1/memory?parent=` answers one level per request, so a level is
 * fetched the first time it is opened and kept afterwards; the arithmetic
 * of which rows that produces is `src/lib/memoryTree.ts`.
 */
import { For, Show, createSignal, onMount } from "solid-js";
import { ChevronRight, Plus, Trash2 } from "lucide-solid";
import ProblemNotice from "../../components/ProblemNotice";
import {
  createMemoryNode,
  deleteMemoryNode,
  listMemory,
  updateMemoryNode,
  type MemoryNode,
} from "../../api/client";
import {
  ROOT_KEY,
  flattenTree,
  forgetSubtree,
  replaceNode,
  type LoadedChildren,
} from "../../lib/memoryTree";
import { cx } from "../../lib/cx";
import styles from "./Settings.module.css";

/** How far one level of nesting indents a row, in pixels. */
const INDENT = 18;

export default function MemoryOutliner() {
  const [children, setChildren] = createSignal<LoadedChildren>({});
  const [expanded, setExpanded] = createSignal<ReadonlySet<string>>(new Set());
  const [editing, setEditing] = createSignal<string | null>(null);
  const [composingUnder, setComposingUnder] = createSignal<string | null>(null);
  const [loaded, setLoaded] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);

  async function load(parent: string): Promise<void> {
    setError(null);
    try {
      const nodes = await listMemory(parent === ROOT_KEY ? undefined : { parent });
      setChildren({ ...children(), [parent]: nodes });
    } catch (err) {
      setError(err);
    }
  }

  onMount(() => {
    void load(ROOT_KEY).finally(() => setLoaded(true));
  });

  async function toggle(node: MemoryNode): Promise<void> {
    const open = new Set(expanded());
    if (open.has(node.id)) {
      open.delete(node.id);
      setExpanded(open);
      return;
    }
    open.add(node.id);
    setExpanded(open);
    if (children()[node.id] === undefined) {
      await load(node.id);
    }
  }

  async function save(node: MemoryNode, title: string, content: string): Promise<void> {
    setError(null);
    try {
      const updated = await updateMemoryNode(node.id, { title, content });
      setChildren(replaceNode(children(), updated));
      setEditing(null);
    } catch (err) {
      setError(err);
    }
  }

  async function remove(node: MemoryNode): Promise<void> {
    setError(null);
    try {
      await deleteMemoryNode(node.id);
      setChildren(forgetSubtree(children(), node.id));
    } catch (err) {
      setError(err);
    }
  }

  async function create(parent: string, title: string): Promise<void> {
    setError(null);
    try {
      await createMemoryNode({
        title,
        content: "",
        parent: parent === ROOT_KEY ? null : parent,
      });
      setComposingUnder(null);
      // The parent is reloaded rather than patched in place so the new node
      // arrives with the id and timestamp the server actually assigned.
      if (parent !== ROOT_KEY) {
        setExpanded(new Set(expanded()).add(parent));
      }
      await load(parent);
    } catch (err) {
      setError(err);
    }
  }

  const rows = () => flattenTree(children(), expanded());

  return (
    <div class={styles.group}>
      <p class={styles.groupLabel}>Memory</p>
      <ProblemNotice error={error()} />

      <Show when={loaded()}>
        <Show
          when={rows().length > 0 || composingUnder() === ROOT_KEY}
          fallback={
            <div class={styles.empty}>
              <p class={styles.emptyLine}>
                Nothing is remembered yet. Add a note and every agent reads it before it starts.
              </p>
              <button
                type="button"
                class={styles.pillPrimary}
                onClick={() => setComposingUnder(ROOT_KEY)}
              >
                <Plus size={14} aria-hidden="true" />
                Add note
              </button>
            </div>
          }
        >
          <ul class={styles.tree}>
            <For each={rows()}>
              {(row) => (
                <>
                  <li class={styles.treeRow} style={{ "padding-left": `${row.depth * INDENT + 12}px` }}>
                    <button
                      type="button"
                      class={cx(styles.twisty, row.expanded && styles.twistyOpen)}
                      aria-expanded={row.expanded}
                      aria-label={`${row.expanded ? "Collapse" : "Expand"} ${row.node.title}`}
                      onClick={() => void toggle(row.node)}
                    >
                      <ChevronRight size={14} aria-hidden="true" />
                    </button>
                    <Show
                      when={editing() === row.node.id}
                      fallback={
                        <>
                          <span class={styles.treeTitle}>{row.node.title}</span>
                          <Show when={row.node.content !== ""}>
                            <span class={styles.treeSummary}>{row.node.content}</span>
                          </Show>
                          <div class={styles.treeActions}>
                            <button
                              type="button"
                              class={styles.pill}
                              onClick={() => setEditing(row.node.id)}
                            >
                              Edit
                            </button>
                            <button
                              type="button"
                              class={styles.iconButton}
                              aria-label={`Add a note under ${row.node.title}`}
                              onClick={() => setComposingUnder(row.node.id)}
                            >
                              <Plus size={14} aria-hidden="true" />
                            </button>
                            <button
                              type="button"
                              class={cx(styles.iconButton, styles.iconButtonDanger)}
                              aria-label={`Delete ${row.node.title}`}
                              onClick={() => void remove(row.node)}
                            >
                              <Trash2 size={14} aria-hidden="true" />
                            </button>
                          </div>
                        </>
                      }
                    >
                      <NodeEditor
                        node={row.node}
                        onSave={(title, content) => void save(row.node, title, content)}
                        onCancel={() => setEditing(null)}
                      />
                    </Show>
                  </li>
                  <Show when={composingUnder() === row.node.id}>
                    <li
                      class={styles.treeRow}
                      style={{ "padding-left": `${(row.depth + 1) * INDENT + 12}px` }}
                    >
                      <NewNodeInput
                        label={`New note under ${row.node.title}`}
                        onCreate={(title) => void create(row.node.id, title)}
                        onCancel={() => setComposingUnder(null)}
                      />
                    </li>
                  </Show>
                </>
              )}
            </For>
            <Show when={composingUnder() === ROOT_KEY}>
              <li class={styles.treeRow} style={{ "padding-left": "12px" }}>
                <NewNodeInput
                  label="New note"
                  onCreate={(title) => void create(ROOT_KEY, title)}
                  onCancel={() => setComposingUnder(null)}
                />
              </li>
            </Show>
          </ul>
          <div>
            <button
              type="button"
              class={styles.pill}
              onClick={() => setComposingUnder(ROOT_KEY)}
            >
              <Plus size={14} aria-hidden="true" />
              Add note
            </button>
          </div>
        </Show>
      </Show>
    </div>
  );
}

/** Renaming a note, and editing what it says, without leaving the row. */
function NodeEditor(props: {
  node: MemoryNode;
  onSave: (title: string, content: string) => void;
  onCancel: () => void;
}) {
  const [title, setTitle] = createSignal(props.node.title);
  const [content, setContent] = createSignal(props.node.content);

  return (
    <form
      class={styles.form}
      style={{ flex: "1", "min-width": "0" }}
      onSubmit={(event) => {
        event.preventDefault();
        props.onSave(title(), content());
      }}
    >
      <div class={styles.field}>
        <label for={`memory-title-${props.node.id}`}>Title</label>
        <input
          id={`memory-title-${props.node.id}`}
          value={title()}
          onInput={(event) => setTitle(event.currentTarget.value)}
          required
        />
      </div>
      <div class={styles.field}>
        <label for={`memory-content-${props.node.id}`}>Note</label>
        <textarea
          id={`memory-content-${props.node.id}`}
          rows="3"
          value={content()}
          onInput={(event) => setContent(event.currentTarget.value)}
        />
      </div>
      <div class={styles.formActions}>
        <button type="submit" class={styles.pillPrimary}>
          Save
        </button>
        <button type="button" class={styles.pill} onClick={props.onCancel}>
          Cancel
        </button>
      </div>
    </form>
  );
}

/** One input that becomes a note: type a title, press Enter. */
function NewNodeInput(props: {
  label: string;
  onCreate: (title: string) => void;
  onCancel: () => void;
}) {
  const [title, setTitle] = createSignal("");

  return (
    <form
      style={{ display: "flex", gap: "var(--space-2)", flex: "1", "min-width": "0" }}
      onSubmit={(event) => {
        event.preventDefault();
        if (title().trim() !== "") {
          props.onCreate(title().trim());
        }
      }}
    >
      <input
        class={styles.renameInput}
        aria-label={props.label}
        placeholder="Title"
        value={title()}
        onInput={(event) => setTitle(event.currentTarget.value)}
        onKeyDown={(event) => {
          if (event.key === "Escape") {
            props.onCancel();
          }
        }}
        // The row appeared because the user asked to type in it.
        autofocus
      />
      <button type="submit" class={styles.pillPrimary}>
        Add
      </button>
      <button type="button" class={styles.pill} onClick={props.onCancel}>
        Cancel
      </button>
    </form>
  );
}
