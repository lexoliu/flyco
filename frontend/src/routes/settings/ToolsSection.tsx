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
import { For, Show, createSignal } from "solid-js";
import { A } from "@solidjs/router";
import { createQuery } from "../../lib/query";
import { LibraryBig, Plus, Upload } from "lucide-solid";
import ProblemNotice from "../../components/ProblemNotice";
import Skeleton from "../../components/Skeleton";
import Toggle from "../../components/Toggle";
import Modal from "../../components/Modal";
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
  type SkillView,
} from "../../api/client";
import { formatDate } from "../../lib/dates";
import { cx } from "../../lib/cx";
import styles from "./Settings.module.css";

/** Where `Add from catalog` goes: the picker over the official registry. */
const MCP_CATALOG = "/settings/tools/mcp-catalog";

/** Where `Add from a marketplace` goes: the picker over plugin marketplaces. */
const SKILL_CATALOG = "/settings/tools/skill-catalog";

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
      </header>
      <McpServers />
      <Skills />
    </section>
  );
}

/* ── MCP servers ──────────────────────────────────────────────────────── */

function McpServers() {
  const [servers, { refetch }] = createQuery(listMcpServers);
  const listed = (): McpServerView[] => servers() ?? [];
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

      {/* The rows the first read is bringing, rather than an empty state
          that contradicts itself a moment later. A refetch keeps what is
          on screen: the list is still true while it is being re-read. */}
      <Show when={servers.settled} fallback={<Skeleton lines={3} />}>
        <Show when={listed().length > 0}>
          <div class={styles.cards}>
            <For each={listed()}>
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
                        onClick={() => setEditing(server.id)}
                      >
                        Edit
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

                </article>
              )}
            </For>
          </div>
        </Show>

        <Show
          when={listed().length > 0}
          fallback={
            <div class={styles.empty}>
              {/* The catalog is the primary: a browser-only user picks a
                  server from a list; typing a transport by hand is the
                  way in for one the registry does not list. */}
              <div class={styles.actions}>
                <A href={MCP_CATALOG} class={styles.pillPrimary}>
                  <LibraryBig size={14} aria-hidden="true" />
                  Add from catalog
                </A>
                <button type="button" class={styles.pill} onClick={() => setAdding(true)}>
                  <Plus size={14} aria-hidden="true" />
                  Add server
                </button>
              </div>
            </div>
          }
        >
          <div class={styles.actions}>
            <A href={MCP_CATALOG} class={styles.pill}>
              <LibraryBig size={14} aria-hidden="true" />
              Add from catalog
            </A>
            <button type="button" class={styles.pill} onClick={() => setAdding(true)}>
              <Plus size={14} aria-hidden="true" />
              Add server
            </button>
          </div>
        </Show>
      </Show>

      {/* One form, in a dialog, for both adding and editing: it carries a
          transport, a command line and a set of headers, and growing that
          out of the row the user clicked moved every row below it. */}
      <Show when={adding()}>
        <Modal title="Add MCP server" onClose={() => setAdding(false)}>
          <McpServerForm
            onSaved={() => {
              setAdding(false);
              void refetch();
            }}
            onCancel={() => setAdding(false)}
          />
        </Modal>
      </Show>
      <Show when={listed().find((server) => server.id === editing())}>
        {(server) => (
          <Modal title={`Edit ${server().name}`} onClose={() => setEditing(null)}>
            <McpServerForm
              server={server()}
              onSaved={() => {
                setEditing(null);
                void refetch();
              }}
              onCancel={() => setEditing(null)}
            />
          </Modal>
        )}
      </Show>
    </div>
  );
}

/* ── Skills ───────────────────────────────────────────────────────────── */

function Skills() {
  const [skills, { refetch }] = createQuery(listSkills);
  const uploaded = (): SkillView[] => skills() ?? [];
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

      <Show when={uploaded().length > 0}>
        <div class={cx(styles.cards, styles.cardsPaired)}>
          <For each={uploaded()}>
            {(skill) => <SkillCard skill={skill} onRemove={() => void remove(skill.id)} />}
          </For>
        </div>
      </Show>

      {/* A browser-only user picks a skill from a marketplace; the .zip is
          the way in for one nobody publishes. */}
      <div class={styles.actions}>
        <A
          href={SKILL_CATALOG}
          class={uploaded().length > 0 ? styles.pill : styles.pillPrimary}
        >
          <LibraryBig size={14} aria-hidden="true" />
          Add from a marketplace
        </A>
      </div>

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
            updated {formatDate(props.skill.uploaded_at_unix)}
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
      await uploadSkill(directoryOf(file.name), file);
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
