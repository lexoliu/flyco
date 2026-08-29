import { For, Show, createResource, createSignal } from "solid-js";
import ProblemNotice from "../../components/ProblemNotice";
import { deleteSkill, listSkills, uploadSkill, type SkillScope } from "../../api/client";
import styles from "../../components/Panel.module.css";

function formatBytes(bytes: number): string {
  if (bytes < 1024) {
    return `${bytes} B`;
  }
  const kib = bytes / 1024;
  if (kib < 1024) {
    return `${kib.toFixed(1)} KiB`;
  }
  return `${(kib / 1024).toFixed(1)} MiB`;
}

export default function SkillsTab() {
  const [skills, { refetch }] = createResource(listSkills);
  const [name, setName] = createSignal("");
  const [scope, setScope] = createSignal<SkillScope>("claude");
  const [file, setFile] = createSignal<File | null>(null);
  const [uploading, setUploading] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);

  async function onUpload(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    const bundle = file();
    if (bundle === null || name().trim() === "") {
      setError(new Error("Pick a name and a zip file before uploading."));
      return;
    }
    setUploading(true);
    setError(null);
    try {
      await uploadSkill(name(), scope(), bundle);
      setName("");
      setFile(null);
      await refetch();
    } catch (err) {
      setError(err);
    } finally {
      setUploading(false);
    }
  }

  async function onDelete(id: string): Promise<void> {
    setError(null);
    try {
      await deleteSkill(id);
      await refetch();
    } catch (err) {
      setError(err);
    }
  }

  return (
    <div class={styles.tab}>
      <div class={styles.tabHeader}>
        <h2>Skills</h2>
        <p class={styles.tabDescription}>
          Global skills are read-only to agents. To change one, an agent uploads a zip through
          its MCP tool instead of editing files directly, and the update reaches every session
          immediately. Claude Code and Codex keep separate skill directories.
        </p>
      </div>

      <form class={styles.form} onSubmit={(event) => void onUpload(event)}>
        <div class={styles.field}>
          <label for="skill-name">Directory name</label>
          <input id="skill-name" value={name()} onInput={(event) => setName(event.currentTarget.value)} required />
        </div>
        <div class={styles.field}>
          <label for="skill-scope">Harness</label>
          <select id="skill-scope" value={scope()} onChange={(event) => setScope(event.currentTarget.value as SkillScope)}>
            <option value="claude">Claude Code</option>
            <option value="codex">Codex</option>
          </select>
        </div>
        <div class={styles.field}>
          <label for="skill-file">Zip bundle</label>
          <input
            id="skill-file"
            type="file"
            accept=".zip"
            onChange={(event) => setFile(event.currentTarget.files?.item(0) ?? null)}
            required
          />
        </div>
        <ProblemNotice error={error()} />
        <button type="submit" class={styles.primaryButton} disabled={uploading()}>
          {uploading() ? "Uploading…" : "Upload skill"}
        </button>
      </form>

      <ProblemNotice error={skills.error} />
      <Show when={!skills.loading}>
        <Show
          when={skills.error !== undefined || (skills() ?? []).length > 0}
          fallback={<p class={styles.empty}>No skills uploaded yet.</p>}
        >
          <ul class={styles.list}>
            <For each={skills()}>
              {(skill) => (
                <li class={styles.listItem}>
                  <div>
                    <strong>{skill.name}</strong>
                    <p class={styles.itemDetail}>
                      {skill.scope === "claude" ? "Claude Code" : "Codex"} · {formatBytes(skill.size_bytes)}
                    </p>
                  </div>
                  <div class={styles.itemActions}>
                    <button type="button" class={styles.dangerButton} onClick={() => void onDelete(skill.id)}>
                      Delete
                    </button>
                  </div>
                </li>
              )}
            </For>
          </ul>
        </Show>
      </Show>
    </div>
  );
}
