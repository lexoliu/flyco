/**
 * Settings → Tools (docs/ux.md §10).
 *
 * Two kinds of card. An MCP server is a thing you switch on and off, so the
 * switch is on the card and takes effect the moment it is flipped; editing
 * expands the card in place rather than sending the user to a second
 * screen. A skill is a bundle you hand over, so the primary control is a
 * place to drop the zip.
 *
 * Both are read-only to agents by design: an agent cannot register an MCP
 * server or overwrite a skill directory, which is why this page is the only
 * place either is configured.
 */
import { For, Show, createResource, createSignal } from "solid-js";
import { Plus, Upload } from "lucide-solid";
import ProblemNotice from "../../components/ProblemNotice";
import Toggle from "../../components/Toggle";
import McpServerForm from "./McpServerForm";
import {
  deleteMcpServer,
  deleteSkill,
  listMcpServers,
  listSkills,
  updateMcpServer,
  uploadSkill,
  type McpServerConfig,
  type McpServerView,
  type SkillScope,
  type SkillView,
} from "../../api/client";
import { formatDate } from "../../lib/dates";
import { cx } from "../../lib/cx";
import styles from "./Settings.module.css";

const SCOPE_LABEL: Record<SkillScope, string> = {
  claude: "Claude Code",
  codex: "Codex",
};

function describeConfig(config: McpServerConfig): string {
  return config.transport === "stdio"
    ? `${config.command} ${config.args.join(" ")}`.trim()
    : config.url;
}

export default function ToolsSection() {
  return (
    <section class={styles.section}>
      <header class={styles.sectionHead}>
        <h2>Tools</h2>
        <p class={styles.lede}>
          Extra capabilities every session gets. Agents can use what is here but cannot change it —
          flyco provisions both from this page and nowhere else.
        </p>
      </header>
      <McpServers />
      <Skills />
    </section>
  );
}

/* ── MCP servers ──────────────────────────────────────────────────────── */

function McpServers() {
  const [servers, { refetch }] = createResource(listMcpServers);
  const [editing, setEditing] = createSignal<string | null>(null);
  const [adding, setAdding] = createSignal(false);
  const [busy, setBusy] = createSignal<string | null>(null);
  const [error, setError] = createSignal<unknown>(null);

  async function setEnabled(server: McpServerView, enabled: boolean): Promise<void> {
    setBusy(server.id);
    setError(null);
    try {
      await updateMcpServer(server.id, { name: server.name, enabled, config: server.config });
      await refetch();
    } catch (err) {
      setError(err);
    } finally {
      setBusy(null);
    }
  }

  async function remove(id: string): Promise<void> {
    setBusy(id);
    setError(null);
    try {
      await deleteMcpServer(id);
      await refetch();
    } catch (err) {
      setError(err);
    } finally {
      setBusy(null);
    }
  }

  return (
    <div class={styles.group}>
      <p class={styles.groupLabel}>MCP servers</p>
      <ProblemNotice error={servers.error ?? error()} />

      <Show when={!servers.loading}>
        <Show when={(servers() ?? []).length > 0}>
          <div class={styles.cards}>
            <For each={servers()}>
              {(server) => (
                <article class={styles.card}>
                  <div class={styles.cardTop}>
                    <div class={styles.identity}>
                      <span class={styles.cardTitle}>{server.name}</span>
                      <span class={styles.mono}>{describeConfig(server.config)}</span>
                    </div>
                    <div class={styles.actions}>
                      <Toggle
                        label={`Give sessions ${server.name}`}
                        checked={server.enabled}
                        disabled={busy() === server.id}
                        onChange={(next) => void setEnabled(server, next)}
                      />
                      <button
                        type="button"
                        class={styles.pill}
                        onClick={() => setEditing(editing() === server.id ? null : server.id)}
                      >
                        {editing() === server.id ? "Close" : "Edit"}
                      </button>
                      <button
                        type="button"
                        class={styles.pillDanger}
                        disabled={busy() === server.id}
                        onClick={() => void remove(server.id)}
                      >
                        Remove
                      </button>
                    </div>
                  </div>
                  <Show when={editing() === server.id}>
                    <McpServerForm
                      server={server}
                      onSaved={() => {
                        setEditing(null);
                        void refetch();
                      }}
                      onCancel={() => setEditing(null)}
                    />
                  </Show>
                </article>
              )}
            </For>
          </div>
        </Show>

        <Show
          when={adding()}
          fallback={
            <Show
              when={(servers() ?? []).length > 0}
              fallback={
                <div class={styles.empty}>
                  <p class={styles.emptyLine}>
                    No MCP servers yet. Add one and every session gets its tools.
                  </p>
                  <button type="button" class={styles.pillPrimary} onClick={() => setAdding(true)}>
                    <Plus size={14} aria-hidden="true" />
                    Add server
                  </button>
                </div>
              }
            >
              <div>
                <button type="button" class={styles.pill} onClick={() => setAdding(true)}>
                  <Plus size={14} aria-hidden="true" />
                  Add server
                </button>
              </div>
            </Show>
          }
        >
          <article class={styles.card}>
            <McpServerForm
              onSaved={() => {
                setAdding(false);
                void refetch();
              }}
              onCancel={() => setAdding(false)}
            />
          </article>
        </Show>
      </Show>
    </div>
  );
}

