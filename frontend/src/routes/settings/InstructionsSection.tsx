/**
 * Settings → Instructions (docs/ux.md §10).
 *
 * Three things the agent reads, in the order it reads them: the shared
 * `AGENTS.md`, the changes an agent has asked to make to it, and the memory
 * tree.
 *
 * The change requests are the reason this section is not just a textarea.
 * An agent cannot edit `AGENTS.md` itself — it raises an approval carrying
 * a find/replace — and approving one blind would defeat the point of the
 * approval. So each pending request is rendered as a diff of the document
 * the user has against the document they would get.
 */
import { For, Show, createMemo, createSignal } from "solid-js";
import { createQuery } from "../../lib/query";
import ProblemNotice from "../../components/ProblemNotice";
import DiffView from "../../components/DiffView";
import MemoryOutliner from "./MemoryOutliner";
import {
  decideApproval,
  getAgentsMd,
  listApprovals,
  putAgentsMd,
  type ApprovalView,
} from "../../api/client";
import { applyFindReplace, diffRows } from "../../lib/diffRows";
import { relativeTime } from "../../lib/relativeTime";
import styles from "./Settings.module.css";

export default function InstructionsSection() {
  const [doc, { refetch: refetchDoc }] = createQuery(getAgentsMd);
  const [draft, setDraft] = createSignal("");
  const [loadedAt, setLoadedAt] = createSignal<number | null>(null);
  const [saving, setSaving] = createSignal(false);
  const [saveError, setSaveError] = createSignal<unknown>(null);

  // The editor takes the server's content whenever a newer version arrives —
  // on first load, and after an approval rewrites the document underneath it.
  createMemo(() => {
    const current = doc();
    if (current !== undefined && loadedAt() !== current.updated_at_unix) {
      setDraft(current.content);
      setLoadedAt(current.updated_at_unix);
    }
  });

  const dirty = () => doc() !== undefined && draft() !== doc()?.content;

  async function save(): Promise<void> {
    setSaving(true);
    setSaveError(null);
    try {
      const saved = await putAgentsMd(draft());
      setLoadedAt(saved.updated_at_unix);
      await refetchDoc();
    } catch (err) {
      setSaveError(err);
    } finally {
      setSaving(false);
    }
  }

  return (
    <section class={styles.section}>
      <header class={styles.sectionHead}>
        <h2>Instructions</h2>
        <p class={styles.lede}>
          What every agent reads before it starts: one shared AGENTS.md, provisioned onto each
          session's machine alongside whatever the repository carries, and a tree of notes.
        </p>
      </header>

      <div class={styles.group}>
        <p class={styles.groupLabel}>AGENTS.md</p>
        <ProblemNotice error={doc.error} />
        <Show when={!doc.loading}>
          <article class={styles.card}>
            <div class={styles.field}>
              {/* The group above already says AGENTS.md; a visible "Content"
                  label repeats the box's own shape and says nothing. What an
                  empty box does need is an example of what belongs in it. */}
              <label for="agents-md-content" class="visually-hidden">
                AGENTS.md
              </label>
              <textarea
                id="agents-md-content"
                rows="12"
                placeholder={"Always run `cargo fmt` before committing.\nThe staging database is read-only."}
                value={draft()}
                onInput={(event) => setDraft(event.currentTarget.value)}
              />
            </div>
            <ProblemNotice error={saveError()} />
            <div class={styles.formActions}>
              <button
                type="button"
                class={styles.pillPrimary}
                disabled={saving() || !dirty()}
                onClick={() => void save()}
              >
                {saving() ? "Saving…" : "Save"}
              </button>
              <span class={styles.note} role="status">
                <Show
                  when={dirty()}
                  fallback={
                    <Show when={loadedAt()} fallback="">
                      {(at) => `Saved ${relativeTime(at(), Date.now())}`}
                    </Show>
                  }
                >
                  Unsaved changes
                </Show>
              </span>
            </div>
          </article>
        </Show>
      </div>

      <AgentsMdApprovals content={doc()?.content} onApplied={() => void refetchDoc()} />

      <MemoryOutliner />
    </section>
  );
}

/** An `agents_md_change` approval, as a diff the user can actually judge. */
function AgentsMdApprovals(props: { content: string | undefined; onApplied: () => void }) {
  const [approvals, { refetch }] = createQuery(() => listApprovals({ state: "pending" }));
  const [busy, setBusy] = createSignal<string | null>(null);
  const [error, setError] = createSignal<unknown>(null);

  const changes = createMemo(() =>
    (approvals() ?? []).filter(
      (approval): approval is ApprovalView & { payload: { kind: "agents_md_change" } } =>
        approval.payload.kind === "agents_md_change",
    ),
  );

  async function decide(id: string, decision: "approved" | "denied"): Promise<void> {
    setBusy(id);
    setError(null);
    try {
      await decideApproval(id, decision);
      await refetch();
      if (decision === "approved") {
        props.onApplied();
      }
    } catch (err) {
      setError(err);
    } finally {
      setBusy(null);
    }
  }

  return (
    // The block is the pending changes, plus the reason there are none on
    // screen when asking for them failed: gating it on the list alone would
    // hide the notice inside it in exactly the case it exists for.
    <Show when={changes().length > 0 || approvals.error !== undefined}>
      <div class={styles.group}>
        <p class={styles.groupLabel}>Changes agents have asked for</p>
        <ProblemNotice error={approvals.error ?? error()} />
        <div class={styles.cards}>
          <For each={changes()}>
            {(approval) => {
              const payload = approval.payload;
              const before = () => props.content ?? "";
              const after = () => applyFindReplace(before(), payload.find, payload.replace);
              return (
                <article class={styles.card}>
                  <div class={styles.cardTop}>
                    <div class={styles.identity}>
                      <span class={styles.cardTitle}>Edit AGENTS.md</span>
                      <span class={styles.cardMeta}>
                        Asked {relativeTime(approval.created_at_unix, Date.now())}
                      </span>
                    </div>
                    <div class={styles.actions}>
                      <button
                        type="button"
                        class={styles.pillPrimary}
                        disabled={busy() === approval.id}
                        onClick={() => void decide(approval.id, "approved")}
                      >
                        Accept
                      </button>
                      <button
                        type="button"
                        class={styles.pillDanger}
                        disabled={busy() === approval.id}
                        onClick={() => void decide(approval.id, "denied")}
                      >
                        Reject
                      </button>
                    </div>
                  </div>
                  <Show
                    when={after()}
                    fallback={
                      /* The document moved on since the agent read it, so
                         there is no diff to show — only a decision to make. */
                      <p class={styles.note}>
                        The text this change replaces is no longer in AGENTS.md, so accepting it
                        would do nothing.
                      </p>
                    }
                  >
                    {(proposed) => (
                      <DiffView
                        label="Proposed change to AGENTS.md"
                        rows={diffRows(before(), proposed())}
                      />
                    )}
                  </Show>
                </article>
              );
            }}
          </For>
        </div>
      </div>
    </Show>
  );
}
