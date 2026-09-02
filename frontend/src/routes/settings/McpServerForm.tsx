/**
 * The one form that describes an MCP server, used to add and to edit.
 *
 * `PATCH /v1/mcp-servers/{id}` takes the whole document rather than a diff,
 * so editing and adding differ only in which call the submit makes and what
 * the fields start out as — which is exactly one component, not two.
 *
 * Field ids come from `createUniqueId` because more than one of these can
 * be open at once (an `Add server` form below a card being edited), and two
 * `id="mcp-name"` on a page silently breaks every label.
 */
import { Show, createSignal, createUniqueId } from "solid-js";
import ProblemNotice from "../../components/ProblemNotice";
import {
  registerMcpServer,
  updateMcpServer,
  type McpServerConfig,
  type McpServerView,
} from "../../api/client";
import styles from "./Settings.module.css";

type Transport = McpServerConfig["transport"];

export interface McpServerFormProps {
  /** The server being edited, or `undefined` to register a new one. */
  server?: McpServerView | undefined;
  /** Called after a successful save. */
  onSaved: () => void;
  onCancel: () => void;
}

export default function McpServerForm(props: McpServerFormProps) {
  const existing = props.server;
  const ids = createUniqueId();
  const [name, setName] = createSignal(existing?.name ?? "");
  const [transport, setTransport] = createSignal<Transport>(
    existing?.config.transport ?? "stdio",
  );
  const [command, setCommand] = createSignal(
    existing?.config.transport === "stdio" ? existing.config.command : "",
  );
  const [args, setArgs] = createSignal(
    existing?.config.transport === "stdio" ? existing.config.args.join(" ") : "",
  );
  const [url, setUrl] = createSignal(
    existing?.config.transport === "http" ? existing.config.url : "",
  );
  const [submitting, setSubmitting] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);

  /**
   * Environment and headers are not editable here, so an edit carries the
   * ones the server already had rather than silently emptying them.
   */
  function config(): McpServerConfig {
    if (transport() === "stdio") {
      return {
        transport: "stdio",
        command: command(),
        args: args()
          .split(/\s+/)
          .filter((token) => token !== ""),
        env: existing?.config.transport === "stdio" ? existing.config.env : [],
      };
    }
    return {
      transport: "http",
      url: url(),
      headers: existing?.config.transport === "http" ? existing.config.headers : [],
    };
  }

  async function onSubmit(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    setSubmitting(true);
    setError(null);
    try {
      const document = { name: name(), enabled: existing?.enabled ?? true, config: config() };
      if (existing === undefined) {
        await registerMcpServer(document);
      } else {
        await updateMcpServer(existing.id, document);
      }
      props.onSaved();
    } catch (err) {
      setError(err);
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <form class={styles.form} onSubmit={(event) => void onSubmit(event)}>
      <div class={styles.fieldRow}>
        <div class={styles.field}>
          <label for={`${ids}-name`}>Name</label>
          <input
            id={`${ids}-name`}
            value={name()}
            onInput={(event) => setName(event.currentTarget.value)}
            placeholder="playwright"
            required
          />
        </div>
        <div class={styles.field}>
          <label for={`${ids}-transport`}>Transport</label>
          <select
            id={`${ids}-transport`}
            value={transport()}
            onChange={(event) => setTransport(event.currentTarget.value as Transport)}
          >
            <option value="stdio">stdio (a process on the machine)</option>
            <option value="http">http (a remote endpoint)</option>
          </select>
        </div>
      </div>

      <Show
        when={transport() === "stdio"}
        fallback={
          <div class={styles.field}>
            <label for={`${ids}-url`}>Endpoint URL</label>
            <input
              id={`${ids}-url`}
              type="url"
              value={url()}
              onInput={(event) => setUrl(event.currentTarget.value)}
              required
            />
          </div>
        }
      >
        <div class={styles.fieldRow}>
          <div class={styles.field}>
            <label for={`${ids}-command`}>Command</label>
            <input
              id={`${ids}-command`}
              value={command()}
              onInput={(event) => setCommand(event.currentTarget.value)}
              placeholder="npx"
              required
            />
          </div>
          <div class={styles.field}>
            <label for={`${ids}-args`}>Arguments</label>
            <input
              id={`${ids}-args`}
              value={args()}
              onInput={(event) => setArgs(event.currentTarget.value)}
              placeholder="-y @playwright/mcp@latest"
            />
          </div>
        </div>
      </Show>

      <ProblemNotice error={error()} />
      <div class={styles.formActions}>
        <button type="submit" class={styles.pillPrimary} disabled={submitting()}>
          {submitting() ? "Saving…" : existing === undefined ? "Add server" : "Save changes"}
        </button>
        <button type="button" class={styles.pill} onClick={props.onCancel} disabled={submitting()}>
          Cancel
        </button>
      </div>
    </form>
  );
}