/* ── Skills ───────────────────────────────────────────────────────────── */

function Skills() {
  const [skills, { refetch }] = createResource(listSkills);
  const [error, setError] = createSignal<unknown>(null);

  async function remove(id: string): Promise<void> {
    setError(null);
    try {
      await deleteSkill(id);
      await refetch();
    } catch (err) {
      setError(err);
    }
  }

  return (
    <div class={styles.group}>
      <p class={styles.groupLabel}>Skills</p>
      <ProblemNotice error={skills.error ?? error()} />

      <Show when={(skills() ?? []).length > 0}>
        <div class={cx(styles.cards, styles.cardsPaired)}>
          <For each={skills()}>
            {(skill) => <SkillCard skill={skill} onRemove={() => void remove(skill.id)} />}
          </For>
        </div>
      </Show>

      <SkillDropZone onUploaded={() => void refetch()} />
    </div>
  );
}

function SkillCard(props: { skill: SkillView; onRemove: () => void }) {
  return (
    <article class={styles.card}>
      <div class={styles.cardTop}>
        <div class={styles.identity}>
          <span class={styles.cardTitle}>{props.skill.name}</span>
          <span class={styles.cardMeta}>
            {SCOPE_LABEL[props.skill.scope]} · updated {formatDate(props.skill.uploaded_at_unix)}
          </span>
        </div>
        <div class={styles.actions}>
          <button type="button" class={styles.pillDanger} onClick={props.onRemove}>
            Remove
          </button>
        </div>
      </div>
    </article>
  );
}

/** The `.zip` a skill arrives as, with the directory name taken from the file. */
function SkillDropZone(props: { onUploaded: () => void }) {
  const [scope, setScope] = createSignal<SkillScope>("claude");
  const [over, setOver] = createSignal(false);
  const [uploading, setUploading] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  let picker: HTMLInputElement | undefined;

  /**
   * The bundle's own name is the directory name: asking for it twice, once
   * as a filename and once as a field, is asking the user to repeat
   * themselves.
   */
  function directoryOf(fileName: string): string {
    return fileName.replace(/\.zip$/i, "");
  }

  async function upload(file: File): Promise<void> {
    if (!file.name.toLowerCase().endsWith(".zip")) {
      setError(new Error(`A skill is a .zip bundle; "${file.name}" is not one.`));
      return;
    }
    setUploading(true);
    setError(null);
    try {
      await uploadSkill(directoryOf(file.name), scope(), file);
      props.onUploaded();
    } catch (err) {
      setError(err);
    } finally {
      setUploading(false);
    }
  }

  function onDrop(event: DragEvent): void {
    event.preventDefault();
    setOver(false);
    const file = event.dataTransfer?.files.item(0);
    if (file !== null && file !== undefined) {
      void upload(file);
    }
  }

  return (
    <div
      class={cx(styles.dropZone, over() && styles.dropZoneActive)}
      onDragOver={(event) => {
        event.preventDefault();
        setOver(true);
      }}
      onDragLeave={() => setOver(false)}
      onDrop={onDrop}
    >
      <Upload size={18} aria-hidden="true" />
      <p class={styles.emptyLine}>
        Drop a skill's <code>.zip</code> here. Its file name becomes the directory name.
      </p>
      <div class={styles.segmented} role="group" aria-label="Which harness gets the skill">
        <For each={Object.entries(SCOPE_LABEL) as [SkillScope, string][]}>
          {([value, label]) => (
            <button
              type="button"
              class={cx(styles.segment, scope() === value && styles.segmentOn)}
              aria-pressed={scope() === value}
              onClick={() => setScope(value)}
            >
              {label}
            </button>
          )}
        </For>
      </div>
      <input
        ref={picker}
        type="file"
        accept=".zip"
        class="visually-hidden"
        aria-label="Skill bundle"
        onChange={(event) => {
          const file = event.currentTarget.files?.item(0);
          if (file !== null && file !== undefined) {
            void upload(file);
          }
          event.currentTarget.value = "";
        }}
      />
      <button
        type="button"
        class={styles.pill}
        disabled={uploading()}
        onClick={() => picker?.click()}
      >
        {uploading() ? "Uploading…" : "Choose a file"}
      </button>
      <ProblemNotice error={error()} />
    </div>
  );
}
