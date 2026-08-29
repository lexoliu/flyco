import { For, Show, createResource, createSignal } from "solid-js";
import ProblemNotice from "../../components/ProblemNotice";
import {
  deleteMcpServer,
  listMcpServers,
  registerMcpServer,
  updateMcpServer,
  type McpServerConfig,
  type McpServerView,
} from "../../api/client";
import styles from "./Tab.module.css";

type Transport = McpServerConfig["transport"];

function ServerForm(props: { onSaved: () => void }) {
  const [name, setName] = createSignal("");
  const [transport, setTransport] = createSignal<Transport>("stdio");
  const [command, setCommand] = createSignal("");
  const [args, setArgs] = createSignal("");
  const [url, setUrl] = createSignal("");
  const [submitting, setSubmitting] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);

  async function onSubmit(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    setSubmitting(true);
    setError(null);
    try {
      const config: McpServerConfig =
        transport() === "stdio"
          ? {
              transport: "stdio",
              command: command(),
              args: args()
                .split(/\s+/)
                .filter((token) => token !== ""),
              env: [],
            }
          : { transport: "http", url: url(), headers: [] };
      await registerMcpServer({ name: name(), enabled: true, config });
      setName("");
      setCommand("");
      setArgs("");
      setUrl("");
      props.onSaved();
    } catch (err) {
      setError(err);
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <form class={styles.form} onSubmit={(event) => void onSubmit(event)}>
      <div class={styles.field}>
        <label for="mcp-name">Name</label>
        <input id="mcp-name" value={name()} onInput={(event) => setName(event.currentTarget.value)} required />
      </div>
      <div class={styles.field}>
        <label for="mcp-transport">Transport</label>
        <select
          id="mcp-transport"
          value={transport()}
          onChange={(event) => setTransport(event.currentTarget.value as Transport)}
        >
          <option value="stdio">stdio (local process)</option>
          <option value="http">http (remote)</option>
        </select>
      </div>
      <Show
        when={transport() === "stdio"}
        fallback={
          <div class={styles.field}>
            <label for="mcp-url">Endpoint URL</label>
            <input
              id="mcp-url"
              type="url"
              value={url()}
              onInput={(event) => setUrl(event.currentTarget.value)}
              required
            />
          </div>
        }
      >
        <div class={styles.field}>
          <label for="mcp-command">Command</label>
          <input
            id="mcp-command"
            value={command()}
            onInput={(event) => setCommand(event.currentTarget.value)}
            required
          />
        </div>
        <div class={styles.field}>
          <label for="mcp-args">Arguments (space-separated)</label>
          <input id="mcp-args" value={args()} onInput={(event) => setArgs(event.currentTarget.value)} />
        </div>
      </Show>
      <ProblemNotice error={error()} />
      <button type="submit" class={styles.primaryButton} disabled={submitting()}>
        {submitting() ? "Adding…" : "Add server"}
      </button>
    </form>
  );
}

function describeConfig(config: McpServerConfig): string {
  return config.transport === "stdio" ? `${config.command} ${config.args.join(" ")}`.trim() : config.url;
}

export default function McpServersTab() {
  const [servers, { refetch }] = createResource(listMcpServers);
  const [toggleError, setToggleError] = createSignal<unknown>(null);

  async function onToggle(server: McpServerView): Promise<void> {
    setToggleError(null);
    try {
      await updateMcpServer(server.id, { name: server.name, enabled: !server.enabled, config: server.config });
      await refetch();
    } catch (err) {
      setToggleError(err);
    }
  }

  async function onDelete(id: string): Promise<void> {
    setToggleError(null);
    try {
      await deleteMcpServer(id);
      await refetch();
    } catch (err) {
      setToggleError(err);
    }
  }

  return (
    <div class={styles.tab}>
      <div class={styles.tabHeader}>
        <h2>MCP servers</h2>
        <p class={styles.tabDescription}>
          This is the one place your agents' MCP servers are configured. Agents cannot add or
          edit servers themselves — flyco enforces that with a read-only allowlist on every
          session.
        </p>
      </div>

      <ServerForm onSaved={() => void refetch()} />
      <ProblemNotice error={servers.error ?? toggleError()} />

      <Show when={!servers.loading}>
        <Show when={servers.error !== undefined || (servers() ?? []).length > 0} fallback={<p class={styles.empty}>No MCP servers configured yet.</p>}>
          <ul class={styles.list}>
            <For each={servers()}>
              {(server) => (
                <li class={styles.listItem}>
                  <div>
                    <strong>{server.name}</strong>
                    <p class={styles.itemDetail}>{describeConfig(server.config)}</p>
                  </div>
                  <div class={styles.itemActions}>
                    <button type="button" onClick={() => void onToggle(server)}>
                      {server.enabled ? "Disable" : "Enable"}
                    </button>
                    <button type="button" class={styles.dangerButton} onClick={() => void onDelete(server.id)}>
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
