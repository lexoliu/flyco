/**
 * Google Cloud's pages (docs/ux.md §4 C5″–C6″, §7.4): the commands, then
 * the file they produce.
 *
 * A Google credential is a *document* rather than a set of fields, so the
 * second page is a drop zone rather than a form: the user already has the
 * file, and retyping any part of it would only be a chance to get it
 * wrong. What the page reads out of it is `project_id` and `client_email`,
 * which is how somebody with three projects tells which key they dropped.
 */
import { Show, createMemo, createSignal } from "solid-js";
import { FileJson, Upload } from "lucide-solid";
import CommandBlock from "../../CommandBlock";
import { useReadiness } from "../../Readiness";
import { linkProvider } from "../../../api/client";
import { parseGcpServiceAccount } from "../../../lib/gcpCredentials";
import { NEXT, type PageComponent, type Primary } from "../page";
import { ConfirmRow } from "./shared";
import styles from "./pages.module.css";

/** The commands that create the account and download its key. */
const CREATE_ACCOUNT = `gcloud iam service-accounts create flyco --display-name flyco

gcloud projects add-iam-policy-binding $(gcloud config get-value project) \\
  --member serviceAccount:flyco@$(gcloud config get-value project).iam.gserviceaccount.com \\
  --role roles/compute.admin

gcloud iam service-accounts keys create flyco-key.json \\
  --iam-account flyco@$(gcloud config get-value project).iam.gserviceaccount.com`;

export const GcpCommands: PageComponent<{ id: "gcp-commands" }> = (props) => ({
  title: "Create a service account",
  body: (
    <>
      <CommandBlock value={CREATE_ACCOUNT} label="Copy" />
      <p class={styles.hint}>
        Run these where the <code>gcloud</code> CLI is signed in to the project you want sessions
        to run in. The last one writes <code>flyco-key.json</code> into the current directory.
      </p>
    </>
  ),
  primary: () => NEXT(() => props.advance()),
});

export const GcpKeyFile: PageComponent<{ id: "gcp-key-file" }> = (props) => {
  const readiness = useReadiness();
  const [contents, setContents] = createSignal("");
  const [filename, setFilename] = createSignal("");
  const [dragging, setDragging] = createSignal(false);
  const [readError, setReadError] = createSignal<string | null>(null);

  const parsed = createMemo(() =>
    contents().trim() === "" ? null : parseGcpServiceAccount(contents()),
  );
  const account = () => {
    const result = parsed();
    return result !== null && result.ok ? result.account : null;
  };
  const parseError = () => {
    const result = parsed();
    return result !== null && !result.ok ? result.error : null;
  };

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

  const primary = (): Primary => {
    const found = account();
    return {
      label: "Link Google Cloud",
      busy: "Linking…",
      disabled: found === null ? "Drop the key file to continue" : null,
      onClick: async () => {
        if (found === null) {
          return;
        }
        const linked = await linkProvider({
          label: found.projectId,
          credentials: { kind: "gcp", service_account_json: contents() },
        });
        await readiness.refresh();
        props.advance({ computeAccount: linked });
      },
    };
  };

  return {
    title: "Drop the key file",
    body: (
      <>
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
            aria-label="Service account key file"
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
              <ConfirmRow label="Project" value={found().projectId} />
              <ConfirmRow label="Service account" value={found().clientEmail} />
            </dl>
          )}
        </Show>
      </>
    ),
    primary,
  };
};
