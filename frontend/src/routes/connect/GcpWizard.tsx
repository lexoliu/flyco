/**
 * Linking Google Cloud: two commands, then the file they produce
 * (docs/ux.md §7.4).
 *
 * A Google credential is a *document* rather than a set of fields, so the
 * step is a drop zone rather than a form: the user already has the file, and
 * retyping any part of it would only be a chance to get it wrong. What the
 * page reads out of it is `project_id` and `client_email`, which is how
 * somebody with three projects tells which key they just dropped.
 */
import { Show, createMemo, createSignal } from "solid-js";
import { FileJson, Upload } from "lucide-solid";
import CommandBlock from "../../components/CommandBlock";
import ProblemNotice from "../../components/ProblemNotice";
import type { ProviderCredentials } from "../../api/client";
import { parseGcpServiceAccount } from "../../lib/gcpCredentials";
import styles from "./Connect.module.css";

/** The two commands that create the account and download its key. */
const CREATE_ACCOUNT = `gcloud iam service-accounts create flyco --display-name flyco

gcloud projects add-iam-policy-binding $(gcloud config get-value project) \\
  --member serviceAccount:flyco@$(gcloud config get-value project).iam.gserviceaccount.com \\
  --role roles/compute.admin

gcloud iam service-accounts keys create flyco-key.json \\
  --iam-account flyco@$(gcloud config get-value project).iam.gserviceaccount.com`;

export interface GcpWizardProps {
  onLink: (credentials: ProviderCredentials, label: string) => Promise<void>;
  linking: boolean;
  error: unknown;
}

export default function GcpWizard(props: GcpWizardProps) {
  const [contents, setContents] = createSignal("");
  const [filename, setFilename] = createSignal("");
  const [dragging, setDragging] = createSignal(false);
  const [readError, setReadError] = createSignal<string | null>(null);

  const parsed = createMemo(() =>
    contents().trim() === "" ? null : parseGcpServiceAccount(contents()),
  );
  const account = createMemo(() => {
    const result = parsed();
    return result !== null && result.ok ? result.account : null;
  });
  const parseError = createMemo(() => {
    const result = parsed();
    return result !== null && !result.ok ? result.error : null;
  });

  async function accept(file: File | undefined): Promise<void> {
    setReadError(null);
    if (file === undefined) {
      return;
    }
    if (!file.name.endsWith(".json")) {
      setReadError("That is not a .json file. Drop the key the last command wrote.");
      return;
    }
    setFilename(file.name);
    setContents(await file.text());
  }

  async function link(): Promise<void> {
    if (account() === null) {
      return;
    }
    await props.onLink(
      { kind: "gcp", service_account_json: contents() },
      account()?.projectId ?? "Google Cloud",
    );
  }

  return (
    <div class={styles.step}>
      <section class={styles.stage}>
        <p class={styles.stageTitle}>1 · Create a service account and its key</p>
        <CommandBlock value={CREATE_ACCOUNT} label="Copy commands" />
        <p class={styles.hint}>
          Run these where the `gcloud` CLI is signed in to the project you want sessions to run in.
          The last one writes `flyco-key.json` into the current directory.
        </p>
      </section>

      <section class={styles.stage}>
        <p class={styles.stageTitle}>2 · Drop the key file here</p>
        <label
          class={dragging() ? `${styles.drop} ${styles.dropActive}` : styles.drop}
          onDragOver={(event) => {
            event.preventDefault();
            setDragging(true);
          }}
          onDragLeave={() => setDragging(false)}
          onDrop={(event) => {
            event.preventDefault();
            setDragging(false);
            void accept(event.dataTransfer?.files[0]);
          }}
        >
          <input
            type="file"
            accept="application/json,.json"
            class={styles.fileInput}
            onChange={(event) => void accept(event.currentTarget.files?.[0])}
          />
          <Upload size={18} aria-hidden="true" />
          <span>
            <Show when={filename()} fallback="Drop flyco-key.json, or choose a file">
              {(name) => (
                <>
                  <FileJson size={13} aria-hidden="true" /> {name()}
                </>
              )}
            </Show>
          </span>
        </label>

        <Show when={readError()}>{(message) => <p class={styles.error}>{message()}</p>}</Show>
        <Show when={parseError()}>{(message) => <p class={styles.error}>{message()}</p>}</Show>

        <Show when={account()}>
          {(found) => (
            <dl class={styles.confirm}>
              <div class={styles.confirmRow}>
                <dt>Project</dt>
                <dd>{found().projectId}</dd>
              </div>
              <div class={styles.confirmRow}>
                <dt>Service account</dt>
                <dd>{found().clientEmail}</dd>
              </div>
            </dl>
          )}
        </Show>
      </section>

      <ProblemNotice error={props.error} />
      <button
        type="button"
        class={styles.primary}
        disabled={account() === null || props.linking}
        onClick={() => void link()}
      >
        {props.linking ? "Linking…" : "Link Google Cloud"}
      </button>
    </div>
  );
}
